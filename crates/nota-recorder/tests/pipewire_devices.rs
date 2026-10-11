//! The recorder's device events against real `PipeWire`, in the private
//! instance `scripts/pipewire-devices.sh` starts: a followed default output
//! switched is `Changed` and opens an epoch that starts with the new
//! sink's audio, a pinned sink removed is `Lost` and ends its track, within
//! 2 s each. And the device snapshot, which Setup lists, names the
//! instance's sinks and sources, and its defaults, as they come and go.
//! Skips (and says so) unless the script runs it: switching a default
//! anywhere else would change the user's (AGENTS.md section 8). The script
//! runs the tests one at a time, since each sets the defaults.

// Test code throughout: clippy allows unwraps and panics in it.
#![cfg(test)]

#[cfg(target_os = "linux")]
mod linux {
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::Duration;

    use nota_core::recorder::DeviceChange;
    use nota_core::{
        Clock, Epoch, EpochError, SampleRate, SessionId, SessionTime, SystemClock, TrackId,
    };
    use nota_recorder::capture::{
        CaptureBackend as _, CaptureError, Device, Devices, PipeWireBackend, RecorderEvent, Source,
        prepare_tracks, record_tracks,
    };
    use nota_recorder::fs::StdFs;
    use nota_recorder::segment::SegmentLength;
    use nota_recorder::session::{SessionDir, SessionWriter};

    const MIC: TrackId = TrackId::new(0);
    const SYSTEM: TrackId = TrackId::new(1);
    const PINNED: TrackId = TrackId::new(2);

    /// The time a device change has to show in (development plan, T4).
    const SHOWN_WITHIN: Duration = Duration::from_secs(2); // check-bound

    /// A sample at least this loud is the test's tone on sink B (its peak
    /// is 8 000), never sink A's quiet one or a null sink's silence.
    const TONE_ONSET: u16 = 4_000;

    /// The peaks of the tones played on sinks B and A.
    const LOUD: f64 = 8_000.0;
    const QUIET: f64 = 1_000.0;

    /// How many samples before B's tone reaches [`TONE_ONSET`] it may ring
    /// in by, through the resampler: about 25 measured.
    const ONSET_RING: u64 = 64; // check-bound

    /// The most a switch's epoch may start late by: the new sink's first
    /// buffer is stamped tens of milliseconds before the old sink's audio
    /// ends (40–90 ms measured on `PipeWire` 1.6).
    const MOST_LATE: Duration = Duration::from_millis(250); // check-bound

    /// The longest the test waits for anything before failing.
    const PATIENCE: Duration = Duration::from_secs(10);

    /// The test's two null sinks, by node name, and their descriptions.
    const SINK_A: (&str, &str) = ("nota_test_a", "NotaTestA");
    const SINK_B: (&str, &str) = ("nota_test_b", "NotaTestB");
    /// The test's microphones: sources remapped from sink A's monitor.
    const MIC_1: (&str, &str) = ("nota_test_mic1", "NotaMic1");
    const MIC_2: (&str, &str) = ("nota_test_mic2", "NotaMic2");

    /// The private instance's runtime directory, if the script started one
    /// and this process talks to it. `None` otherwise: the test skips.
    fn private_instance() -> Option<PathBuf> {
        let dir = std::env::var_os("NOTA_PRIVATE_PIPEWIRE").map(PathBuf::from);
        let Some(dir) = dir.filter(|dir| !dir.as_os_str().is_empty()) else {
            let _ = writeln!(
                std::io::stderr(),
                "skipped: run scripts/pipewire-devices.sh, which starts a private PipeWire"
            );
            return None;
        };
        // The guard the script checks too: this process's PipeWire and
        // pactl must be the private instance's, never the user's.
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        assert_eq!(runtime.as_ref(), Some(&dir), "XDG_RUNTIME_DIR");
        let info = pactl(&["info"]);
        assert!(
            info.lines()
                .any(|l| l.starts_with("Server String: ") && l.contains(&*dir.to_string_lossy())),
            "pactl isn't talking to the private instance:\n{info}"
        );
        Some(dir)
    }

    /// Runs `pactl` with `args` and returns what it printed.
    fn pactl(args: &[&str]) -> String {
        let out = Command::new("pactl").args(args).output().unwrap();
        assert!(out.status.success(), "pactl {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    /// The private instance's null sinks and players, removed on drop: the
    /// players by PID, the sinks by module index.
    #[derive(Default)]
    struct Fixtures {
        modules: Vec<String>,
        players: Vec<Child>,
    }

    impl Fixtures {
        /// A null sink named `name`, described as `description`.
        fn sink(&mut self, (name, description): (&str, &str)) -> String {
            let module = pactl(&[
                "load-module",
                "module-null-sink",
                &format!("sink_name={name}"),
                &format!("sink_properties=node.description={description}"),
            ]);
            let module = module.trim().to_owned();
            self.modules.push(module.clone());
            module
        }

        /// A source named `name`, described as `description`, that hears
        /// what plays on the sink `master`.
        fn source(&mut self, (name, description): (&str, &str), master: &str) -> String {
            let module = pactl(&[
                "load-module",
                "module-remap-source",
                &format!("master={master}.monitor"),
                &format!("source_name={name}"),
                &format!("source_properties=node.description={description}"),
            ]);
            let module = module.trim().to_owned();
            self.modules.push(module.clone());
            module
        }

        /// Removes the null sink loaded as `module`.
        fn remove(&mut self, module: &str) {
            pactl(&["unload-module", module]);
            self.modules.retain(|m| m != module);
        }

        /// Plays a tone peaking at `peak` into the sink `name`, for as long
        /// as the test runs.
        fn play_tone(&mut self, name: &str, peak: f64) {
            let mut player = Command::new("pw-play")
                .args(["--target", name, "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut stdin = player.stdin.take().unwrap();
            // Written as it plays; ends when the player is killed.
            thread::spawn(move || {
                let _ = stdin.write_all(&tone_wav(120, peak));
            });
            self.players.push(player);
        }
    }

    impl Drop for Fixtures {
        fn drop(&mut self) {
            for player in &mut self.players {
                let _ = player.kill();
                let _ = player.wait();
            }
            for module in &self.modules {
                let _ = Command::new("pactl")
                    .args(["unload-module", module])
                    .status();
            }
        }
    }

    /// A WAV file of `seconds` of a 440 Hz tone peaking at `peak`, 16 kHz
    /// mono.
    fn tone_wav(seconds: u32, peak: f64) -> Vec<u8> {
        let samples = 16_000 * seconds;
        let mut wav = Vec::new();
        wav.extend(b"RIFF");
        wav.extend((36 + samples * 2).to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16_u32.to_le_bytes());
        wav.extend(1_u16.to_le_bytes());
        wav.extend(1_u16.to_le_bytes());
        wav.extend(16_000_u32.to_le_bytes());
        wav.extend(32_000_u32.to_le_bytes());
        wav.extend(2_u16.to_le_bytes());
        wav.extend(16_u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend((samples * 2).to_le_bytes());
        for i in 0..samples {
            let phase = 2.0 * std::f64::consts::PI * 440.0 * f64::from(i) / 16_000.0;
            #[expect(clippy::cast_possible_truncation, reason = "within i16 at either peak")]
            let value = (phase.sin() * peak) as i16;
            wav.extend(value.to_le_bytes());
        }
        wav
    }

    /// A scratch directory for the session, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        #[expect(
            clippy::disallowed_methods,
            reason = "test scaffolding outside the write path"
        )]
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("nota-pipewire-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        #[expect(
            clippy::disallowed_methods,
            reason = "test scaffolding outside the write path"
        )]
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// What the recorder reported, as it came, with each event's session
    /// time of arrival.
    struct Heard {
        events: mpsc::Receiver<(Option<TrackId>, RecorderEvent)>,
        seen: Vec<(SessionTime, Option<TrackId>, RecorderEvent)>,
        clock: Arc<SystemClock>,
    }

    impl Heard {
        /// Waits for an event on `track` that `wanted` accepts, and says when
        /// it arrived. Fails the test after [`PATIENCE`].
        fn wait_for(
            &mut self,
            track: TrackId,
            wanted: impl Fn(&RecorderEvent) -> bool,
        ) -> SessionTime {
            if let Some((at, ..)) = self
                .seen
                .iter()
                .find(|(_, t, e)| *t == Some(track) && wanted(e))
            {
                return *at;
            }
            // One deadline for the whole wait: other tracks' audio keeps
            // coming, so a wait per event would never end.
            let until = self.clock.now().checked_add(PATIENCE).unwrap();
            loop {
                let left = until
                    .checked_duration_since(self.clock.now())
                    .unwrap_or_else(|| panic!("nothing wanted on track {}", track.get()));
                let (t, event) = self
                    .events
                    .recv_timeout(left)
                    .unwrap_or_else(|_| panic!("nothing wanted on track {}", track.get()));
                let at = self.clock.now();
                let hit = t == Some(track) && wanted(&event);
                self.seen.push((at, t, event));
                if hit {
                    return at;
                }
            }
        }

        /// How many audio events `track` reported that arrived after `at`.
        fn audio_events_after(&self, track: TrackId, at: SessionTime) -> usize {
            self.seen
                .iter()
                .filter(|(seen, t, e)| {
                    *t == Some(track) && *seen > at && matches!(e, RecorderEvent::Audio(_))
                })
                .count()
        }

        /// The first sample from `from` on, on `track`, of the test's tone,
        /// waiting for it to be recorded. Fails the test after
        /// [`PATIENCE`].
        fn wait_for_tone(&mut self, track: TrackId, from: u64) -> u64 {
            let until = self.clock.now().checked_add(PATIENCE).unwrap();
            loop {
                if let Some(at) = self.tone_from(track, from) {
                    return at;
                }
                assert!(self.clock.now() < until, "no tone on track {}", track.get());
                self.listen(Duration::from_millis(100));
            }
        }

        /// Takes in what comes for `long`.
        fn listen(&mut self, long: Duration) {
            let until = self.clock.now().checked_add(long).unwrap();
            while let Some(left) = until.checked_duration_since(self.clock.now()) {
                match self.events.recv_timeout(left) {
                    Ok((t, event)) => self.seen.push((self.clock.now(), t, event)),
                    Err(_) => break,
                }
            }
        }

        /// The epochs `track` opened after `after`, in order, and what
        /// refused any.
        fn reopenings(&self, track: TrackId, after: SessionTime) -> Vec<Result<Epoch, EpochError>> {
            self.seen
                .iter()
                .filter(|(at, t, _)| *t == Some(track) && *at >= after)
                .filter_map(|(_, _, e)| match e {
                    RecorderEvent::Epoch(epoch) => Some(Ok(*epoch)),
                    RecorderEvent::EpochRefused(refused) => Some(Err(*refused)),
                    _ => None,
                })
                .collect()
        }

        /// Where `track`'s audio received before `at` ends.
        fn audio_end_before(&self, track: TrackId, at: SessionTime) -> u64 {
            self.seen
                .iter()
                .filter(|(seen, t, _)| *t == Some(track) && *seen < at)
                .filter_map(|(_, _, e)| match e {
                    RecorderEvent::Audio(chunk) => Some(chunk.range().end().get()),
                    _ => None,
                })
                .max()
                .unwrap()
        }

        /// The first sample from `from` on, on `track`, loud enough to be
        /// the test's tone.
        fn tone_from(&self, track: TrackId, from: u64) -> Option<u64> {
            self.audio_from(track, from, u64::MAX - from)
                .iter()
                .position(|s| s.unsigned_abs() >= TONE_ONSET)
                .map(|at| from + at as u64)
        }

        /// The samples `track` recorded from sample `from`, up to `len` of
        /// them.
        fn audio_from(&self, track: TrackId, from: u64, len: u64) -> Vec<i16> {
            let mut out = Vec::new();
            for (_, t, event) in &self.seen {
                let RecorderEvent::Audio(chunk) = event else {
                    continue;
                };
                if *t != Some(track) {
                    continue;
                }
                let start = chunk.range().start().get();
                for (i, &s) in (start..).zip(chunk.samples()) {
                    if i >= from && i - from < len {
                        out.push(s);
                    }
                }
            }
            out
        }
    }

    /// The switch opened an epoch, late by no more than [`MOST_LATE`], that
    /// starts with B's audio: silence (the new link's first buffer is) up
    /// to B's tone, which follows within 100 ms, never A's quiet tone. A's
    /// last audio stays in the epoch before. The switch opens that one
    /// epoch, though the stream and the watch both report it.
    fn check_reopening(heard: &mut Heard, switched: SessionTime) {
        let epoch = match heard.reopenings(SYSTEM, switched).as_slice() {
            [Ok(epoch)] => *epoch,
            other => panic!("the switch opened not one epoch but {other:?}"),
        };
        assert!(epoch.overrun() <= MOST_LATE, "{:?} late", epoch.overrun());
        let first = epoch.first_sample().get();
        let tone = heard.wait_for_tone(SYSTEM, first);
        assert!(
            tone - first <= 1_600,
            "B's tone began {} samples in",
            tone - first
        );
        // Up to B's tone, only silence, but for the few samples its onset
        // rings in by: no buffer of A's, which holds hundreds of samples.
        let silent = (tone - first).saturating_sub(ONSET_RING);
        let opening = heard.audio_from(SYSTEM, first, silent);
        assert!(
            opening.iter().all(|&s| s == 0),
            "A's tone began the switch's epoch: {opening:?}"
        );
        let before = heard.audio_from(SYSTEM, first - 160, 160);
        assert!(
            before.iter().any(|&s| s != 0) && before.iter().all(|s| s.unsigned_abs() < TONE_ONSET),
            "A's tone didn't end the epoch before: {before:?}"
        );
    }

    /// The only microphone removed: the session manager clears the default
    /// source (there's no stand-in source, as there is a sink). The followed
    /// microphone is lost, but its stream isn't ended: a new microphone
    /// named the default is followed, and recorded from.
    fn follow_through_no_mic(
        heard: &mut Heard,
        fixtures: &mut Fixtures,
        mic: &str,
        clock: &SystemClock,
    ) {
        let gone = clock.now();
        fixtures.remove(mic);
        let lost = heard.wait_for(MIC, |e| is_device(e, &DeviceChange::Lost));
        assert!(lost.checked_duration_since(gone).unwrap() <= SHOWN_WITHIN);
        fixtures.source(MIC_2, SINK_A.0);
        pactl(&["set-default-source", MIC_2.0]);
        let back = heard.wait_for(MIC, |e| {
            is_device(e, &DeviceChange::Changed(MIC_2.1.into()))
        });
        let until = clock.now().checked_add(PATIENCE).unwrap();
        while heard.audio_events_after(MIC, back) == 0 {
            assert!(
                clock.now() < until,
                "nothing recorded from the new microphone"
            );
            heard.listen(Duration::from_millis(100));
        }
        let mic_failed = heard
            .seen
            .iter()
            .any(|(_, t, e)| *t == Some(MIC) && matches!(e, RecorderEvent::CaptureFailed(_)));
        assert!(!mic_failed, "the followed microphone's track was ended");
    }

    /// Whether `event` is the device change `change`.
    fn is_device(event: &RecorderEvent, change: &DeviceChange) -> bool {
        matches!(event, RecorderEvent::Device { change: c, .. } if c == change)
    }

    /// A followed default switched and a pinned sink removed, on the
    /// private instance: each shows within 2 s, the pinned track ends with
    /// nothing recorded after, and the capture moves to the new sink in an
    /// epoch that starts with the new sink's audio, not the old's. With no
    /// microphone left, the followed microphone is lost but follows the
    /// next default.
    #[test]
    fn device_changes_show_on_real_pipewire() {
        let Some(_private) = private_instance() else {
            return;
        };
        let mut fixtures = Fixtures::default();
        fixtures.sink(SINK_A);
        let b = fixtures.sink(SINK_B);
        let mic = fixtures.source(MIC_1, SINK_A.0);
        pactl(&["set-default-sink", SINK_A.0]);
        pactl(&["set-default-source", MIC_1.0]);
        // B plays a loud tone, A a quiet one: which sink a buffer came
        // from shows in the buffer, and silence is neither's. The players
        // stay on their sinks whatever the default, so the tones move only
        // if the capture does.
        fixtures.play_tone(SINK_B.0, LOUD);
        fixtures.play_tone(SINK_A.0, QUIET);

        let scratch = Scratch::new();
        let rate = SampleRate::SPEECH;
        let clock = Arc::new(SystemClock::start().unwrap());
        let session_clock = Arc::clone(&clock) as Arc<dyn Clock>;
        let session = SessionDir::new(SessionId::new(1), StdFs, &scratch.0)
            .lock()
            .unwrap();
        let mut writer = SessionWriter::open(
            &session,
            rate,
            SegmentLength::default_at(rate),
            Arc::clone(&session_clock),
        )
        .unwrap();
        let (starter, receiver) = prepare_tracks(
            &[
                (MIC, Source::Microphone),
                (SYSTEM, Source::SystemAudio),
                (PINNED, Source::Device(SINK_B.0.into())),
            ],
            rate,
            &session_clock,
        );
        let (sender, events) = mpsc::channel();
        let recorder = thread::spawn(move || {
            record_tracks(&mut writer, &mut [], &receiver, &mut |track, event| {
                let _ = sender.send((track, event));
            })
        });
        let streams = starter.start(&PipeWireBackend);
        assert!(streams.iter().all(Result::is_ok), "{streams:?}");
        let mut heard = Heard {
            events,
            seen: Vec::new(),
            clock: Arc::clone(&clock),
        };
        for track in [MIC, SYSTEM, PINNED] {
            heard.wait_for(track, |e| matches!(e, RecorderEvent::Audio(_)));
        }
        heard.listen(Duration::from_secs(1));

        // The followed default switched from A to B.
        let switched = clock.now();
        pactl(&["set-default-sink", SINK_B.0]);
        let changed = heard.wait_for(SYSTEM, |e| {
            is_device(e, &DeviceChange::Changed(SINK_B.1.into()))
        });
        assert!(changed.checked_duration_since(switched).unwrap() <= SHOWN_WITHIN);
        // The stream followed: A's quiet tone, then B's.
        let switch_sample = heard.audio_end_before(SYSTEM, switched);
        let before = heard.audio_from(SYSTEM, switch_sample - 1_600, 1_600);
        assert!(
            before.iter().any(|&s| s != 0) && before.iter().all(|s| s.unsigned_abs() < TONE_ONSET),
            "A's tone before"
        );
        let tone_from = heard.wait_for_tone(SYSTEM, switch_sample);
        assert!(
            tone_from - switch_sample <= 32_000,
            "B's tone began 2 s after"
        );
        heard.listen(Duration::from_millis(500));
        check_reopening(&mut heard, switched);

        // The pinned sink, now also the default, removed.
        let removed = clock.now();
        fixtures.remove(&b);
        let lost = heard.wait_for(PINNED, |e| is_device(e, &DeviceChange::Lost));
        assert!(lost.checked_duration_since(removed).unwrap() <= SHOWN_WITHIN);
        let failed = heard.wait_for(PINNED, |e| {
            matches!(
                e,
                RecorderEvent::CaptureFailed(CaptureError::DeviceNotAvailable(_))
            )
        });
        // The system audio follows the default back to A.
        let back = heard.wait_for(SYSTEM, |e| {
            is_device(e, &DeviceChange::Changed(SINK_A.1.into()))
        });
        assert!(back.checked_duration_since(removed).unwrap() <= SHOWN_WITHIN);

        follow_through_no_mic(&mut heard, &mut fixtures, &mic, &clock);

        // Nothing was recorded on the pinned track after it failed, from
        // whatever device the server moved its stream to.
        let after_failure = heard.audio_events_after(PINNED, failed);
        assert_eq!(after_failure, 0, "audio on the pinned track after its loss");

        drop(streams);
        let outcome = recorder.join().unwrap();
        assert!(outcome.is_ok(), "{outcome:?}");
    }

    /// The snapshot's sinks and sources: two sinks and a source the test
    /// makes, with names of their own.
    const SNAP_SINK_A: (&str, &str) = ("nota_snap_a", "NotaSnapA");
    const SNAP_SINK_B: (&str, &str) = ("nota_snap_b", "NotaSnapB");
    const SNAP_MIC: (&str, &str) = ("nota_snap_mic", "NotaSnapMic");

    /// The device `(name, description)` names.
    fn device((name, description): (&str, &str)) -> Device {
        Device {
            name: name.into(),
            description: description.into(),
        }
    }

    /// Takes snapshots until one satisfies `wanted`, and returns it: the
    /// session manager names a default a moment after `pactl` asks. Fails
    /// the test with the last one after [`PATIENCE`].
    fn snapshot_until(
        clock: &SystemClock,
        what: &str,
        wanted: impl Fn(&Devices) -> bool,
    ) -> Devices {
        let until = clock.now().checked_add(PATIENCE).unwrap();
        loop {
            let devices = PipeWireBackend.devices().unwrap();
            if wanted(&devices) {
                return devices;
            }
            // Each snapshot waits on the server's round trip, which paces
            // the loop.
            assert!(clock.now() < until, "{what}: {devices:#?}");
        }
    }

    /// The device snapshot (`PipeWireBackend::devices`, which Setup lists)
    /// on the private instance: each null sink made is an output and the
    /// source an input, under their descriptions, with the defaults as
    /// named; a node removed is gone from the next snapshot, and so is
    /// the default it was. Only the defaults are waited for: the session
    /// manager names them a moment after `pactl` asks, while `pactl` adds
    /// and removes a node before it returns.
    #[test]
    fn the_snapshot_lists_what_the_instance_has() {
        let Some(_private) = private_instance() else {
            return;
        };
        let clock = SystemClock::start().unwrap();
        let mut fixtures = Fixtures::default();
        fixtures.sink(SNAP_SINK_A);
        let b = fixtures.sink(SNAP_SINK_B);
        let mic = fixtures.source(SNAP_MIC, SNAP_SINK_A.0);
        pactl(&["set-default-sink", SNAP_SINK_B.0]);
        pactl(&["set-default-source", SNAP_MIC.0]);

        let named = snapshot_until(&clock, "the defaults as named", |d| {
            d.default_output.as_deref() == Some(SNAP_SINK_B.0)
                && d.default_input.as_deref() == Some(SNAP_MIC.0)
        });
        for sink in [SNAP_SINK_A, SNAP_SINK_B] {
            assert!(
                named.outputs.contains(&device(sink)),
                "{sink:?}: {named:#?}"
            );
            assert!(
                !named.inputs.contains(&device(sink)),
                "{sink:?}: {named:#?}"
            );
        }
        assert!(named.inputs.contains(&device(SNAP_MIC)), "{named:#?}");
        assert!(!named.outputs.contains(&device(SNAP_MIC)), "{named:#?}");

        // The default sink removed: gone from the next snapshot, and the
        // default, if any, one that's listed.
        fixtures.remove(&b);
        let gone = PipeWireBackend.devices().unwrap();
        assert!(
            !gone.outputs.iter().any(|o| o.name == SNAP_SINK_B.0),
            "{gone:#?}"
        );
        assert!(
            listed_default(&gone.outputs, gone.default_output.as_deref()),
            "{gone:#?}"
        );
        assert!(gone.outputs.contains(&device(SNAP_SINK_A)), "{gone:#?}");
        assert!(gone.inputs.contains(&device(SNAP_MIC)), "{gone:#?}");

        // The default source removed: the same.
        fixtures.remove(&mic);
        let gone = PipeWireBackend.devices().unwrap();
        assert!(
            !gone.inputs.iter().any(|i| i.name == SNAP_MIC.0),
            "{gone:#?}"
        );
        assert!(
            listed_default(&gone.inputs, gone.default_input.as_deref()),
            "{gone:#?}"
        );
        assert!(gone.outputs.contains(&device(SNAP_SINK_A)), "{gone:#?}");
    }

    /// Whether `default` is none, or one of `devices`.
    fn listed_default(devices: &[Device], default: Option<&str>) -> bool {
        default.is_none_or(|name| devices.iter().any(|d| d.name == name))
    }
}
