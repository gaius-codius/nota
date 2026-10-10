//! Identifiers that would otherwise be bare integers.

/// One recorded track in a session: the mic, or the system audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrackId(u32);

impl TrackId {
    /// The track numbered `id`.
    #[must_use]
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// The track's number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// One epoch of a track: the stretch from one opening of its stream, one
/// loss of audio, or one correction of its drift, to the next.
/// Numbered from zero within each track, in the order they were opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EpochId(u32);

impl EpochId {
    /// The epoch numbered `id`.
    #[must_use]
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// The epoch's number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The epoch numbered one above this one, or `None` after the last.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(id) => Some(Self(id)),
            None => None,
        }
    }
}

/// One recording session: a lecture or workshop, recorded into its own
/// directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(u64);

impl SessionId {
    /// The session numbered `id`.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The session's number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        assert_eq!(TrackId::new(7).get(), 7);
        assert_eq!(EpochId::new(9).get(), 9);
        assert_eq!(SessionId::new(u64::MAX).get(), u64::MAX);
        assert!(EpochId::new(1) < EpochId::new(2));
    }

    /// Epoch numbers count up by one, and the last has no next.
    #[test]
    fn the_next_epoch_is_one_higher_until_the_last() {
        assert_eq!(EpochId::new(4).next(), Some(EpochId::new(5)));
        assert_eq!(EpochId::new(u32::MAX).next(), None);
    }
}
