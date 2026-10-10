//! The recorder's device events against real `PipeWire`, in the private
//! instance `scripts/pipewire-devices.sh` starts: a followed default output
//! switched is `Changed`, a pinned sink removed is `Lost` and ends its
//! track, within 2 s each, and the switch's epoch holds only the new
//! sink's audio. Skips (and says so) unless the script runs it: switching
//! a default anywhere else would change the user's (AGENTS.md section 8).

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
        CaptureError, PipeWireBackend, RecorderEvent, Source, prepare_tracks, record_tracks,
    };
    use nota_recorder::fs::StdFs;
    use nota_recorder::segment::SegmentLength;
    use nota_recorder::session::{SessionDir, SessionWriter};

    const MIC: TrackId = TrackId::new(0);
    const SYSTEM: TrackId = TrackId::new(1);
    const PINNED: TrackId = TrackId::new(2);

    /// The time a device change has to show in (development plan, T4).
    const SHOWN_WITHIN: Duration = Duration::from_secs(2); // check-bound

    /// A sample at least this loud is the test's tone (its peak is 8 000),
    /// never a null sink's silence.
    const TONE_ONSET: u16 = 4_000;

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

        /// Plays a tone into the sink `name`, for as long as the test runs.
        fn play_tone(&mut self, name: &str) {
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
                let _ = stdin.write_all(&tone_wav(120));
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

    /// A WAV file of `seconds` of a 440 Hz tone at -12 dBFS, 16 kHz mono.
    fn tone_wav(seconds: u32) -> Vec<u8> {
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
            #[expect(clippy::cast_possible_truncation, reason = "within i16 at -12 dBFS")]
            let value = (phase.sin() * 8_000.0) as i16;
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

        /// What the first route change `track` reported after `after` did to
        /// its epochs.
        fn reopening(&self, track: TrackId, after: SessionTime) -> Option<Reopening> {
            self.seen
                .iter()
                .filter(|(at, t, _)| *t == Some(track) && *at >= after)
                .find_map(|(_, _, e)| match e {
                    RecorderEvent::Epoch(epoch) => Some(Reopening::Opened(*epoch)),
                    RecorderEvent::EpochRefused(EpochError::ImplausibleOverrun { .. }) => {
                        Some(Reopening::Refused)
                    }
                    _ => None,
                })
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

    /// What a route change did to a track's epochs.
    enum Reopening {
        /// It moved to this epoch.
        Opened(Epoch),
        /// The timeline refused one: the new device's first stamp was
        /// before the end of the audio already placed.
        Refused,
    }

    /// The switch's epoch, if the timeline opened one, starts with B's
    /// tone: no buffer of A's silence took it, which would put a buffer's
    /// worth of zeros first. The timeline may refuse the epoch instead, when
    /// B's first stamp is earlier than the end of A's audio already placed:
    /// the track then carries on in its epoch, with no gap (filed as a
    /// follow-up).
    fn check_reopening(heard: &Heard, switched: SessionTime) {
        match heard.reopening(SYSTEM, switched) {
            Some(Reopening::Opened(epoch)) => {
                let opening = heard.audio_from(SYSTEM, epoch.first_sample().get(), 160);
                assert_eq!(opening.len(), 160, "10 ms recorded in the epoch");
                assert!(
                    opening.iter().any(|&s| s != 0),
                    "A's silence began the switch's epoch"
                );
            }
            Some(Reopening::Refused) => {}
            None => panic!("the switch neither opened nor refused an epoch"),
        }
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
    /// nothing recorded after, and the capture moves to the new sink, its
    /// epoch (if one opens) starting with the new sink's audio, not the
    /// old's. With no microphone left, the followed microphone is lost but
    /// follows the next default.
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
        // B plays a tone; A plays nothing, so its monitor is exact zeros:
        // which sink a buffer came from shows in the buffer. The player
        // stays on B whatever the default, so the tone moves only if the
        // capture does.
        fixtures.play_tone(SINK_B.0);

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
        // The stream followed: A's silence, then B's tone.
        let switch_sample = heard.audio_end_before(SYSTEM, switched);
        let before = heard.audio_from(SYSTEM, switch_sample - 1_600, 1_600);
        assert!(before.iter().all(|&s| s == 0), "A's silence before");
        let tone_from = heard.wait_for_tone(SYSTEM, switch_sample);
        assert!(
            tone_from - switch_sample <= 32_000,
            "B's tone began 2 s after"
        );
        heard.listen(Duration::from_millis(500));
        check_reopening(&heard, switched);

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
}
