//! A log of how long each text took to reach the screen, for measuring
//! live-text latency (`nota record --latency-log FILE`, built only with the
//! `latency-log` feature). `scripts/live-latency.sh` reads it.
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

use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use nota_core::{Clock, SessionTime, TrackId};

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
    /// order: a header, then a line for each entry in the order noted.
    pub(crate) fn contents(&self, draw_ends: &[SessionTime]) -> String {
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
        out
    }

    /// Writes the log, replacing any file at its path.
    ///
    /// # Errors
    ///
    /// If the file can't be written.
    pub(crate) fn write(&self, draw_ends: &[SessionTime]) -> io::Result<()> {
        #[expect(
            clippy::disallowed_methods,
            reason = "a measurement log for tests, not a recording or the database"
        )]
        std::fs::write(&self.path, self.contents(draw_ends))
    }
}

/// When a text handed to the screen at `handed` was drawn: the end of the
/// draw after the first draw to end at or after `handed`.
fn drawn(handed: SessionTime, draw_ends: &[SessionTime]) -> Option<SessionTime> {
    let first = draw_ends.iter().position(|&end| end >= handed)?;
    draw_ends.get(first + 1).copied()
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

    use nota_core::FakeClock;

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
            log.contents(&[ms(4_900), ms(5_000), ms(5_150)]),
            "kind\ttrack\tstart_ms\tend_ms\thanded_ms\tdrawn_ms\n\
             text\t1\t0\t3200\t4950\t5150\n\
             skipped\t0\t-\t-\t5000\t-\n\
             text\t0\t1000\t4000\t5100\t-\n\
             offline\t-\t-\t-\t6000\t-\n"
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
        log.write(&[ms(1_300), ms(1_400)]).unwrap();
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

    #[test]
    fn unwatched_output_notes_nothing() {
        let mut out = Watched::new(Vec::new(), None);
        out.flush().unwrap();
        out.write_all(b"x").unwrap();
        assert_eq!(out.inner, b"x");
    }
}
