//! The session's marks: the ids its recordings have used, kept durably in
//! the session directory so a resumed session never uses one again.
//!
//! - **Journal ids.** Every journal id ever used is below
//!   [`Marks::journals_below`]. The writer reserves ids in blocks, and makes
//!   each reservation durable before a journal takes an id from it, so a
//!   journal deleted by publishing can't have its id reused by a later
//!   writer: a stale [`FinishedJournal`](super::FinishedJournal) never names
//!   a newer journal.
//! - **Epochs,** per track. Epoch ids come from an in-memory timeline, so a
//!   resumed session must start each track's timeline above the epochs it
//!   used, or two recordings would share an epoch. The highest epoch each
//!   track has journaled is kept here, with its anchor (its first sample,
//!   rate and start), durable before the first journal in it. A resumed
//!   session times its tracks on from that anchor, and resumes its clock
//!   after the audio it times.
//!
//! The file is replaced whole: written to a temp file, fsync'd, renamed over
//! the old one, and its directory fsync'd. A crash leaves the old marks or
//! the new ones, never a mix. It's plain text; an epoch line gives the
//! track, the epoch, then its first sample, rate in hertz and start in
//! session-time nanoseconds:
//!
//! ```text
//! nota session marks 2
//! journals-below 64
//! epoch 0 3 48000 16000 3000000000
//! ```
//!
//! Version 1 files, written before epochs were timed, have only the track
//! and epoch on each line. They're still read, and an epoch from one stays
//! untimed until the track opens a newer one.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::io;
use std::path::Path;

use nota_core::{EpochAnchor, EpochId, SampleIndex, SampleRate, SessionTime, TrackId};

use crate::fs::{Fs, FsFile};
use crate::journal::JournalId;

/// The marks file's name in the session directory.
pub const FILE_NAME: &str = "session-marks";
/// Where a new version is written before it replaces the file.
const TEMP_NAME: &str = "session-marks.tmp";
/// The first line, naming the format and its version.
const MAGIC: &str = "nota session marks 2";
/// The first line of the older format, whose epochs aren't timed.
const UNTIMED_MAGIC: &str = "nota session marks 1";

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
    pub(crate) epochs: BTreeMap<TrackId, MarkedEpoch>,
}

/// The highest epoch a track has journaled, as the marks keep it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkedEpoch {
    /// Marked with its anchor.
    Timed(EpochAnchor),
    /// Marked by a version 1 file: only its number is known.
    Untimed(EpochId),
}

impl MarkedEpoch {
    /// The epoch's number.
    pub(crate) const fn id(self) -> EpochId {
        match self {
            Self::Timed(anchor) => anchor.id,
            Self::Untimed(id) => id,
        }
    }

    /// The epoch's anchor, if it was marked with one.
    pub(crate) const fn anchor(self) -> Option<EpochAnchor> {
        match self {
            Self::Timed(anchor) => Some(anchor),
            Self::Untimed(_) => None,
        }
    }

    /// Whether this epoch, not `known`, is the track's highest: it's
    /// numbered above it, or it's the same epoch with the anchor `known`
    /// lacks. An epoch's anchor never changes, so one already timed stays.
    pub(crate) fn supersedes(self, known: Option<Self>) -> bool {
        known.is_none_or(|known| {
            self.id() > known.id()
                || (self.id() == known.id() && known.anchor().is_none() && self.anchor().is_some())
        })
    }
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
            let _ = match epoch {
                MarkedEpoch::Timed(a) => writeln!(
                    out,
                    "epoch {} {} {} {} {}",
                    track.get(),
                    a.id.get(),
                    a.first_sample.get(),
                    a.rate.hz(),
                    a.start.as_nanos()
                ),
                MarkedEpoch::Untimed(id) => writeln!(out, "epoch {} {}", track.get(), id.get()),
            };
        }
        out
    }

    /// Parses the file's contents, accepting exactly what [`Self::encode`]
    /// makes.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, BadMarks> {
        let text = std::str::from_utf8(bytes).map_err(|_| BadMarks)?;
        let body = text.strip_suffix('\n').ok_or(BadMarks)?;
        let mut lines = body.split('\n');
        let timed = match lines.next() {
            Some(MAGIC) => true,
            Some(UNTIMED_MAGIC) => false,
            _ => return Err(BadMarks),
        };
        let below = lines
            .next()
            .and_then(|l| l.strip_prefix("journals-below "))
            .and_then(number::<u64>)
            .ok_or(BadMarks)?;
        let mut epochs = BTreeMap::new();
        for line in lines {
            let (track, epoch) = epoch_line(line, timed).ok_or(BadMarks)?;
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

/// The track and epoch of an `epoch` line: timed, with five numbers, or
/// untimed, with two (the only kind a version 1 file has, so only `timed`
/// files may hold the first). `None` if it's neither.
fn epoch_line(line: &str, timed: bool) -> Option<(TrackId, MarkedEpoch)> {
    let words: Vec<&str> = line.strip_prefix("epoch ")?.split(' ').collect();
    let track = TrackId::new(number(words.first()?)?);
    let id = EpochId::new(number(words.get(1)?)?);
    let epoch = match *words.get(2..)? {
        [] => MarkedEpoch::Untimed(id),
        [first, hz, start] if timed => MarkedEpoch::Timed(EpochAnchor {
            id,
            first_sample: SampleIndex::new(number(first)?),
            rate: SampleRate::new(number(hz)?)?,
            start: SessionTime::from_nanos(number(start)?),
        }),
        _ => return None,
    };
    Some((track, epoch))
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

    /// Epoch `id`, from sample `first` at 16 kHz, starting at `start` ns.
    fn timed(id: u32, first: u64, start: u64) -> MarkedEpoch {
        MarkedEpoch::Timed(EpochAnchor {
            id: EpochId::new(id),
            start: SessionTime::from_nanos(start),
            first_sample: SampleIndex::new(first),
            rate: SampleRate::SPEECH,
        })
    }

    /// Marks with journal ids below `below`, and each track's newest epoch
    /// timed by [`timed`] from sample 0 at time 0.
    fn marks(below: u64, epochs: &[(u32, u32)]) -> Marks {
        Marks {
            journals_below: JournalId::new(below),
            epochs: epochs
                .iter()
                .map(|&(t, e)| (TrackId::new(t), timed(e, 0, 0)))
                .collect(),
        }
    }

    /// Each epoch line gives the track, the epoch and, when it's timed, its
    /// first sample, rate and start.
    #[test]
    fn the_format_is_plain_text() {
        let mut m = marks(64, &[]);
        m.epochs
            .insert(TrackId::new(0), timed(3, 48_000, 3_000_000_000));
        m.epochs
            .insert(TrackId::new(1), MarkedEpoch::Untimed(EpochId::new(0)));
        assert_eq!(
            m.encode(),
            "nota session marks 2\njournals-below 64\nepoch 0 3 48000 16000 3000000000\n\
             epoch 1 0\n"
        );
    }

    /// A version 1 file, from before epochs were timed, reads with its
    /// epochs untimed, and is written back as version 2.
    #[test]
    fn a_version_1_file_reads_with_untimed_epochs() {
        let old = Marks::decode(b"nota session marks 1\njournals-below 64\nepoch 0 3\n").unwrap();
        assert_eq!(
            old.epochs.get(&TrackId::new(0)),
            Some(&MarkedEpoch::Untimed(EpochId::new(3)))
        );
        assert_eq!(
            old.encode(),
            "nota session marks 2\njournals-below 64\nepoch 0 3\n"
        );
    }

    /// An epoch is the track's highest if it's numbered above the known
    /// one, or the same one with the anchor the known one lacks; never
    /// lower, and never a second anchor for an epoch already timed.
    #[test]
    fn the_highest_epoch_is_the_highest_number_timed_if_it_can_be() {
        let untimed = |id| MarkedEpoch::Untimed(EpochId::new(id));
        for (new, known, wins) in [
            (timed(3, 0, 0), None, true),
            (timed(4, 0, 0), Some(timed(3, 0, 0)), true),
            (untimed(4), Some(timed(3, 0, 0)), true),
            (timed(2, 0, 0), Some(timed(3, 0, 0)), false),
            (timed(2, 0, 0), Some(untimed(3)), false),
            (timed(3, 0, 0), Some(untimed(3)), true),
            (timed(3, 5, 5), Some(timed(3, 0, 0)), false),
            (untimed(3), Some(timed(3, 0, 0)), false),
            (untimed(3), Some(untimed(3)), false),
        ] {
            assert_eq!(new.supersedes(known), wins, "{new:?} over {known:?}");
        }
    }

    /// A marked epoch's number and anchor are what it was marked with.
    #[test]
    fn a_marked_epoch_gives_its_number_and_anchor() {
        let epoch = timed(3, 7, 9);
        assert_eq!(epoch.id(), EpochId::new(3));
        assert_eq!(
            epoch.anchor().map(|a| (a.first_sample, a.start)),
            Some((SampleIndex::new(7), SessionTime::from_nanos(9)))
        );
        let untimed = MarkedEpoch::Untimed(EpochId::new(4));
        assert_eq!((untimed.id(), untimed.anchor()), (EpochId::new(4), None));
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
            "nota session marks 1\njournals-below 064\n",
            "nota session marks 1\njournals-below +64\n",
            "nota session marks 1\njournals-below 64\nepoch 0 1 2\n",
            "nota session marks 1\njournals-below 64\nepoch 1 0\nepoch 0 0\n",
            "nota session marks 1\njournals-below 64\nepoch 0 0\nepoch 0 1\n",
            "nota session marks 1\njournals-below 64\n\n",
            // Version 1 has no timed epochs.
            "nota session marks 1\njournals-below 64\nepoch 0 1 0 16000 0\n",
            // A timed epoch has all three numbers, a real rate, and no more.
            "nota session marks 2\njournals-below 64\nepoch 0 1 0 16000\n",
            "nota session marks 2\njournals-below 64\nepoch 0 1 0 0 0\n",
            "nota session marks 2\njournals-below 64\nepoch 0 1 0 16000 0 0\n",
            "nota session marks 2\njournals-below 64\nepoch 0 1 00 16000 0\n",
            "nota session marks 2\njournals-below 64\nepoch 0\n",
            "nota session marks 3\njournals-below 64\n",
        ] {
            assert_eq!(Marks::decode(text.as_bytes()), Err(BadMarks), "{text:?}");
        }
    }

    proptest! {
        #[test]
        fn encode_then_decode_is_the_identity(
            below in any::<u64>(),
            epochs in proptest::collection::btree_map(any::<u32>(), any_marked_epoch(), 0..8),
        ) {
            let m = Marks {
                journals_below: JournalId::new(below),
                epochs: epochs.into_iter().map(|(t, e)| (TrackId::new(t), e)).collect(),
            };
            prop_assert_eq!(Marks::decode(m.encode().as_bytes()), Ok(m));
        }

        /// Whatever the bytes, decoding doesn't panic; what it accepts
        /// encodes back to the same marks, and a version 2 file to the same
        /// bytes.
        #[test]
        fn decoding_any_bytes_never_panics_and_round_trips_what_it_accepts(
            bytes in proptest::collection::vec(any::<u8>(), 0..96),
        ) {
            if let Ok(m) = Marks::decode(&bytes) {
                let encoded = m.encode().into_bytes();
                prop_assert_eq!(Marks::decode(&encoded), Ok(m));
                if bytes.starts_with(MAGIC.as_bytes()) {
                    prop_assert_eq!(encoded, bytes);
                }
            }
        }

        /// A version 2 file of timed and untimed epochs, damaged by one
        /// byte, never decodes to other marks with the same encoding.
        #[test]
        fn a_changed_byte_is_refused_or_read_as_written(
            epochs in proptest::collection::btree_map(any::<u32>(), any_marked_epoch(), 1..4),
            at in any::<prop::sample::Index>(),
            to in any::<u8>(),
        ) {
            let m = Marks {
                journals_below: JournalId::new(64),
                epochs: epochs.into_iter().map(|(t, e)| (TrackId::new(t), e)).collect(),
            };
            let mut bytes = m.encode().into_bytes();
            let at = at.index(bytes.len());
            bytes[at] = to;
            if let Ok(read) = Marks::decode(&bytes) {
                prop_assert_eq!(read.encode().into_bytes(), bytes);
            }
        }
    }

    /// Any marked epoch: timed at any rate, or untimed.
    fn any_marked_epoch() -> impl Strategy<Value = MarkedEpoch> {
        prop_oneof![
            any::<u32>().prop_map(|id| MarkedEpoch::Untimed(EpochId::new(id))),
            (
                any::<u32>(),
                any::<u64>(),
                1..=SampleRate::MAX_HZ,
                any::<u64>()
            )
                .prop_map(|(id, first, hz, start)| MarkedEpoch::Timed(EpochAnchor {
                    id: EpochId::new(id),
                    start: SessionTime::from_nanos(start),
                    first_sample: SampleIndex::new(first),
                    rate: SampleRate::new(hz).unwrap(),
                })),
        ]
    }
}
