//! Captures one track through `PipeWire` into journals for a fixed time,
//! and checks that the sample count matches the wall clock.
//!
//! `capture_wall_time <dir> <seconds> [system | mic | device <node>]`
//!
//! Records at 16 kHz into `<dir>`, which must exist and hold no journals,
//! for `<seconds>` of session time. Then it reads the journals back and
//! prints the samples captured, the wall time, the difference and the peak
//! level, with any overruns the stream reported. It fails if the difference
//! is more than 1000 ppm of the wall time plus 100 ms (the stream's start
//! and its last buffer), if the journals don't hold every sample, or if the
//! audio is silent (peak at or below -70 dBFS): it measures playing audio.
//!
//! `scripts/capture-wall-time.sh` runs it against a temporary null sink
//! with a tone playing, so the user's own devices are left alone.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
mod linux {
    use std::error::Error;
    use std::io::{self, Write as _};
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use nota_core::{
        Clock, EpochId, SampleCount, SampleIndex, SampleRate, SessionId, SystemClock, TrackTimeline,
    };
    use nota_recorder::capture::{
        CaptureNotice, PipeWireBackend, RecorderEvent, Source, record_track, start,
    };
    use nota_recorder::fs::{Fs, StdFs};
    use nota_recorder::journal::read_journal;
    use nota_recorder::segment::SegmentLength;
    use nota_recorder::session::{SessionDir, SessionWriter};

    type Res<T> = Result<T, Box<dyn Error>>;

    const TRACK: nota_core::TrackId = nota_core::TrackId::new(0);
    /// The drift allowed, in parts per million of the wall time.
    const PPM: u128 = 1_000;
    /// Allowed on top: the stream's start-up and its last buffer.
    const SLACK: Duration = Duration::from_millis(100);
    /// The quietest peak that counts as audio playing: -70 dBFS.
    const MIN_PEAK: i16 = 10;

    pub(super) fn main() -> ExitCode {
        match run() {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::FAILURE,
            Err(e) => {
                let _ = writeln!(io::stderr(), "capture_wall_time: {e}");
                ExitCode::from(2)
            }
        }
    }

    fn parse() -> Res<(PathBuf, u64, Source)> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let usage = "usage: capture_wall_time <dir> <seconds> [system | mic | device <node>]";
        let (dir, seconds) = match args.as_slice() {
            [dir, seconds, ..] => (PathBuf::from(dir), seconds.parse()?),
            _ => return Err(usage.into()),
        };
        let source = match args.get(2..).unwrap_or_default() {
            [] => Source::SystemAudio,
            [s] if s == "system" => Source::SystemAudio,
            [s] if s == "mic" => Source::Microphone,
            [s, node] if s == "device" => Source::Device(node.clone()),
            _ => return Err(usage.into()),
        };
        Ok((dir, seconds, source))
    }

    fn run() -> Res<bool> {
        let (dir, seconds, source) = parse()?;
        let rate = SampleRate::SPEECH;
        let clock = Arc::new(SystemClock::start().map_err(|_| "no monotonic clock")?);
        let session = SessionDir::new(SessionId::new(1), StdFs, &dir).lock()?;
        let mut writer = SessionWriter::open(
            &session,
            rate,
            SegmentLength::default_at(rate),
            Arc::clone(&clock) as Arc<dyn Clock>,
        )?;
        writer.start_track(TRACK, EpochId::new(0), SampleIndex::ZERO)?;

        let mut timeline = TrackTimeline::new(TRACK);
        timeline.open_epoch(clock.now(), SampleIndex::ZERO, rate)?;
        let (capture, events) = start(
            &PipeWireBackend,
            &source,
            rate,
            Arc::clone(&clock) as Arc<dyn Clock>,
        )?;
        let started = clock.now();
        let recorder = thread::spawn(move || {
            let mut journals = Vec::new();
            let mut notices = Vec::new();
            let mut failures = 0_usize;
            let result = record_track(&mut writer, &mut timeline, &events, &mut |e| match e {
                RecorderEvent::Finished(j) => journals.extend(j),
                RecorderEvent::JournalFailed(_) | RecorderEvent::EpochRefused(_) => {
                    failures += 1;
                }
                RecorderEvent::Capture(n) => notices.push(n),
                RecorderEvent::Epoch(_) => {}
            });
            (writer, journals, notices, failures, result)
        });
        wait(Duration::from_secs(seconds));
        let stopped = clock.now();
        drop(capture);
        let (writer, mut journals, notices, failures, result) = recorder
            .join()
            .map_err(|_| "the recorder thread panicked")?;
        result?;
        let captured = writer
            .next_sample(TRACK)
            .ok_or("the track wasn't started")?;
        journals.extend(writer.finish()?);

        let wall = stopped
            .checked_duration_since(started)
            .ok_or("the clock went back")?;
        let (journaled, peak) = read_back(session.session(), &journals)?;
        report(&Report {
            source: &source,
            captured,
            journaled,
            rate,
            wall,
            peak,
            notices: &notices,
            failures,
        })
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the measurement runs for a fixed wall time while the recorder thread records"
    )]
    fn wait(duration: Duration) {
        thread::sleep(duration);
    }

    /// The samples in `journals`, in order and without gaps, and their peak.
    fn read_back(
        session: &SessionDir<StdFs>,
        journals: &[nota_recorder::session::FinishedJournal],
    ) -> Res<(u64, i16)> {
        let (mut next, mut peak) = (0_u64, 0_i16);
        for journal in journals {
            let path: &Path = session.dir();
            let bytes = session.fs().read(&path.join(journal.id().file_name()))?;
            let Some((range, audio)) = read_journal(&bytes).audio() else {
                continue;
            };
            if range.start().get() != next {
                return Err(format!(
                    "journal {} starts at {}, not {next}",
                    journal.id().get(),
                    range.start().get()
                )
                .into());
            }
            next = range.end().get();
            peak = audio
                .iter()
                .map(|s| s.saturating_abs())
                .fold(peak, i16::max);
        }
        Ok((next, peak))
    }

    struct Report<'a> {
        source: &'a Source,
        captured: SampleIndex,
        journaled: u64,
        rate: SampleRate,
        wall: Duration,
        peak: i16,
        notices: &'a [CaptureNotice],
        failures: usize,
    }

    fn report(r: &Report<'_>) -> Res<bool> {
        let audio = SampleCount::new(r.captured.get())
            .duration_at(r.rate)
            .ok_or("too many samples")?;
        let diff = audio.abs_diff(r.wall);
        let ppm = diff.as_nanos() * 1_000_000 / r.wall.as_nanos().max(1);
        let allowed = SLACK
            + Duration::from_nanos(
                u64::try_from(r.wall.as_nanos() * PPM / 1_000_000).unwrap_or(u64::MAX),
            );
        let overruns = r
            .notices
            .iter()
            .filter(|n| **n == CaptureNotice::Overrun)
            .count();
        let peak_dbfs = 20.0 * (f64::from(r.peak.max(1)) / 32_768.0).log10();
        let ok = diff <= allowed
            && r.journaled == r.captured.get()
            && r.failures == 0
            && r.peak > MIN_PEAK;
        let mut out = io::stdout().lock();
        writeln!(out, "source        {}", r.source)?;
        writeln!(
            out,
            "samples       {} at {} Hz ({:.3} s)",
            r.captured.get(),
            r.rate.hz(),
            audio.as_secs_f64()
        )?;
        writeln!(out, "wall time     {:.3} s", r.wall.as_secs_f64())?;
        writeln!(
            out,
            "difference    {:.1} ms ({ppm} ppm); allowed {:.1} ms",
            diff.as_secs_f64() * 1e3,
            allowed.as_secs_f64() * 1e3
        )?;
        writeln!(out, "journaled     {} samples", r.journaled)?;
        writeln!(out, "peak          {peak_dbfs:.1} dBFS")?;
        writeln!(
            out,
            "overruns      {overruns}; other notices {}",
            r.notices.len() - overruns
        )?;
        writeln!(out, "journal fails {}", r.failures)?;
        writeln!(out, "{}", if ok { "PASS" } else { "FAIL" })?;
        Ok(ok)
    }
}
