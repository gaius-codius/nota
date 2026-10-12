//! The `first-epoch` mode of `real_capture` (GAI-477): a track's first
//! epoch, measured against the stamps `PipeWire` gave its first buffers.
//!
//! The recorder holds a stream's first buffers until a buffer's delay is
//! at least its length, and dates them back from that buffer's stamp
//! (`capture/hold.rs`). Nothing past the sender shows whether it did: the
//! first epoch's start is all the recorder keeps, and the buffers that
//! settled are sent by their own stamps, so a first epoch a quantum late
//! looks the same as a right one. So this mode captures through a backend
//! of its own, [`StampBackend`], that opens the same cpal stream as
//! `PipeWireBackend` and hands each buffer to the same
//! `CaptureSender::audio_delivered`, but first writes down its length,
//! cycle and capture stamp. The hold, the epoch and the recorder are the
//! real ones; what this backend leaves out is `PipeWireBackend`'s
//! real-time priority, its device watch, the mapping of the stream's
//! errors, and the guard that sends the buffers still held when the stream
//! drops (that call is private to the recorder). So `write` and the crash
//! runs stay on `PipeWireBackend`, and a run that ends within the hold's
//! 500 ms of a move or a resume doesn't show the epoch that opens for it.
//!
//! The check: the first buffer that settled (its delay at least its length,
//! within a millisecond, as the hold judges) puts the stream's first sample
//! at its stamp less the samples before it, and the track's first epoch
//! must start there, within a sample. A run with the hold off opens it at
//! the first buffer's cycle less its length instead, a quantum later. A
//! buffer that settles only after the hold has given up (500 ms or 64
//! buffers) is reported as that, not as a hold that failed: the first epoch
//! was dated by the zero-delay rule, as the recorder does for a stream
//! whose delay is slow to arrive, and it fails the check all the same.
//!
//! Each buffer's stamps are placed on the session clock as the buffer
//! arrives, as the sender places them: a suspend later must not move
//! them.
//!
//! The mode prints each track's first buffers and then its whole delay
//! profile, as runs of buffers with the same length and delay, so the
//! profile of a default move or a resume can be read off the same run.

use std::collections::BTreeMap;
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, DeviceId, ErrorKind, HostId, StreamConfig};
use nota_core::{Clock, SampleCount, SampleRate, SessionTime, SystemClock, TrackId};
use nota_recorder::capture::{
    CaptureBackend, CaptureError, CaptureNotice, CaptureReceiver, CaptureSender, Delivery,
    RecordError, RecorderEvent, Source, prepare_tracks, record_tracks,
};
use nota_recorder::fs::{Fs, FsFile, StdFs};
use nota_recorder::segment::SegmentLength;
use nota_recorder::session::{SessionDir, SessionLock, SessionWriter, Syncing};

use super::{Args, RATE, Res, SESSION, ms, wait};

/// How far short of its buffer's length a delay can be and still be
/// settled: the same judgment as the hold's `SETTLED_WITHIN`.
const SETTLED_WITHIN: Duration = Duration::from_millis(1); // check-bound

/// How long, and how many buffers, the hold waits for a settled stamp: the
/// same limits as the hold's `HOLD_LIMIT` and `HOLD_BUFFERS`.
const HOLD_LIMIT: Duration = Duration::from_millis(500); // check-bound
const HOLD_BUFFERS: usize = 64; // check-bound

/// The rounding of a buffer's length and of the clock readings to the
/// nanosecond, allowed on top of a sample in the first epoch's start.
const ROUNDING_NANOS: u64 = 1_000; // check-bound

/// How far from the first settled buffer's start the first epoch may open:
/// one sample, and the rounding.
fn epoch_within_nanos() -> u64 {
    1_000_000_000 / u64::from(RATE.hz()) + ROUNDING_NANOS
}

/// The most buffers noted for each track: about 21 minutes at the
/// graph's usual quantum.
const PROFILE_CAP: usize = 60_000;

/// How many of a track's first buffers are printed one by one.
const FIRST_PRINTED: usize = 16;

/// A buffer as the stream delivered it, before the sender held it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Raw {
    /// Its length, in samples.
    len: usize,
    /// Its cycle's time and its capture stamp, on `CLOCK_MONOTONIC`; `None`
    /// if cpal couldn't stamp it.
    stamp: Option<Delivery>,
    /// The same stamps on the session clock's elapsed time, placed as the
    /// buffer arrived; `None` if it wasn't stamped or the clock can't place
    /// them (they come before it started).
    placed: Option<Delivery>,
}

/// One track's buffers, as the stream's callback notes them.
type Buffers = Arc<Mutex<Vec<Raw>>>;

/// Captures through cpal's `PipeWire` host as `PipeWireBackend` does, and
/// notes each buffer's length and stamps before it goes to the sender.
#[derive(Debug)]
struct StampBackend {
    /// The session clock, to place each buffer's stamps as it arrives.
    clock: Arc<dyn Clock>,
    /// The buffers noted for each stream started, in the order they were
    /// started: the order the tracks were asked for, since the starter
    /// opens them one after another.
    taps: Mutex<Vec<Buffers>>,
}

impl StampBackend {
    /// A backend that places stamps on `clock`.
    fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            taps: Mutex::default(),
        }
    }

    /// The buffers noted for each stream, in the order they started.
    fn taps(&self) -> Vec<Buffers> {
        self.taps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The cpal node a source is captured from, as `PipeWireBackend` names it.
fn node(source: &Source) -> &str {
    match source {
        Source::SystemAudio => "sink_default",
        Source::Microphone => "input_default",
        Source::Device(name) => name,
    }
}

impl CaptureBackend for StampBackend {
    type Stream = cpal::Stream;

    fn start(
        &self,
        source: &Source,
        rate: SampleRate,
        events: CaptureSender,
    ) -> Result<cpal::Stream, CaptureError> {
        let host = cpal::host_from_id(HostId::PipeWire)
            .map_err(|e| CaptureError::HostUnavailable(e.to_string()))?;
        let device = host
            .device_by_id(&DeviceId::new(HostId::PipeWire, node(source)))
            .ok_or_else(|| CaptureError::DeviceNotAvailable(source.clone()))?;
        let config = StreamConfig {
            channels: 1,
            sample_rate: rate.hz(),
            buffer_size: BufferSize::Default,
        };
        let tap: Buffers = Arc::new(Mutex::new(Vec::with_capacity(PROFILE_CAP)));
        self.taps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::clone(&tap));
        let errors = events.clone();
        let clock = Arc::clone(&self.clock);
        let backend = |e: cpal::Error| CaptureError::Backend(format!("{source}: {e}"));
        let stream = device
            .build_input_stream::<i16, _, _>(
                config,
                move |samples: &[i16], info: &cpal::InputCallbackInfo| {
                    let delivery = delivery_of(info);
                    note(&tap, &*clock, samples.len(), delivery);
                    events.audio_delivered(samples, delivery);
                },
                move |error: cpal::Error| match error.kind() {
                    ErrorKind::Xrun => errors.notice(CaptureNotice::Overrun),
                    ErrorKind::DeviceChanged => errors.notice(CaptureNotice::RouteChanged),
                    _ => errors.notice(CaptureNotice::Warning(error.to_string())),
                },
                None,
            )
            .map_err(backend)?;
        stream.play().map_err(backend)?;
        Ok(stream)
    }
}

/// The stamps cpal gave a buffer, on `CLOCK_MONOTONIC`; `None` if it gave
/// none.
fn delivery_of(info: &cpal::InputCallbackInfo) -> Option<Delivery> {
    let stamp = info.timestamp();
    let monotonic = |instant: cpal::StreamInstant| {
        u64::try_from(instant.as_nanos())
            .ok()
            .map(Duration::from_nanos)
    };
    monotonic(stamp.callback)
        .zip(monotonic(stamp.capture))
        .map(|(cycle, capture)| Delivery { cycle, capture })
}

/// Notes a buffer of `len` samples stamped `stamp`, if there's room, with
/// the stamps placed on `clock` now.
fn note(tap: &Buffers, clock: &dyn Clock, len: usize, stamp: Option<Delivery>) {
    if len == 0 {
        return;
    }
    let place = |awake| clock.awake_to_session(awake).map(SessionTime::elapsed);
    let placed = stamp.and_then(|stamp| {
        Some(Delivery {
            cycle: place(stamp.cycle)?,
            capture: place(stamp.capture)?,
        })
    });
    let mut seen = tap.lock().unwrap_or_else(PoisonError::into_inner);
    if seen.len() < PROFILE_CAP {
        seen.push(Raw { len, stamp, placed });
    }
}

/// How long `len` samples last at `rate`.
fn length(len: usize, rate: SampleRate) -> Option<Duration> {
    SampleCount::new(u64::try_from(len).ok()?).duration_at(rate)
}

/// The first buffer whose stamp says when its audio began, and when the
/// stream's first sample was captured by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Settled {
    /// Its place among the buffers.
    index: usize,
    /// The stream's first sample, on the session clock's elapsed time: the
    /// buffer's capture stamp, placed as it arrived, less the samples before
    /// it. `None` if the clock couldn't place the stamp, or it comes before
    /// the clock started.
    start: Option<Duration>,
    /// Whether the hold was still waiting when this buffer came, so it
    /// dated the buffers before it from this one's stamp. If not, it had
    /// given up, and sent them by their own.
    in_hold: bool,
}

/// The first of `buffers` whose delay is at least its length (within
/// [`SETTLED_WITHIN`]). `None` if none is, or an unstamped buffer comes
/// first.
fn settled(buffers: &[Raw], rate: SampleRate) -> Option<Settled> {
    let limit = SampleCount::started_within(HOLD_LIMIT, rate)
        .and_then(|count| usize::try_from(count.get()).ok())
        .unwrap_or(0);
    let mut before = 0_usize;
    for (index, raw) in buffers.iter().enumerate() {
        let stamp = raw.stamp?;
        let delay = stamp.cycle.saturating_sub(stamp.capture);
        if !delay.is_zero() && delay + SETTLED_WITHIN >= length(raw.len, rate)? {
            let start = raw
                .placed
                .and_then(|placed| placed.capture.checked_sub(length(before, rate)?));
            return Some(Settled {
                index,
                start,
                in_hold: before <= limit && index <= HOLD_BUFFERS,
            });
        }
        before += raw.len;
    }
    None
}

/// What the first epoch's start says against the settled stamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The track never had a first epoch.
    NoEpoch,
    /// No buffer settled, so there's nothing to hold the epoch to.
    Unsettled,
    /// A buffer settled, but its stamp can't be placed on the session
    /// clock, or comes before the clock started: nothing to hold the epoch
    /// to.
    Unplaced,
    /// The epoch opened `off_nanos` after (negative: before) the stream's
    /// first sample, as the buffer at `index` settled it.
    Measured {
        /// The settled buffer's place among the buffers.
        index: usize,
        /// How much later the epoch started than the settled stamps say.
        off_nanos: i128,
        /// Whether the hold was still waiting when that buffer came. If not,
        /// it had given up and dated the first buffers by their own stamps.
        in_hold: bool,
    },
}

impl Outcome {
    /// Whether the epoch is where the settled stamps put it, within a
    /// sample. An epoch the hold gave up on is never where they put it.
    fn holds(self) -> bool {
        matches!(self, Self::Measured { off_nanos, in_hold: true, .. }
            if off_nanos.unsigned_abs() <= u128::from(epoch_within_nanos()))
    }
}

/// The first sample as the first settled buffer puts it: its place among
/// the buffers, how far into the session if the clock can place it, and
/// whether the hold was still waiting when it came.
type Expected = (usize, Option<u64>, bool);

/// The epoch starting `epoch` nanoseconds into the session, against the
/// stream's first sample as `expected` puts it, if a buffer settled.
fn outcome(epoch: Option<u64>, expected: Option<Expected>) -> Outcome {
    match (epoch, expected) {
        (None, _) => Outcome::NoEpoch,
        (_, None) => Outcome::Unsettled,
        (_, Some((_, None, _))) => Outcome::Unplaced,
        (Some(epoch), Some((index, Some(at), in_hold))) => Outcome::Measured {
            index,
            off_nanos: i128::from(epoch) - i128::from(at),
            in_hold,
        },
    }
}

/// A run of buffers with the same length and about the same delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    /// The first buffer's place among the buffers.
    from: usize,
    /// How many buffers.
    count: usize,
    /// Each one's length, in samples.
    len: usize,
    /// Each one's delay; `None` for buffers cpal couldn't stamp.
    delay: Option<Duration>,
    /// When the run began, by its first cycle's time on the session clock
    /// after the stream's first; zero if the clock couldn't place it. A
    /// suspend shows here as a gap, as the stamps stop during it.
    at: Duration,
}

/// The first buffer's cycle on the session clock's elapsed time, if the
/// clock could place it.
fn first_placed_cycle(buffers: &[Raw]) -> Option<Duration> {
    buffers.first()?.placed.map(|placed| placed.cycle)
}

/// `buffers` as runs: a buffer joins the run before it if it's as long
/// (within a sample: a quantum resampled comes as 341 or 342 samples) and its
/// delay is within [`SETTLED_WITHIN`] of the run's first.
fn runs(buffers: &[Raw]) -> Vec<Run> {
    let first_cycle = first_placed_cycle(buffers).unwrap_or(Duration::ZERO);
    let mut runs: Vec<Run> = Vec::new();
    for (from, raw) in buffers.iter().enumerate() {
        let delay = raw
            .stamp
            .map(|stamp| stamp.cycle.saturating_sub(stamp.capture));
        let joins = |run: &Run| {
            run.len.abs_diff(raw.len) <= 1
                && match (run.delay, delay) {
                    (Some(a), Some(b)) => a.abs_diff(b) <= SETTLED_WITHIN,
                    (None, None) => true,
                    _ => false,
                }
        };
        if let Some(run) = runs.last_mut().filter(|run| joins(run)) {
            run.count += 1;
        } else {
            let at = raw.placed.map_or(Duration::ZERO, |placed| {
                placed.cycle.saturating_sub(first_cycle)
            });
            runs.push(Run {
                from,
                count: 1,
                len: raw.len,
                delay,
                at,
            });
        }
    }
    runs
}

/// How long `len` samples last at the harness's rate, in milliseconds.
fn len_ms(len: usize) -> f64 {
    ms(u64::try_from(len).unwrap_or(u64::MAX))
}

/// A duration in milliseconds, for printing.
fn millis(d: Duration) -> f64 {
    d.as_secs_f64() * 1_000.0
}

/// Prints `track`'s first buffers one by one, then its delay profile, with
/// where its first cycle sits on the session clock, which the epochs are on.
fn print_profile(out: &mut impl io::Write, track: TrackId, buffers: &[Raw]) -> io::Result<()> {
    let t = track.get();
    if let Some(at) = first_placed_cycle(buffers) {
        writeln!(out, "first-cycle {t} session {:.3}", millis(at))?;
    }
    print_first_buffers(out, t, buffers)?;
    for run in runs(buffers) {
        let delay = run
            .delay
            .map_or_else(|| "unstamped".to_owned(), |d| format!("{:.3}", millis(d)));
        writeln!(
            out,
            "profile {t} from {} x{} len {:.3} delay {delay} at {:.3}",
            run.from,
            run.count,
            len_ms(run.len),
            millis(run.at)
        )?;
    }
    Ok(())
}

/// Prints the first [`FIRST_PRINTED`] of `buffers`, with their stamps
/// after the first cycle's.
fn print_first_buffers(out: &mut impl io::Write, t: u32, buffers: &[Raw]) -> io::Result<()> {
    let first_cycle = buffers
        .iter()
        .find_map(|raw| raw.stamp)
        .map_or(Duration::ZERO, |stamp| stamp.cycle);
    for (i, raw) in buffers.iter().take(FIRST_PRINTED).enumerate() {
        let len = len_ms(raw.len);
        match raw.stamp {
            Some(s) => writeln!(
                out,
                "buffer {t} {i} len {len:.3} cycle {:.3} capture {:.3} delay {:.3}",
                millis(s.cycle.saturating_sub(first_cycle)),
                millis(s.capture.saturating_sub(first_cycle)),
                millis(s.cycle.saturating_sub(s.capture)),
            )?,
            None => writeln!(out, "buffer {t} {i} len {len:.3} unstamped")?,
        }
    }
    Ok(())
}

/// What the recorder noted of a track, in order, after its stream started:
/// every epoch, and what the stream reported. A default move or a resume
/// shows here as a notice and the epoch that follows it.
#[derive(Debug, Clone, PartialEq)]
enum Noted {
    /// The track moved to an epoch, in which `first_sample` plays from
    /// `start`.
    Epoch {
        /// The track.
        track: TrackId,
        /// The epoch's first sample.
        first_sample: u64,
        /// Where the epoch starts on the session clock, in nanoseconds.
        start: u64,
    },
    /// The stream noted something and recording went on.
    Notice {
        /// The track.
        track: TrackId,
        /// What it noted.
        notice: CaptureNotice,
    },
}

/// What the recorder thread hands back: the writer, what it noted, and how
/// recording ended.
type Recorded = (SessionWriter<StdFs>, Vec<Noted>, Result<(), RecordError>);

/// Records until every capture stops, noting each epoch and notice.
fn record(mut writer: SessionWriter<StdFs>, events: &CaptureReceiver) -> Recorded {
    let mut noted = Vec::new();
    let result = record_tracks(&mut writer, &mut [], events, &mut |track, event| {
        let Some(track) = track else {
            return;
        };
        match event {
            RecorderEvent::Epoch(epoch) => noted.push(Noted::Epoch {
                track,
                first_sample: epoch.first_sample().get(),
                start: epoch.start().as_nanos(),
            }),
            RecorderEvent::Capture(notice) => noted.push(Noted::Notice { track, notice }),
            _ => {}
        }
    });
    (writer, noted, result)
}

/// Each track's first epoch's start, in nanoseconds on the session clock.
fn first_starts(noted: &[Noted]) -> BTreeMap<TrackId, u64> {
    let mut starts = BTreeMap::new();
    for item in noted {
        if let Noted::Epoch { track, start, .. } = item {
            starts.entry(*track).or_insert(*start);
        }
    }
    starts
}

/// A writer for a session in `dir`, on `clock`, with 10 s segments, and the
/// session's lock, which is held until it drops.
fn open_writer(
    dir: &Path,
    clock: &Arc<dyn Clock>,
) -> Res<(SessionLock<StdFs>, SessionWriter<StdFs>)> {
    let session_path = dir.join("session");
    StdFs.create_dir(&session_path)?;
    let session = SessionDir::new(SESSION, StdFs, &session_path).lock()?;
    let length =
        SegmentLength::new(SampleCount::new(10 * u64::from(RATE.hz()))).ok_or("bad length")?;
    let writer =
        SessionWriter::open(&session, RATE, length, Arc::clone(clock))?.with_syncing(Syncing::Auto);
    Ok((session, writer))
}

/// Captures `--tracks` streams of `--source` for `--seconds`, and checks
/// each track's first epoch against the stamps its first buffers carried.
/// `<dir>` is the session's scratch directory. With `--ready PATH`, creates
/// `PATH` once the streams are running, so a script can change the graph at
/// that point. Returns whether every track's first epoch holds.
pub(super) fn command(args: &Args) -> Res<bool> {
    let dir = args.path(0, "<dir>")?;
    let source = Source::Device(args.flag("source")?.to_owned());
    let seconds = args.number("seconds")?;
    let count = u32::try_from(args.optional("tracks")?.unwrap_or(1))?;
    if count == 0 || seconds < 2 {
        return Err("--tracks must be at least 1 and --seconds at least 2".into());
    }
    let tracks: Vec<TrackId> = (0..count).map(TrackId::new).collect();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::start().map_err(|_| "no monotonic clock")?);
    // The first stamps come a delay before the stream's first cycle; the
    // clock can place them only if it started before that.
    wait(Duration::from_secs(1));
    let (_session, writer) = open_writer(&dir, &clock)?;
    let sources: Vec<(TrackId, Source)> = tracks.iter().map(|&t| (t, source.clone())).collect();
    let (starter, events) = prepare_tracks(&sources, RATE, &clock);
    let recorder = thread::spawn(move || record(writer, &events));
    let backend = StampBackend::new(Arc::clone(&clock));
    let captures: Result<Vec<_>, CaptureError> = starter.start(&backend).into_iter().collect();
    let captures = match captures {
        Ok(captures) => captures,
        Err(e) => {
            let _ = recorder.join();
            return Err(e.into());
        }
    };
    if let Some(ready) = args.flags.get("ready") {
        // The streams are running: the script may now change the graph.
        let mut marker = StdFs.create(Path::new(ready))?;
        marker.sync()?;
    }
    wait(Duration::from_secs(seconds));
    drop(captures);
    let (writer, noted, result) = recorder
        .join()
        .map_err(|_| "the recorder thread panicked")?;
    // The report comes first, so a probe that ends in an error still shows
    // what it saw; the recording's own error comes before the writer's.
    let reported = report(&tracks, &backend.taps(), &noted);
    let finished = writer.finish();
    result?;
    finished?;
    reported
}

/// Prints each track's profile and its first epoch's outcome, and the
/// result line. `true` if every track's first epoch holds.
fn report(tracks: &[TrackId], taps: &[Buffers], noted: &[Noted]) -> Res<bool> {
    let mut out = io::stdout();
    let starts = first_starts(noted);
    let mut ok = true;
    for (&track, tap) in tracks.iter().zip(taps) {
        let buffers = tap.lock().unwrap_or_else(PoisonError::into_inner).clone();
        print_profile(&mut out, track, &buffers)?;
        let expected = settled(&buffers, RATE).map(|s| {
            let at = s
                .start
                .and_then(|start| u64::try_from(start.as_nanos()).ok());
            (s.index, at, s.in_hold)
        });
        let found = outcome(starts.get(&track).copied(), expected);
        ok &= found.holds();
        writeln!(out, "track {} {}", track.get(), describe(found))?;
    }
    print_noted(&mut out, noted)?;
    writeln!(
        out,
        "result {} first epochs against the settled stamps",
        if ok { "ok" } else { "FAIL" }
    )?;
    Ok(ok)
}

/// Prints what the recorder noted, in order, on the session clock.
fn print_noted(out: &mut impl io::Write, noted: &[Noted]) -> io::Result<()> {
    for item in noted {
        match item {
            Noted::Epoch {
                track,
                first_sample,
                start,
            } => writeln!(
                out,
                "epoch {} sample {first_sample} at {:.3}",
                track.get(),
                millis(Duration::from_nanos(*start))
            )?,
            Noted::Notice { track, notice } => {
                writeln!(out, "notice {} {notice:?}", track.get())?;
            }
        }
    }
    Ok(())
}

/// `found` in words, for the track's line.
fn describe(found: Outcome) -> String {
    match found {
        Outcome::NoEpoch => "FAIL no first epoch".to_owned(),
        Outcome::Unsettled => "FAIL no buffer settled: its delay never reached its length, \
             or a buffer came unstamped"
            .to_owned(),
        Outcome::Unplaced => "FAIL the settled buffer's stamp can't be placed on the session \
             clock, or comes before it started"
            .to_owned(),
        Outcome::Measured {
            index,
            off_nanos,
            in_hold,
        } => {
            // A nanosecond difference this size is exact in an f64.
            #[expect(
                clippy::cast_precision_loss,
                reason = "an epoch's offset is far below 2^52 nanoseconds"
            )]
            let off = off_nanos as f64 / 1e6;
            let gave_up = if in_hold {
                ""
            } else {
                ": the hold gave up (500 ms, 64 buffers) before this buffer settled, so \
                 the epoch is dated by the zero-delay rule"
            };
            format!(
                "{} first epoch starts {off:+.3} ms from the first sample the settled \
                 buffer ({index}) puts{gave_up}",
                if found.holds() { "ok" } else { "FAIL" }
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 683 samples at 16 kHz: a graph quantum of 42.667 ms, resampled.
    const QUANTUM: usize = 683;

    /// A buffer of `len` samples whose cycle is `cycle_ms` after a second in,
    /// and whose capture is `delay` before it. Its placement on the session
    /// clock is the same times.
    fn raw(len: usize, cycle_ms: u64, delay: Duration) -> Raw {
        let cycle = Duration::from_millis(1_000 + cycle_ms);
        let stamp = Delivery {
            cycle,
            capture: cycle.saturating_sub(delay),
        };
        Raw {
            len,
            stamp: Some(stamp),
            placed: Some(stamp),
        }
    }

    /// A stream as a real capture's: the first six buffers carry no delay,
    /// and from the seventh on the delay is a whole buffer.
    fn real_profile() -> Vec<Raw> {
        let quantum = length(QUANTUM, RATE).unwrap_or_default();
        (0..12_u64)
            .map(|i| {
                raw(
                    QUANTUM,
                    i * 43,
                    if i < 6 { Duration::ZERO } else { quantum },
                )
            })
            .collect()
    }

    /// A stream of 341-sample buffers (21.3 ms) with no delay until the
    /// buffer at `index`, and a delay of two buffers from it.
    fn settling_at(index: u64) -> Vec<Raw> {
        let len = 341;
        let delay = length(2 * len, RATE).unwrap_or_default();
        (0..=index)
            .map(|i| raw(len, i * 21, if i < index { Duration::ZERO } else { delay }))
            .collect()
    }

    /// The first settled buffer is the first whose delay is a whole
    /// buffer, and the stream's first sample is that buffer's stamp less
    /// the samples before it.
    #[test]
    fn the_first_settled_buffer_dates_the_first_sample() {
        let buffers = real_profile();
        let found = settled(&buffers, RATE).expect("a buffer settles");
        assert_eq!(found.index, 6);
        let capture = buffers[6].placed.expect("placed").capture;
        let before = length(6 * QUANTUM, RATE).expect("a length");
        assert_eq!(found.start, Some(capture.saturating_sub(before)));
        assert!(found.in_hold);
    }

    /// The first sample is dated from the stamp placed as the buffer
    /// arrived, not the monotonic stamp: a suspend after it moves the
    /// clock's reading of that stamp, and must not move this.
    #[test]
    fn the_first_sample_is_dated_from_the_placement_made_on_arrival() {
        let mut buffers = real_profile();
        let early = buffers[6].placed.expect("placed");
        let shift = Duration::from_secs(10);
        buffers[6].placed = Some(Delivery {
            cycle: early.cycle + shift,
            capture: early.capture + shift,
        });
        let before = length(6 * QUANTUM, RATE).expect("a length");
        let found = settled(&buffers, RATE).expect("a buffer settles");
        assert_eq!(
            found.start,
            Some((early.capture + shift).saturating_sub(before))
        );
    }

    /// A settled buffer the clock couldn't place has no start to hold the
    /// epoch to.
    #[test]
    fn a_settled_buffer_the_clock_could_not_place_has_no_start() {
        let mut buffers = real_profile();
        buffers[6].placed = None;
        let found = settled(&buffers, RATE).expect("a buffer settles");
        assert_eq!((found.index, found.start), (6, None));
    }

    /// A delay a hair short of its buffer's, as the quantum resampled
    /// leaves it, still settles; one 2 ms short doesn't.
    #[test]
    fn a_delay_within_a_millisecond_of_its_buffer_settles() {
        let quantum = length(QUANTUM, RATE).expect("a length");
        let at = |short: Duration| {
            let buffers = [raw(QUANTUM, 0, quantum.saturating_sub(short))];
            settled(&buffers, RATE).map(|s| s.index)
        };
        assert_eq!(at(Duration::from_micros(900)), Some(0));
        assert_eq!(at(Duration::from_millis(2)), None);
    }

    /// A stream whose delay never reaches its buffers' length, or whose
    /// first buffer is unstamped, has no settled buffer.
    #[test]
    fn a_stream_that_never_settles_has_no_settled_buffer() {
        let never: Vec<Raw> = (0..10)
            .map(|i| raw(QUANTUM, i * 43, Duration::from_millis(5)))
            .collect();
        assert_eq!(settled(&never, RATE), None);
        let unstamped = [Raw {
            len: QUANTUM,
            stamp: None,
            placed: None,
        }];
        assert_eq!(settled(&unstamped, RATE), None);
    }

    /// A buffer that settles while the hold is still waiting (within
    /// 500 ms of audio) is in the hold; one that settles after is not,
    /// as the hold sent what it held by then.
    #[test]
    fn a_buffer_that_settles_after_the_hold_gave_up_is_not_in_the_hold() {
        // 20 buffers of 341 samples are 426 ms; 30 are 639 ms.
        assert!(settled(&settling_at(20), RATE).expect("settles").in_hold);
        let late = settled(&settling_at(30), RATE).expect("settles");
        assert_eq!(late.index, 30);
        assert!(!late.in_hold);
    }

    /// An epoch where the settled stamps put it holds, and one a sample
    /// early or late still does.
    #[test]
    fn an_epoch_within_a_sample_of_the_settled_start_holds() {
        let at = 5_000_000_000;
        let sample = 62_500;
        assert!(outcome(Some(at), Some((6, Some(at), true))).holds());
        assert!(outcome(Some(at + sample), Some((6, Some(at), true))).holds());
        assert!(outcome(Some(at - sample), Some((6, Some(at), true))).holds());
    }

    /// An epoch a quantum after the settled start, as taking the first
    /// buffer to end at its cycle puts it, doesn't hold, and says by how
    /// much.
    #[test]
    fn an_epoch_a_quantum_late_fails() {
        let at = 5_000_000_000;
        let quantum = 42_688_000;
        let late = outcome(Some(at + quantum), Some((6, Some(at), true)));
        assert_eq!(
            late,
            Outcome::Measured {
                index: 6,
                off_nanos: i128::from(quantum),
                in_hold: true
            }
        );
        assert!(!late.holds());
        assert!(!outcome(Some(at - quantum), Some((6, Some(at), true))).holds());
    }

    /// An epoch right on a buffer that settled after the hold gave up still
    /// fails: the hold dated the first buffers by their own stamps.
    #[test]
    fn an_epoch_on_a_buffer_that_settled_after_the_hold_gave_up_fails() {
        let found = outcome(Some(5_000), Some((30, Some(5_000), false)));
        assert!(!found.holds());
        assert!(describe(found).contains("the hold gave up"));
    }

    /// A track with no epoch, or no settled buffer, can't hold.
    #[test]
    fn a_track_without_an_epoch_or_a_settled_buffer_fails() {
        assert_eq!(outcome(None, Some((6, Some(1), true))), Outcome::NoEpoch);
        assert_eq!(outcome(None, None), Outcome::NoEpoch);
        assert_eq!(outcome(Some(1), None), Outcome::Unsettled);
        assert_eq!(outcome(Some(1), Some((6, None, true))), Outcome::Unplaced);
        assert!(!Outcome::Unplaced.holds());
        assert!(!Outcome::NoEpoch.holds());
        assert!(!Outcome::Unsettled.holds());
    }

    /// A track's first epoch is the first one noted for it, whatever
    /// notices and later epochs follow.
    #[test]
    fn the_first_epoch_is_the_first_one_noted_for_the_track() {
        let epoch = |track, first_sample, start| Noted::Epoch {
            track: TrackId::new(track),
            first_sample,
            start,
        };
        let noted = [
            Noted::Notice {
                track: TrackId::new(0),
                notice: CaptureNotice::RouteChanged,
            },
            epoch(1, 0, 40),
            epoch(0, 0, 30),
            epoch(0, 900, 5_000),
        ];
        assert_eq!(
            first_starts(&noted),
            BTreeMap::from([(TrackId::new(0), 30), (TrackId::new(1), 40)])
        );
    }

    /// The profile joins buffers of the same length and delay, and a
    /// change of either starts a run.
    #[test]
    fn the_profile_is_runs_of_the_same_length_and_delay() {
        let quantum = length(QUANTUM, RATE).expect("a length");
        let found = runs(&real_profile());
        assert_eq!(found.len(), 2);
        assert_eq!(
            (found[0].from, found[0].count, found[0].delay),
            (0, 6, Some(Duration::ZERO))
        );
        assert_eq!(
            (found[1].from, found[1].count, found[1].delay),
            (6, 6, Some(quantum))
        );
        // A buffer a sample longer, as a resampled quantum comes, joins the
        // run; a move that resets the delay, and a shorter buffer, each
        // begin one.
        let mut buffers = real_profile();
        buffers.push(raw(QUANTUM + 1, 600, quantum));
        buffers.push(raw(QUANTUM, 643, Duration::ZERO));
        buffers.push(raw(256, 686, Duration::ZERO));
        let found = runs(&buffers);
        assert_eq!(
            found.iter().map(|r| (r.from, r.count)).collect::<Vec<_>>(),
            [(0, 6), (6, 7), (13, 1), (14, 1)]
        );
    }
}
