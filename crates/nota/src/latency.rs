//! A log of how long each text took to reach the screen, for measuring
//! live-text latency (`nota record --latency-log FILE`, built only with the
//! `latency-log` feature). `scripts/live-latency.sh` reads it.
//!
//! Each text is logged as the live thread hands it to the screen, which
//! draws as soon as it receives it. The log is kept in memory and written
//! once the recording has stopped, so measuring adds no disk work while it
//! runs.

use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;

use nota_core::{SessionTime, TrackId};

/// The log's first line: the columns, all in milliseconds of session time
/// but the track.
const HEADER: &str = "track\tstart_ms\tend_ms\tshown_ms\n";

/// One text on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Text {
    track: TrackId,
    /// When its chunk's first sample was recorded.
    start: SessionTime,
    /// When the sample after its chunk's last was recorded.
    end: SessionTime,
    /// When it was handed to the screen.
    shown: SessionTime,
}

/// The texts shown so far, to be written to `path`.
#[derive(Debug)]
pub(crate) struct LatencyLog {
    path: PathBuf,
    texts: Vec<Text>,
}

impl LatencyLog {
    /// An empty log, for `path`.
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            texts: Vec::new(),
        }
    }

    /// Notes that `track`'s text for `start..end` was handed to the screen
    /// at `shown`.
    pub(crate) fn note(
        &mut self,
        track: TrackId,
        start: SessionTime,
        end: SessionTime,
        shown: SessionTime,
    ) {
        self.texts.push(Text {
            track,
            start,
            end,
            shown,
        });
    }

    /// The log as written: a header, then a line for each text in the
    /// order shown.
    pub(crate) fn contents(&self) -> String {
        let ms = |t: SessionTime| t.elapsed().as_millis();
        let mut out = HEADER.to_owned();
        for s in &self.texts {
            let _ = writeln!(
                out,
                "{}\t{}\t{}\t{}",
                s.track.get(),
                ms(s.start),
                ms(s.end),
                ms(s.shown)
            );
        }
        out
    }

    /// Writes the log, replacing any file at its path.
    ///
    /// # Errors
    ///
    /// If the file can't be written.
    pub(crate) fn write(&self) -> io::Result<()> {
        #[expect(
            clippy::disallowed_methods,
            reason = "a measurement log for tests, not a recording or the database"
        )]
        std::fs::write(&self.path, self.contents())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(ms: u64) -> SessionTime {
        SessionTime::from_nanos(ms * 1_000_000)
    }

    #[test]
    fn logs_each_text_in_the_order_shown() {
        let mut log = LatencyLog::new(PathBuf::new());
        log.note(TrackId::new(1), ms(0), ms(3_200), ms(4_950));
        log.note(TrackId::new(0), ms(1_000), ms(4_000), ms(5_100));
        assert_eq!(
            log.contents(),
            "track\tstart_ms\tend_ms\tshown_ms\n1\t0\t3200\t4950\n0\t1000\t4000\t5100\n"
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
        log.note(TrackId::new(0), ms(10), ms(20), ms(1_234));
        log.write().unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "track\tstart_ms\tend_ms\tshown_ms\n0\t10\t20\t1234\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
