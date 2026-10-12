//! A log of how long each text took to reach the screen, for measuring
//! live-text latency (`nota record --latency-log FILE`, built only with the
//! `latency-log` feature), and how far each track's durable position ran
//! behind its stream. `scripts/live-latency.sh` and `scripts/soak.sh` read it.
//!
//! Each text is noted as the live thread hands it to the screen, and the
//! screen's output is watched for the end of each draw ([`Watched`]). A
//! text is drawn by the first draw that starts after it was handed over:
//! after each draw the screen takes every update waiting and then draws
//! again. So the draw after the first draw end that follows the hand-over
//! has it, and the time logged as drawn is never early; it can be late by
//! one draw, if the text arrived before the screen began the draw it ended.
//!
//! Anything that keeps text from the screen is logged too: a transcript that
//! couldn't be placed, audio the engine skipped, the engine going offline, a
//! stream that failed or moved to a new epoch.
//!
//! The log is kept in memory and written once the recording has stopped,
//! so measuring adds no disk work while it runs.
//!
//! It also says how far behind the stream the recording ran: every
//! [`LAG_TICK`] a thread reads each track's [`Progress`], the counters the
//! recorder keeps for any thread to read, and notes the audio delivered and
//! not yet durable (what a crash then would lose) and the audio delivered
//! and not yet in a journal. They are kept as counts and a number for each
//! minute, so the memory grows by a few bytes a minute and not with the
//! samples, and written as `lag` lines (a track's quantiles) and `lagmin`
//! lines (its highest in each minute, `-` for a minute not sampled), which
//! have their own columns, named in the line. It is an amount of audio, not
//! an age: the two are the same while the stream delivers in real time.

use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use nota_core::{Clock, SampleCount, SampleRate, SessionTime, TrackId};
use nota_recorder::capture::{CaptureReceiver, Positions, Progress};

use crate::record::RATE;

/// The log's first line. Times are milliseconds of session time; `-` where
/// a column doesn't apply, or for a text never drawn.
const HEADER: &str = "kind\ttrack\tstart_ms\tend_ms\thanded_ms\tdrawn_ms\n";

/// Something that kept text from the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Problem {
    /// A transcript that couldn't be placed in session time.
    Dropped,
    /// A transcript that came after the screen had closed.
    Late,
    /// Audio the engine never transcribed live.
    Skipped,
    /// The engine went down.
    Offline,
    /// A stream moved to a new epoch after an overrun, or couldn't.
    Epoch,
    /// A stream failed.
    Failed,
}

impl Problem {
    const fn name(self) -> &'static str {
        match self {
            Self::Dropped => "dropped",
            Self::Late => "late",
            Self::Skipped => "skipped",
            Self::Offline => "offline",
            Self::Epoch => "epoch",
            Self::Failed => "failed",
        }
    }
}

/// One line of the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    /// A text handed to the screen.
    Text {
        track: TrackId,
        /// When its chunk's first sample was recorded.
        start: SessionTime,
        /// When the sample after its chunk's last was recorded.
        end: SessionTime,
        /// When it was handed to the screen.
        handed: SessionTime,
    },
    Problem {
        problem: Problem,
        track: Option<TrackId>,
        at: SessionTime,
    },
}

/// The texts and problems so far, to be written to `path`.
#[derive(Debug)]
pub(crate) struct LatencyLog {
    path: PathBuf,
    entries: Vec<Entry>,
}

impl LatencyLog {
    /// An empty log, for `path`.
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            entries: Vec::new(),
        }
    }

    /// Notes that `track`'s text for `start..end` was handed to the screen
    /// at `handed`.
    pub(crate) fn text(
        &mut self,
        track: TrackId,
        start: SessionTime,
        end: SessionTime,
        handed: SessionTime,
    ) {
        self.entries.push(Entry::Text {
            track,
            start,
            end,
            handed,
        });
    }

    /// Notes a problem at `at`.
    pub(crate) fn problem(&mut self, problem: Problem, track: Option<TrackId>, at: SessionTime) {
        self.entries.push(Entry::Problem { problem, track, at });
    }

    /// The log as written, given when each of the screen's draws ended, in
    /// order, and the `lag` lines ([`LagSamples::finish`]): a header, then a
    /// line for each entry in the order noted, then the lag lines.
    pub(crate) fn contents(&self, draw_ends: &[SessionTime], lag: &str) -> String {
        let ms = |t: SessionTime| t.elapsed().as_millis().to_string();
        let track = |t: Option<TrackId>| t.map_or_else(|| "-".to_owned(), |t| t.get().to_string());
        let mut out = HEADER.to_owned();
        for entry in &self.entries {
            let _ = match *entry {
                Entry::Text {
                    track: t,
                    start,
                    end,
                    handed,
                } => writeln!(
                    out,
                    "text\t{}\t{}\t{}\t{}\t{}",
                    t.get(),
                    ms(start),
                    ms(end),
                    ms(handed),
                    drawn(handed, draw_ends).map_or_else(|| "-".to_owned(), ms)
                ),
                Entry::Problem {
                    problem,
                    track: t,
                    at,
                } => writeln!(out, "{}\t{}\t-\t-\t{}\t-", problem.name(), track(t), ms(at)),
            };
        }
        out.push_str(lag);
        out
    }

    /// Writes the log, replacing any file at its path.
    ///
    /// # Errors
    ///
    /// If the file can't be written.
    pub(crate) fn write(&self, draw_ends: &[SessionTime], lag: &str) -> io::Result<()> {
        #[expect(
            clippy::disallowed_methods,
            reason = "a measurement log for tests, not a recording or the database"
        )]
        std::fs::write(&self.path, self.contents(draw_ends, lag))
    }
}

/// When a text handed to the screen at `handed` was drawn: the end of the
/// draw after the first draw to end at or after `handed`.
fn drawn(handed: SessionTime, draw_ends: &[SessionTime]) -> Option<SessionTime> {
    let first = draw_ends.iter().position(|&end| end >= handed)?;
    draw_ends.get(first + 1).copied()
}

/// How often each track's progress is read.
const LAG_TICK: Duration = Duration::from_millis(20);

/// The most lag, in milliseconds, the quantiles tell apart: a sample that
/// found more counts as this there (each minute's highest is kept whole).
const LAG_CAP_MS: usize = 5_000;

/// The minutes whose highest lag is kept, a day's worth; later ones count
/// in the last.
const LAG_MINUTES: usize = 24 * 60;

/// What the samples of one track found.
#[derive(Debug, Default)]
struct TrackLag {
    /// The samples taken since the track first had audio delivered.
    count: u64,
    /// `at_risk[ms]` is how many samples found `ms` milliseconds of audio
    /// delivered and not yet durable (rounded down); the last takes every
    /// sample that found more.
    at_risk: Vec<u64>,
    /// The most of it found in each minute of the recording, `None` for a
    /// minute with no sample.
    minute_max: Vec<Option<u64>>,
    /// The most audio found delivered and not yet in a journal, in
    /// milliseconds: the recorder falling behind its stream.
    queued_max: u64,
}

impl TrackLag {
    /// Notes the track's `positions` at `at`.
    fn note(&mut self, positions: Positions, at: SessionTime) {
        // A track whose stream hasn't started has nothing to be behind on.
        if positions.delivered.get() == 0 {
            return;
        }
        let at_risk = millis(positions.at_risk(), RATE);
        if self.at_risk.is_empty() {
            self.at_risk = vec![0; LAG_CAP_MS + 1];
        }
        if let Some(bucket) = self.at_risk.get_mut(at_risk.min(LAG_CAP_MS)) {
            *bucket += 1;
        }
        let minute = usize::try_from(at.elapsed().as_secs() / 60)
            .unwrap_or(LAG_MINUTES)
            .min(LAG_MINUTES - 1);
        if self.minute_max.len() <= minute {
            self.minute_max.resize(minute + 1, None);
        }
        if let Some(highest) = self.minute_max.get_mut(minute) {
            let found = u64::try_from(at_risk).unwrap_or(u64::MAX);
            *highest = Some((*highest).map_or(found, |h| h.max(found)));
        }
        let queued = u64::try_from(millis(positions.queued(), RATE)).unwrap_or(u64::MAX);
        self.queued_max = self.queued_max.max(queued);
        self.count += 1;
    }

    /// The lag that `percent` of the samples were at or under, by nearest
    /// rank.
    fn percentile(&self, percent: u64) -> usize {
        if self.count == 0 {
            return 0;
        }
        let rank = (self.count * percent).div_ceil(100).max(1);
        let mut seen = 0;
        for (ms, samples) in self.at_risk.iter().enumerate() {
            seen += samples;
            if seen >= rank {
                return ms;
            }
        }
        LAG_CAP_MS
    }

    /// The highest lag found.
    fn highest(&self) -> usize {
        self.at_risk.iter().rposition(|&n| n > 0).unwrap_or(0)
    }

    /// The track's `lag` line, then a `lagmin` line for each minute.
    fn lines(&self, track: TrackId) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "lag\t{}\tsamples={}\tp50_ms={}\tp95_ms={}\tp99_ms={}\tmax_ms={}\tqueued_max_ms={}",
            track.get(),
            self.count,
            self.percentile(50),
            self.percentile(95),
            self.percentile(99),
            self.highest(),
            self.queued_max
        );
        for (minute, highest) in self.minute_max.iter().enumerate() {
            let highest = highest.map_or_else(|| "-".to_owned(), |h| h.to_string());
            let _ = writeln!(out, "lagmin\t{}\t{minute}\t{highest}", track.get());
        }
        out
    }
}

/// `count` samples at `rate`, in whole milliseconds.
fn millis(count: SampleCount, rate: SampleRate) -> usize {
    usize::try_from(count.get().saturating_mul(1000) / u64::from(rate.hz())).unwrap_or(usize::MAX)
}

/// How far each track's durable position ran behind the audio delivered
/// while recording, as a thread samples it ([`Self::start`]) and
/// [`Self::finish`] reports. Dropping it stops the thread.
#[derive(Debug, Default)]
pub(crate) struct LagSamples {
    /// What the samples found, shared with the thread.
    tracks: Arc<Mutex<Vec<(TrackId, TrackLag)>>>,
    /// Dropping it ends the sampling thread, which waits on its other end.
    stop: Option<Sender<()>>,
}

impl LagSamples {
    /// Starts sampling each of `events`' tracks every [`LAG_TICK`], on a
    /// thread that runs until this is finished or dropped.
    ///
    /// # Errors
    ///
    /// If the thread can't start.
    pub(crate) fn start(
        &mut self,
        events: &CaptureReceiver,
        clock: Arc<dyn Clock>,
    ) -> io::Result<()> {
        let progress: Vec<(TrackId, Progress)> = events
            .tracks()
            .into_iter()
            .filter_map(|track| events.progress(track).map(|progress| (track, progress)))
            .collect();
        let (stop, stopped) = mpsc::channel::<()>();
        let tracks = Arc::clone(&self.tracks);
        thread::Builder::new()
            .name("nota-lag".into())
            .spawn(move || {
                // The wait is the tick, and ends for good when the sender
                // is dropped.
                while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(LAG_TICK) {
                    let at = clock.now();
                    for (track, progress) in &progress {
                        note(&tracks, *track, progress.now(), at);
                    }
                }
            })
            .map_err(|e| io::Error::new(e.kind(), format!("starting the lag sampler: {e}")))?;
        self.stop = Some(stop);
        Ok(())
    }

    /// Stops the sampling, and gives the log's `lag` and `lagmin` lines for
    /// each track that had audio, in track order.
    pub(crate) fn finish(self) -> String {
        drop(self.stop);
        let Ok(found) = self.tracks.lock() else {
            return String::new();
        };
        // A track that never had audio has nothing to report.
        let mut tracks: Vec<_> = found.iter().filter(|(_, lag)| lag.count > 0).collect();
        tracks.sort_by_key(|(track, _)| *track);
        tracks
            .iter()
            .map(|(track, found)| found.lines(*track))
            .collect()
    }
}

/// Notes `track`'s `positions` at `at` among `tracks`.
fn note(
    tracks: &Mutex<Vec<(TrackId, TrackLag)>>,
    track: TrackId,
    positions: Positions,
    at: SessionTime,
) {
    let Ok(mut tracks) = tracks.lock() else {
        return;
    };
    if !tracks.iter().any(|(t, _)| *t == track) {
        tracks.push((track, TrackLag::default()));
    }
    if let Some((_, found)) = tracks.iter_mut().find(|(t, _)| *t == track) {
        found.note(positions, at);
    }
}

/// When each of the screen's draws ended, as [`Watched`] saw them.
#[derive(Debug, Clone, Default)]
pub(crate) struct DrawEnds(Arc<Mutex<Vec<SessionTime>>>);

impl DrawEnds {
    /// The draw ends so far, in order.
    pub(crate) fn times(&self) -> Vec<SessionTime> {
        self.0.lock().map(|ends| ends.clone()).unwrap_or_default()
    }

    fn push(&self, at: SessionTime) {
        if let Ok(mut ends) = self.0.lock() {
            ends.push(at);
        }
    }
}

/// The screen's output, watched for the end of each draw: a flush with
/// nothing written since the last one. A draw writes what changed, then
/// hides or moves the cursor and flushes, then flushes once more with
/// nothing new (ratatui's `apply_buffer_with_cursor`); every other flush
/// follows a write.
#[derive(Debug)]
pub(crate) struct Watched<W> {
    inner: W,
    written: bool,
    watch: Option<(DrawEnds, Arc<dyn Clock>)>,
}

impl<W> Watched<W> {
    /// `inner`, with each draw's end noted in `watch`'s list by its clock,
    /// if given.
    pub(crate) fn new(inner: W, watch: Option<(DrawEnds, Arc<dyn Clock>)>) -> Self {
        Self {
            inner,
            written: false,
            watch,
        }
    }
}

impl<W: Write> Write for Watched<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written |= n > 0;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()?;
        if let Some((ends, clock)) = &self.watch
            && !self.written
        {
            ends.push(clock.now());
        }
        self.written = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nota_core::{FakeClock, SampleIndex};

    use super::*;

    fn ms(ms: u64) -> SessionTime {
        SessionTime::from_nanos(ms * 1_000_000)
    }

    #[test]
    fn a_text_is_drawn_by_the_draw_after_the_first_draw_end_after_it() {
        let ends = [ms(100), ms(200), ms(300)];
        // Handed during the draw that ends at 200: that draw may have begun
        // before it arrived, so the next one is the one sure to have it.
        assert_eq!(drawn(ms(150), &ends), Some(ms(300)));
        assert_eq!(drawn(ms(200), &ends), Some(ms(300)));
        assert_eq!(drawn(ms(0), &ends), Some(ms(200)));
        // Never drawn: the screen closed first.
        assert_eq!(drawn(ms(250), &ends), None);
        assert_eq!(drawn(ms(50), &[]), None);
    }

    #[test]
    fn logs_texts_and_problems_in_the_order_noted() {
        let mut log = LatencyLog::new(PathBuf::new());
        log.text(TrackId::new(1), ms(0), ms(3_200), ms(4_950));
        log.problem(Problem::Skipped, Some(TrackId::new(0)), ms(5_000));
        log.text(TrackId::new(0), ms(1_000), ms(4_000), ms(5_100));
        log.problem(Problem::Offline, None, ms(6_000));
        assert_eq!(
            log.contents(&[ms(4_900), ms(5_000), ms(5_150)], ""),
            "kind\ttrack\tstart_ms\tend_ms\thanded_ms\tdrawn_ms\n\
             text\t1\t0\t3200\t4950\t5150\n\
             skipped\t0\t-\t-\t5000\t-\n\
             text\t0\t1000\t4000\t5100\t-\n\
             offline\t-\t-\t-\t6000\t-\n"
        );
    }

    /// The lag lines follow the texts and problems.
    #[test]
    fn the_lag_lines_close_the_log() {
        let mut log = LatencyLog::new(PathBuf::new());
        log.text(TrackId::new(0), ms(0), ms(1_000), ms(1_500));
        assert_eq!(
            log.contents(&[ms(1_400), ms(1_600), ms(1_700)], "lag\t0\tsamples=1\n"),
            "kind\ttrack\tstart_ms\tend_ms\thanded_ms\tdrawn_ms\n\
             text\t0\t0\t1000\t1500\t1700\n\
             lag\t0\tsamples=1\n"
        );
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding outside the recorder's write path"
    )]
    fn writes_the_log_to_its_path() {
        let dir = std::env::temp_dir().join(format!("nota-latency-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("latency.tsv");
        let mut log = LatencyLog::new(path.clone());
        log.text(TrackId::new(0), ms(10), ms(20), ms(1_234));
        log.write(&[ms(1_300), ms(1_400)], "").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "kind\ttrack\tstart_ms\tend_ms\thanded_ms\tdrawn_ms\n\
             text\t0\t10\t20\t1234\t1400\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_draw_ends_at_a_flush_with_nothing_written_since_the_last() {
        let fake = Arc::new(FakeClock::new(ms(0)));
        let clock: Arc<dyn Clock> = Arc::clone(&fake) as Arc<dyn Clock>;
        let ends = DrawEnds::default();
        let mut out = Watched::new(Vec::new(), Some((ends.clone(), clock)));
        // A draw: its changes, the cursor hidden and flushed, then the
        // backend's own flush.
        out.write_all(b"cells").unwrap();
        fake.advance(Duration::from_millis(5));
        out.write_all(b"hide").unwrap();
        out.flush().unwrap();
        fake.advance(Duration::from_millis(1));
        out.flush().unwrap();
        // Another, later.
        fake.advance(Duration::from_millis(250));
        out.write_all(b"cells").unwrap();
        out.flush().unwrap();
        out.flush().unwrap();
        assert_eq!(ends.times(), [ms(6), ms(256)]);
        assert_eq!(out.inner, b"cellshidecells");
    }

    /// A track's positions, in samples at the recording rate (16 kHz, so 16
    /// to the millisecond).
    fn positions(delivered: u64, captured: u64, durable: u64) -> Positions {
        Positions {
            delivered: SampleIndex::new(delivered),
            captured: SampleIndex::new(captured),
            durable: SampleIndex::new(durable),
        }
    }

    fn minutes(minutes: u64) -> SessionTime {
        SessionTime::from_nanos(minutes * 60_000_000_000)
    }

    /// What a crash now would lose is what's delivered and not yet durable,
    /// in whole milliseconds; what waits for a journal is counted apart.
    #[test]
    fn lag_is_audio_delivered_and_not_yet_durable() {
        let mut lag = TrackLag::default();
        // 160 samples = 10 ms behind; durable never passes captured.
        lag.note(positions(1_600, 1_600, 1_440), minutes(0));
        // 800 ms behind, 100 ms of it not yet in a journal.
        lag.note(positions(14_400, 12_800, 1_600), minutes(0));
        // Rounded down: 1 ms and 15 samples is 1 ms.
        lag.note(positions(31, 31, 0), minutes(1));
        assert_eq!(lag.count, 3);
        assert_eq!(lag.at_risk.iter().sum::<u64>(), 3);
        assert_eq!(
            [lag.at_risk[10], lag.at_risk[800], lag.at_risk[1]],
            [1, 1, 1]
        );
        assert_eq!(lag.highest(), 800);
        assert_eq!(lag.queued_max, 100);
        assert_eq!(lag.minute_max, [Some(800), Some(1)]);
    }

    /// A track whose stream hasn't delivered anything has no lag to count.
    #[test]
    fn lag_waits_for_the_first_audio() {
        let mut lag = TrackLag::default();
        lag.note(positions(0, 0, 0), minutes(0));
        assert_eq!(lag.count, 0);
        assert!(lag.at_risk.is_empty());
        assert_eq!((lag.percentile(50), lag.highest()), (0, 0));
    }

    /// The quantiles stop at the cap; the minute's highest keeps the whole.
    #[test]
    fn lag_over_the_cap_counts_as_the_cap() {
        let mut lag = TrackLag::default();
        // Ten seconds behind.
        lag.note(positions(160_000, 160_000, 0), minutes(0));
        assert_eq!(
            (lag.highest(), lag.percentile(50)),
            (LAG_CAP_MS, LAG_CAP_MS)
        );
        assert_eq!(lag.minute_max, [Some(10_000)]);
    }

    /// A hundred samples, one at each millisecond from 1 to 100.
    #[test]
    fn lag_percentiles_are_by_nearest_rank() {
        let mut lag = TrackLag::default();
        // 1 ms, 2 ms, ... 100 ms behind, one sample each.
        for ms in 1..=100 {
            lag.note(positions(16_000, 16_000, 16_000 - ms * 16), minutes(0));
        }
        assert_eq!(lag.count, 100);
        assert_eq!(
            [
                lag.percentile(50),
                lag.percentile(95),
                lag.percentile(99),
                lag.highest()
            ],
            [50, 95, 99, 100]
        );
    }

    /// A minute without a sample is `-`, not a lag of nothing.
    #[test]
    fn lag_lines_name_the_track_and_its_minutes() {
        let mut lag = TrackLag::default();
        lag.note(positions(16_000, 16_000, 8_000), minutes(0));
        lag.note(positions(32_000, 32_000, 31_200), minutes(2));
        assert_eq!(
            lag.lines(TrackId::new(1)),
            "lag\t1\tsamples=2\tp50_ms=50\tp95_ms=500\tp99_ms=500\tmax_ms=500\tqueued_max_ms=0\n\
             lagmin\t1\t0\t500\n\
             lagmin\t1\t1\t-\n\
             lagmin\t1\t2\t50\n"
        );
    }

    /// Finishing, or dropping, the lag samples ends the sampling thread, which
    /// waits on the other end of the channel they hold.
    #[test]
    fn finishing_or_dropping_the_lag_samples_ends_the_thread() {
        for finish in [true, false] {
            let (stop, stopped) = mpsc::channel::<()>();
            let lag = LagSamples {
                stop: Some(stop),
                ..LagSamples::default()
            };
            assert_eq!(
                stopped.recv_timeout(Duration::from_millis(10)),
                Err(RecvTimeoutError::Timeout)
            );
            if finish {
                let _ = lag.finish();
            } else {
                drop(lag);
            }
            assert_eq!(stopped.try_recv(), Err(mpsc::TryRecvError::Disconnected));
        }
    }

    /// Tracks come out in order, and one that never had audio doesn't.
    #[test]
    fn finished_lag_lists_the_tracks_in_order() {
        let lag = LagSamples::default();
        note(
            &lag.tracks,
            TrackId::new(1),
            positions(16_000, 16_000, 15_200),
            minutes(0),
        );
        note(
            &lag.tracks,
            TrackId::new(0),
            positions(16_000, 16_000, 16_000),
            minutes(0),
        );
        note(
            &lag.tracks,
            TrackId::new(1),
            positions(32_000, 32_000, 31_200),
            minutes(0),
        );
        note(&lag.tracks, TrackId::new(2), positions(0, 0, 0), minutes(0));
        let lines = lag.finish();
        let heads: Vec<_> = lines.lines().filter(|l| l.starts_with("lag\t")).collect();
        assert_eq!(
            heads,
            [
                "lag\t0\tsamples=1\tp50_ms=0\tp95_ms=0\tp99_ms=0\tmax_ms=0\tqueued_max_ms=0",
                "lag\t1\tsamples=2\tp50_ms=50\tp95_ms=50\tp99_ms=50\tmax_ms=50\tqueued_max_ms=0",
            ]
        );
        assert_eq!(LagSamples::default().finish(), "");
    }

    #[test]
    fn unwatched_output_notes_nothing() {
        let mut out = Watched::new(Vec::new(), None);
        out.flush().unwrap();
        out.write_all(b"x").unwrap();
        assert_eq!(out.inner, b"x");
    }
}
