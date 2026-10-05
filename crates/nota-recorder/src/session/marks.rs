//! The session's marks: the ids its recordings have used, kept durably in
//! the session directory so a resumed session never uses one again.
//!
//! - **Journal ids.** Every journal id ever used is below
//!   [`Marks::journals_below`]. The writer reserves ids in blocks, and makes
//!   each reservation durable before a journal takes an id from it, so a
//!   journal deleted by publishing can't have its id reused by a later
//!   writer: a stale [`FinishedJournal`](super::FinishedJournal) never names
//!   a newer journal.
//! - **Epoch ids,** per track. Epoch ids come from an in-memory timeline,
//!   numbered from zero, so a resumed session would reuse them, and two
//!   recordings would share an epoch. The highest epoch each track has
//!   journaled is kept here, durable before the first journal in it.
//!
//! The file is replaced whole: written to a temp file, fsync'd, renamed over
//! the old one, and its directory fsync'd. A crash leaves the old marks or
//! the new ones, never a mix. It's plain text:
//!
//! ```text
//! nota session marks 1
//! journals-below 64
//! epoch 0 3
//! ```

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::io;
use std::path::Path;

use nota_core::{EpochId, TrackId};

use crate::fs::{Fs, FsFile};
use crate::journal::JournalId;

/// The marks file's name in the session directory.
pub const FILE_NAME: &str = "session-marks";
/// Where a new version is written before it replaces the file.
const TEMP_NAME: &str = "session-marks.tmp";
/// The first line, naming the format and its version.
const MAGIC: &str = "nota session marks 1";

/// Whether `path` is the marks' temp file: never the only copy of the
/// marks, so salvage removes one a crash left.
pub(crate) fn is_temp(path: &Path) -> bool {
    path.file_name() == Some(TEMP_NAME.as_ref())
}

/// What a session's recordings have used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Marks {
    /// Every journal id used so far is below this one.
    pub(crate) journals_below: JournalId,
    /// The highest epoch each track has journaled.
    pub(crate) epochs: BTreeMap<TrackId, EpochId>,
}

impl Default for Marks {
    fn default() -> Self {
        Self {
            journals_below: JournalId::FIRST,
            epochs: BTreeMap::new(),
        }
    }
}

/// The marks file is there but isn't marks: corruption, since it's only
/// ever replaced whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadMarks;

impl fmt::Display for BadMarks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the session's marks file is damaged")
    }
}

impl std::error::Error for BadMarks {}

impl Marks {
    /// The file's contents.
    fn encode(&self) -> String {
        let mut out = format!("{MAGIC}\njournals-below {}\n", self.journals_below.get());
        for (track, epoch) in &self.epochs {
            // Writing to a String can't fail.
            let _ = writeln!(out, "epoch {} {}", track.get(), epoch.get());
        }
        out
    }

    /// Parses the file's contents, accepting exactly what [`Self::encode`]
    /// makes.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, BadMarks> {
        let text = std::str::from_utf8(bytes).map_err(|_| BadMarks)?;
        let body = text.strip_suffix('\n').ok_or(BadMarks)?;
        let mut lines = body.split('\n');
        if lines.next() != Some(MAGIC) {
            return Err(BadMarks);
        }
        let below = lines
            .next()
            .and_then(|l| l.strip_prefix("journals-below "))
            .and_then(number::<u64>)
            .ok_or(BadMarks)?;
        let mut epochs = BTreeMap::new();
        for line in lines {
            let mut words = line.strip_prefix("epoch ").ok_or(BadMarks)?.split(' ');
            let (Some(track), Some(epoch), None) = (words.next(), words.next(), words.next())
            else {
                return Err(BadMarks);
            };
            let track = TrackId::new(number(track).ok_or(BadMarks)?);
            let epoch = EpochId::new(number(epoch).ok_or(BadMarks)?);
            // Tracks in order, each once, as written.
            if epochs
                .last_key_value()
                .is_some_and(|(&last, _)| last >= track)
            {
                return Err(BadMarks);
            }
            epochs.insert(track, epoch);
        }
        Ok(Self {
            journals_below: JournalId::new(below),
            epochs,
        })
    }

    /// The session's marks, or none yet if there's no file. A writer reads
    /// them only when its listing shows the file, so a session without
    /// one costs no failed read.
    ///
    /// # Errors
    ///
    /// Any I/O error but a missing file; [`io::ErrorKind::InvalidData`],
    /// wrapping [`BadMarks`], if the file is damaged.
    pub(crate) fn read<S: Fs>(fs: &S, dir: &Path) -> io::Result<Self> {
        match fs.read(&dir.join(FILE_NAME)) {
            Ok(bytes) => {
                Self::decode(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Replaces the session's marks with these, durably.
    ///
    /// # Errors
    ///
    /// Any I/O error. The file then holds the old marks or these; which is
    /// known only after a later write succeeds.
    pub(crate) fn write<S: Fs>(&self, fs: &S, dir: &Path) -> io::Result<()> {
        let temp = dir.join(TEMP_NAME);
        let mut file = match fs.create(&temp) {
            Ok(file) => file,
            // Left by a crash or a failed write: never the only copy.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                fs.remove(&temp)?;
                fs.create(&temp)?
            }
            Err(e) => return Err(e),
        };
        file.write_all(self.encode().as_bytes())?;
        file.sync()?;
        fs.rename(&temp, &dir.join(FILE_NAME))?;
        fs.sync_dir(dir)
    }
}

/// A decimal number in its one canonical spelling: no sign, no leading
/// zeros.
fn number<N: std::str::FromStr + ToString>(text: &str) -> Option<N> {
    let n: N = text.parse().ok()?;
    (n.to_string() == text).then_some(n)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use proptest::prelude::*;

    use super::*;
    use crate::fs::fake::{CrashOutcome, FakeFs};

    fn dir() -> PathBuf {
        PathBuf::from("/session")
    }

    fn marks(below: u64, epochs: &[(u32, u32)]) -> Marks {
        Marks {
            journals_below: JournalId::new(below),
            epochs: epochs
                .iter()
                .map(|&(t, e)| (TrackId::new(t), EpochId::new(e)))
                .collect(),
        }
    }

    #[test]
    fn the_format_is_plain_text() {
        assert_eq!(
            marks(64, &[(0, 3), (1, 0)]).encode(),
            "nota session marks 1\njournals-below 64\nepoch 0 3\nepoch 1 0\n"
        );
    }

    #[test]
    fn no_file_is_no_marks() {
        let fs = FakeFs::with_dirs([dir()]);
        assert_eq!(Marks::read(&fs, &dir()).unwrap(), Marks::default());
    }

    #[test]
    fn written_marks_read_back_and_replace_the_old() {
        let fs = FakeFs::with_dirs([dir()]);
        marks(64, &[(0, 1)]).write(&fs, &dir()).unwrap();
        marks(128, &[(0, 2), (1, 0)]).write(&fs, &dir()).unwrap();
        assert_eq!(
            Marks::read(&fs, &dir()).unwrap(),
            marks(128, &[(0, 2), (1, 0)])
        );
        assert_eq!(fs.paths(), [dir().join(FILE_NAME)]);
    }

    #[test]
    fn a_leftover_temp_file_is_replaced() {
        let fs = FakeFs::with_dirs([dir()]);
        let mut stale = fs.create(&dir().join(TEMP_NAME)).unwrap();
        stale.write_all(b"half").unwrap();
        marks(64, &[]).write(&fs, &dir()).unwrap();
        assert_eq!(Marks::read(&fs, &dir()).unwrap(), marks(64, &[]));
    }

    #[test]
    fn damage_is_an_error_not_no_marks() {
        let fs = FakeFs::with_dirs([dir()]);
        let mut file = fs.create(&dir().join(FILE_NAME)).unwrap();
        file.write_all(b"nota session marks 1\njournals-below x\n")
            .unwrap();
        let err = Marks::read(&fs, &dir()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_marks_file_that_cant_be_read_is_an_error_not_no_marks() {
        let fs = FakeFs::with_dirs([dir(), dir().join(FILE_NAME)]);
        assert_eq!(
            Marks::read(&fs, &dir()).unwrap_err().kind(),
            io::ErrorKind::IsADirectory
        );
        assert!(!BadMarks.to_string().is_empty());
    }

    #[test]
    fn a_failed_create_is_reported_as_it_failed() {
        let fs = FakeFs::with_dirs([dir()]);
        fs.fail_after(0, io::ErrorKind::StorageFull);
        assert_eq!(
            marks(64, &[]).write(&fs, &dir()).unwrap_err().kind(),
            io::ErrorKind::StorageFull
        );
        assert!(fs.paths().is_empty());
    }

    #[test]
    fn a_crash_at_every_step_leaves_the_old_marks_or_the_new() {
        let old = marks(64, &[(0, 1)]);
        let new = marks(128, &[(0, 2)]);
        let total = {
            let fs = FakeFs::with_dirs([dir()]);
            old.write(&fs, &dir()).unwrap();
            let before = fs.attempted();
            new.write(&fs, &dir()).unwrap();
            fs.attempted() - before
        };
        for n in 0..=total {
            for outcome in CrashOutcome::standard() {
                let fs = FakeFs::with_dirs([dir()]);
                old.write(&fs, &dir()).unwrap();
                fs.crash_after(n);
                let done = new.write(&fs, &dir()).is_ok();
                let got = Marks::read(&fs.crash(outcome), &dir()).unwrap();
                if done {
                    assert_eq!(got, new, "{n} {outcome:?}");
                } else {
                    assert!(got == old || got == new, "{n} {outcome:?}: {got:?}");
                }
            }
        }
    }

    #[test]
    fn non_canonical_spellings_are_refused() {
        for text in [
            "",
            "nota session marks 1\njournals-below 64",
            "nota session marks 2\njournals-below 64\n",
            "nota session marks 1\njournals-below 064\n",
            "nota session marks 1\njournals-below +64\n",
            "nota session marks 1\njournals-below 64\nepoch 0 1 2\n",
            "nota session marks 1\njournals-below 64\nepoch 1 0\nepoch 0 0\n",
            "nota session marks 1\njournals-below 64\nepoch 0 0\nepoch 0 1\n",
            "nota session marks 1\njournals-below 64\n\n",
        ] {
            assert_eq!(Marks::decode(text.as_bytes()), Err(BadMarks), "{text:?}");
        }
    }

    proptest! {
        #[test]
        fn encode_then_decode_is_the_identity(
            below in any::<u64>(),
            epochs in proptest::collection::btree_map(any::<u32>(), any::<u32>(), 0..8),
        ) {
            let m = Marks {
                journals_below: JournalId::new(below),
                epochs: epochs.into_iter().map(|(t, e)| (TrackId::new(t), EpochId::new(e))).collect(),
            };
            prop_assert_eq!(Marks::decode(m.encode().as_bytes()), Ok(m));
        }

        #[test]
        fn decoding_any_bytes_never_panics_and_round_trips_what_it_accepts(
            bytes in proptest::collection::vec(any::<u8>(), 0..96),
        ) {
            if let Ok(m) = Marks::decode(&bytes) {
                prop_assert_eq!(m.encode().into_bytes(), bytes);
            }
        }
    }
}
