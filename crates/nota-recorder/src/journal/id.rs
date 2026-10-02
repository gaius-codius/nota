//! Which journal file a run of audio came from.

use std::ffi::OsStr;

/// The prefix of every journal's file name.
const PREFIX: &str = "journal-";
/// The fewest digits in a journal's file name: ids are zero-padded to this
/// width so names sort by id for a person listing the directory. Code never
/// relies on that; it sorts by the parsed id.
const WIDTH: usize = 6;

/// A journal's identity: numbered in order within a session, across all of
/// its tracks, from [`JournalId::FIRST`]. Each journal file holds one, in its
/// name and in its header, and every [`DurablePosition`] carries the id of
/// the journal it covers, so positions from two journals never compare equal.
///
/// Ids order journals by when they were started. After a crash two journals
/// of one track can hold the same samples (a broken journal's unsynced tail,
/// and its replacement); the higher id wins.
///
/// [`DurablePosition`]: super::DurablePosition
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JournalId(u64);

impl JournalId {
    /// A session's first journal.
    pub const FIRST: Self = Self(0);

    /// The journal numbered `id`.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The journal's number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The id after this one, or `None` if the numbers ran out.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// The journal's file name: `journal-000042`.
    #[must_use]
    pub fn file_name(self) -> String {
        format!("{PREFIX}{:0WIDTH$}", self.0)
    }

    /// The id in a journal's file name, if `name` is exactly what
    /// [`Self::file_name`] makes for some id. Anything else (another file,
    /// a temp file, a non-canonical spelling) is `None`.
    #[must_use]
    pub fn from_file_name(name: &OsStr) -> Option<Self> {
        let digits = name.to_str()?.strip_prefix(PREFIX)?;
        let id = Self(digits.parse().ok()?);
        // One spelling per id: comparing with the canonical name refuses a
        // sign, too few digits and extra leading zeros.
        (name.to_str()? == id.file_name()).then_some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_round_trip() {
        for n in [0, 1, 42, 999_999, 1_000_000, u64::MAX] {
            let id = JournalId::new(n);
            let name = id.file_name();
            assert_eq!(JournalId::from_file_name(OsStr::new(&name)), Some(id));
        }
        assert_eq!(JournalId::new(42).file_name(), "journal-000042");
        assert_eq!(JournalId::new(1_234_567).file_name(), "journal-1234567");
    }

    #[test]
    fn other_names_are_not_journals() {
        for name in [
            "journal",
            "journal-",
            "journal-42",
            "journal-0000042",
            "journal-00004a",
            "journal-+00042",
            "journal-000042.tmp",
            "xjournal-000042",
            "t0-000000000000.flac",
            "journal-99999999999999999999",
        ] {
            assert_eq!(JournalId::from_file_name(OsStr::new(name)), None, "{name}");
        }
    }

    #[test]
    fn ids_count_up_and_stop_at_the_end() {
        assert_eq!(JournalId::FIRST.get(), 0);
        assert_eq!(JournalId::FIRST.next(), Some(JournalId::new(1)));
        assert_eq!(JournalId::new(u64::MAX).next(), None);
        assert!(JournalId::new(1) < JournalId::new(2));
    }
}
