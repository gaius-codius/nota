//! Where segment rows are committed.
//!
//! The real store is SQLite ([`nota_store::Store`]), one library database
//! holding every session's rows. Crash tests use `FakeStore`, which keeps
//! rows as files on a `FakeFs`, so a simulated crash covers the rows and the
//! segments together.

use std::error::Error;

use nota_core::SessionId;
use nota_store::{RowKey, SegmentRow};

use super::publish::DurableSegment;

/// A store of segment rows, scoped by session.
pub trait SegmentStore {
    /// Why an operation failed.
    type Error: Error + Send + Sync + 'static;

    /// Every committed row of `session`.
    ///
    /// # Errors
    ///
    /// The store's error.
    fn rows(&mut self, session: SessionId) -> Result<Vec<SegmentRow>, Self::Error>;

    /// Commits `segment`'s row for `session`, durably, before returning,
    /// and first the anchor of its epoch, if it has one
    /// ([`DurableSegment::anchor`]), so a committed row's epoch is timed in
    /// the store. Committing the same row or anchor again is fine. An epoch
    /// stored with another anchor, or whose stored row doesn't parse, keeps
    /// what it has and the row is committed all the same: its audio
    /// matters more than its timing. Takes a [`DurableSegment`], so a row
    /// can only be committed for a file that's durable under its final
    /// name.
    ///
    /// # Errors
    ///
    /// The store's error, including a different row of the session for the
    /// same track and first sample, a row of the same session and track
    /// whose samples overlap this one's, and a session the store doesn't
    /// hold. A row isn't committed if its anchor failed for another reason.
    fn insert(&mut self, session: SessionId, segment: &DurableSegment) -> Result<(), Self::Error>;

    /// Whether `error` says the disk is full (see
    /// [`is_disk_full`](crate::fs::is_disk_full)). By default, whether it,
    /// or an error it was caused by, is an [`io::Error`](std::io::Error)
    /// that does.
    fn is_disk_full(error: &Self::Error) -> bool {
        caused_by_a_full_disk(error)
    }

    /// The row `error` says doesn't parse, if it says that: publishing
    /// names it in the findings file. By default, the row of the first
    /// [`StoreError::CorruptRow`](nota_store::StoreError::CorruptRow) in
    /// its chain of sources, itself included.
    fn unparsable_row(error: &Self::Error) -> Option<RowKey> {
        corrupt_row(error)
    }
}

/// The row of the first `CorruptRow` store error in `error`'s chain of
/// sources, itself included.
fn corrupt_row(error: &(dyn Error + 'static)) -> Option<RowKey> {
    let mut next = Some(error);
    while let Some(e) = next {
        if let Some(nota_store::StoreError::CorruptRow { key, .. }) =
            e.downcast_ref::<nota_store::StoreError>()
        {
            return Some(*key);
        }
        next = e.source();
    }
    None
}

/// Whether `error`, or any error in its chain of sources, is an I/O error
/// for want of space.
fn caused_by_a_full_disk(error: &(dyn Error + 'static)) -> bool {
    let mut next = Some(error);
    while let Some(e) = next {
        if e.downcast_ref::<std::io::Error>()
            .is_some_and(crate::fs::is_disk_full)
        {
            return true;
        }
        next = e.source();
    }
    false
}

impl SegmentStore for nota_store::Store {
    type Error = nota_store::StoreError;

    fn rows(&mut self, session: SessionId) -> Result<Vec<SegmentRow>, Self::Error> {
        self.segments(session)
    }

    fn insert(&mut self, session: SessionId, segment: &DurableSegment) -> Result<(), Self::Error> {
        if let Some(anchor) = segment.anchor() {
            match self.insert_epoch(session, segment.row().track(), anchor) {
                // The epoch keeps the anchor it has; the row still commits.
                Ok(_)
                | Err(
                    nota_store::StoreError::EpochConflict { .. }
                    | nota_store::StoreError::Corrupt(_),
                ) => {}
                Err(e) => return Err(e),
            }
        }
        self.insert_segment(session, segment.row()).map(|_| ())
    }

    fn is_disk_full(error: &Self::Error) -> bool {
        error.is_disk_full()
    }
}

/// A shared handle on the library database, opened on first use.
impl SegmentStore for nota_store::Writer {
    type Error = nota_store::StoreError;

    fn rows(&mut self, session: SessionId) -> Result<Vec<SegmentRow>, Self::Error> {
        self.with(|store| store.segments(session))
    }

    fn insert(&mut self, session: SessionId, segment: &DurableSegment) -> Result<(), Self::Error> {
        self.with(|store| SegmentStore::insert(store, session, segment))
    }

    fn is_disk_full(error: &Self::Error) -> bool {
        error.is_disk_full()
    }
}

/// A store lent to a [`SessionStore`](crate::session::SessionStore), so the
/// caller keeps it when the session ends.
impl<T: SegmentStore + ?Sized> SegmentStore for &mut T {
    type Error = T::Error;

    fn rows(&mut self, session: SessionId) -> Result<Vec<SegmentRow>, Self::Error> {
        (**self).rows(session)
    }

    fn insert(&mut self, session: SessionId, segment: &DurableSegment) -> Result<(), Self::Error> {
        (**self).insert(session, segment)
    }

    fn is_disk_full(error: &Self::Error) -> bool {
        T::is_disk_full(error)
    }

    fn unparsable_row(error: &Self::Error) -> Option<RowKey> {
        T::unparsable_row(error)
    }
}

#[cfg(any(test, feature = "fake-fs"))]
pub use fake::FakeStore;

#[cfg(any(test, feature = "fake-fs"))]
mod fake {
    use std::io;
    use std::path::{Path, PathBuf};

    use nota_core::{
        Drift, EpochAnchor, EpochId, SampleIndex, SampleRange, SampleRate, SessionId, SessionTime,
        TrackId,
    };
    use nota_store::{AudioDigest, SegmentRow, Sha256Digest};

    use super::{DurableSegment, SegmentStore};
    use crate::fs::fake::FakeFs;
    use crate::fs::{Fs, FsFile};

    /// Bytes in a row file: session, track, epoch, start, end, SHA-256,
    /// whether it has an audio digest (0 or 1), the digest (zeros if not),
    /// CRC-32.
    const ROW_LEN: usize = 8 + 4 + 4 + 8 + 8 + 32 + 1 + 32 + 4;

    /// Bytes in an epoch file: session, track, epoch, first sample, rate,
    /// start, drift, CRC-32.
    const EPOCH_LEN: usize = 8 + 4 + 4 + 8 + 4 + 8 + 4 + 4;

    /// Segment rows as files on a [`FakeFs`], one per row, each published
    /// by temp file, fsync, rename and directory fsync: a commit that's
    /// atomic and durable when it returns, as SQLite's with
    /// `synchronous=FULL`, and that a simulated crash can interrupt. Like
    /// SQLite, it keeps rows by session, and refuses a row whose samples
    /// overlap another row of the same session and track. Epochs' anchors
    /// are files too, committed the same way; an epoch stored already keeps
    /// the anchor it has.
    ///
    /// It writes through any [`Fs`], a [`FakeFs`] by default: through one
    /// that wraps a fake (a [`WatchedFs`](crate::disk::WatchedFs), say),
    /// its commits meet what the wrapper does.
    #[derive(Debug, Clone)]
    pub struct FakeStore<S = FakeFs> {
        fs: S,
        dir: PathBuf,
    }

    impl<S: Fs + Clone> FakeStore<S> {
        /// A store keeping its rows in `dir` on `fs`. The directory must
        /// exist.
        #[must_use]
        pub fn new(fs: &S, dir: &Path) -> Self {
            Self {
                fs: fs.clone(),
                dir: dir.to_path_buf(),
            }
        }
    }

    #[cfg(test)]
    impl<S: Fs> FakeStore<S> {
        /// Commits `row` for `session` as it is, with no file behind it:
        /// for tests that plant a row a store could hold (a restored one,
        /// one from before audio digests).
        pub(crate) fn plant(&self, session: SessionId, row: &SegmentRow) {
            let path = self.row_path(session, row);
            let mut file = self.fs.create(&path).unwrap();
            file.write_all(&encode(session, row)).unwrap();
            file.sync().unwrap();
            self.fs.sync_dir(&self.dir).unwrap();
        }
    }

    impl<S: Fs> FakeStore<S> {
        /// Every anchor of `session`, ordered by track then epoch, as
        /// [`nota_store::Store::epochs`] gives them.
        ///
        /// # Errors
        ///
        /// Any I/O error, and [`io::ErrorKind::InvalidData`] for a file
        /// that doesn't decode.
        pub fn epochs(&self, session: SessionId) -> io::Result<Vec<(TrackId, EpochAnchor)>> {
            let mut epochs = Vec::new();
            for path in self.fs.list(&self.dir)? {
                if path.extension().is_some_and(|e| e == "epoch") {
                    let bytes = self.fs.read(&path)?;
                    let (owner, track, anchor) =
                        decode_epoch(&bytes).ok_or_else(|| corrupt(&path))?;
                    if owner == session {
                        epochs.push((track, anchor));
                    }
                }
            }
            epochs.sort_by_key(|(track, anchor)| (*track, anchor.id));
            Ok(epochs)
        }

        /// Commits `track`'s `anchor` for `session`, unless the epoch is
        /// stored already: it keeps the anchor it has, as SQLite's store
        /// does through [`SegmentStore::insert`].
        fn insert_epoch(
            &self,
            session: SessionId,
            track: TrackId,
            anchor: &EpochAnchor,
        ) -> io::Result<()> {
            let path = self.dir.join(format!(
                "s{}-t{}-e{}.epoch",
                session.get(),
                track.get(),
                anchor.id.get()
            ));
            match self.fs.read(&path) {
                Ok(_) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    self.commit(&path, &encode_epoch(session, track, anchor))
                }
                Err(e) => Err(e),
            }
        }

        /// Writes `bytes` to `path` as a commit: temp file, fsync, rename,
        /// directory fsync.
        fn commit(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            let temp = path.with_extension("tmp");
            match self.fs.remove(&temp) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let mut file = self.fs.create(&temp)?;
            file.write_all(bytes)?;
            file.sync()?;
            self.fs.rename(&temp, path)?;
            if let Err(e) = self.fs.sync_dir(&self.dir) {
                // Not committed: don't leave a file a later read would
                // report while its name isn't durable. Best effort.
                let _ = self.fs.remove(path);
                return Err(e);
            }
            Ok(())
        }
    }

    /// The bytes of `track`'s `anchor` in `session`.
    fn encode_epoch(session: SessionId, track: TrackId, anchor: &EpochAnchor) -> Vec<u8> {
        let mut out = Vec::with_capacity(EPOCH_LEN);
        out.extend_from_slice(&session.get().to_le_bytes());
        out.extend_from_slice(&track.get().to_le_bytes());
        out.extend_from_slice(&anchor.id.get().to_le_bytes());
        out.extend_from_slice(&anchor.first_sample.get().to_le_bytes());
        out.extend_from_slice(&anchor.rate.hz().to_le_bytes());
        out.extend_from_slice(&anchor.start.as_nanos().to_le_bytes());
        out.extend_from_slice(&anchor.drift.ppb().to_le_bytes());
        let crc = crc32fast::hash(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    /// The session, track and anchor in an epoch file's `bytes`, if they
    /// check out.
    fn decode_epoch(bytes: &[u8]) -> Option<(SessionId, TrackId, EpochAnchor)> {
        if bytes.len() != EPOCH_LEN {
            return None;
        }
        let (body, crc) = bytes.split_at(EPOCH_LEN - 4);
        if crc32fast::hash(body).to_le_bytes() != crc {
            return None;
        }
        let u32_at = |at: usize| Some(u32::from_le_bytes(body.get(at..at + 4)?.try_into().ok()?));
        let u64_at = |at: usize| Some(u64::from_le_bytes(body.get(at..at + 8)?.try_into().ok()?));
        let anchor = EpochAnchor {
            id: EpochId::new(u32_at(12)?),
            first_sample: SampleIndex::new(u64_at(16)?),
            rate: SampleRate::new(u32_at(24)?)?,
            drift: Drift::from_ppb(i32::from_le_bytes(body.get(36..40)?.try_into().ok()?))?,
            start: SessionTime::from_nanos(u64_at(28)?),
        };
        Some((SessionId::new(u64_at(0)?), TrackId::new(u32_at(8)?), anchor))
    }

    impl<S> FakeStore<S> {
        fn row_path(&self, session: SessionId, row: &SegmentRow) -> PathBuf {
            self.dir.join(format!(
                "s{}-t{}-{:020}.row",
                session.get(),
                row.track().get(),
                row.range().start().get()
            ))
        }
    }

    fn encode(session: SessionId, row: &SegmentRow) -> Vec<u8> {
        let mut out = Vec::with_capacity(ROW_LEN);
        out.extend_from_slice(&session.get().to_le_bytes());
        out.extend_from_slice(&row.track().get().to_le_bytes());
        out.extend_from_slice(&row.epoch().get().to_le_bytes());
        out.extend_from_slice(&row.range().start().get().to_le_bytes());
        out.extend_from_slice(&row.range().end().get().to_le_bytes());
        out.extend_from_slice(row.sha256().as_bytes());
        if let Some(audio) = row.audio() {
            out.push(1);
            out.extend_from_slice(audio.as_bytes());
        } else {
            out.push(0);
            out.extend_from_slice(&[0; 32]);
        }
        let crc = crc32fast::hash(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Option<(SessionId, SegmentRow)> {
        if bytes.len() != ROW_LEN {
            return None;
        }
        let (body, crc) = bytes.split_at(ROW_LEN - 4);
        if crc32fast::hash(body).to_le_bytes() != crc {
            return None;
        }
        let u32_at = |at: usize| Some(u32::from_le_bytes(body.get(at..at + 4)?.try_into().ok()?));
        let u64_at = |at: usize| Some(u64::from_le_bytes(body.get(at..at + 8)?.try_into().ok()?));
        let range = SampleRange::new(SampleIndex::new(u64_at(16)?), SampleIndex::new(u64_at(24)?))?;
        let row = SegmentRow::new(
            TrackId::new(u32_at(8)?),
            EpochId::new(u32_at(12)?),
            range,
            Sha256Digest::new(body.get(32..64)?.try_into().ok()?),
        )?;
        let audio: [u8; 32] = body.get(65..97)?.try_into().ok()?;
        let row = match body.get(64)? {
            0 if audio == [0; 32] => row,
            1 => row.with_audio(AudioDigest::new(audio)),
            _ => return None,
        };
        Some((SessionId::new(u64_at(0)?), row))
    }

    fn corrupt(path: &Path) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bad row file {}", path.display()),
        )
    }

    impl<S: Fs> SegmentStore for FakeStore<S> {
        type Error = io::Error;

        fn rows(&mut self, session: SessionId) -> io::Result<Vec<SegmentRow>> {
            let mut rows = Vec::new();
            for path in self.fs.list(&self.dir)? {
                if path.extension().is_some_and(|e| e == "row") {
                    let bytes = self.fs.read(&path)?;
                    let (owner, row) = decode(&bytes).ok_or_else(|| corrupt(&path))?;
                    if owner == session {
                        rows.push(row);
                    }
                }
            }
            rows.sort_by_key(|r| (r.track(), r.range().start()));
            Ok(rows)
        }

        fn insert(&mut self, session: SessionId, segment: &DurableSegment) -> io::Result<()> {
            if let Some(anchor) = segment.anchor() {
                self.insert_epoch(session, segment.row().track(), anchor)?;
            }
            let row = segment.row();
            let path = self.row_path(session, row);
            match self.fs.read(&path) {
                Ok(bytes) if decode(&bytes) == Some((session, *row)) => return Ok(()),
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "a different row holds this track and first sample",
                    ));
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let range = row.range();
            let overlaps = self.rows(session)?.iter().any(|other| {
                other.track() == row.track()
                    && other.range().start() < range.end()
                    && other.range().end() > range.start()
            });
            if overlaps {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "a row of this track overlaps this one's samples",
                ));
            }
            self.commit(&path, &encode(session, row))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const SESSION: SessionId = SessionId::new(1);

        #[test]
        fn a_lent_store_is_the_store() {
            use crate::fs::fake::FakeFs;
            use crate::segment::publish::TempSegment;

            let fs = FakeFs::with_dirs(["/s", "/db"]);
            let range = SampleRange::new(SampleIndex::new(5), SampleIndex::new(9)).unwrap();
            let durable = TempSegment::write(
                &fs,
                Path::new("/s"),
                TrackId::new(1),
                EpochId::new(0),
                range,
                b"flac",
                AudioDigest::new([0; 32]),
            )
            .unwrap()
            .sync()
            .unwrap()
            .rename(&fs)
            .unwrap()
            .sync_dir(&fs)
            .unwrap();
            let mut store = FakeStore::new(&fs, Path::new("/db"));
            let mut lent = &mut store;
            SegmentStore::insert(&mut lent, SESSION, &durable).unwrap();
            let rows = SegmentStore::rows(&mut lent, SESSION).unwrap();
            assert_eq!(rows, [*durable.row()]);
            assert_eq!(store.rows(SESSION).unwrap(), rows);
        }

        #[test]
        fn overlapping_rows_are_refused_as_sqlite_does() {
            use crate::fs::fake::FakeFs;
            use crate::segment::publish::TempSegment;

            let fs = FakeFs::with_dirs(["/s", "/db"]);
            let make = |track: u32, start: u64, end: u64| {
                let range =
                    SampleRange::new(SampleIndex::new(start), SampleIndex::new(end)).unwrap();
                TempSegment::write(
                    &fs,
                    Path::new("/s"),
                    TrackId::new(track),
                    EpochId::new(0),
                    range,
                    b"flac",
                    AudioDigest::new([0; 32]),
                )
                .unwrap()
                .sync()
                .unwrap()
                .rename(&fs)
                .unwrap()
                .sync_dir(&fs)
                .unwrap()
            };
            let mut store = FakeStore::new(&fs, Path::new("/db"));
            let a = make(1, 10, 20);
            store.insert(SESSION, &a).unwrap();
            store.insert(SESSION, &a).unwrap();
            let before = fs.paths();

            for (start, end) in [(15, 25), (5, 11), (12, 18)] {
                let err = store.insert(SESSION, &make(1, start, end)).unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{start}..{end}");
            }
            assert_eq!(store.rows(SESSION).unwrap(), [*a.row()]);
            let db_files = |paths: &[PathBuf]| -> Vec<PathBuf> {
                paths
                    .iter()
                    .filter(|p| p.starts_with("/db"))
                    .cloned()
                    .collect()
            };
            assert_eq!(db_files(&fs.paths()), db_files(&before));

            let touching_after = make(1, 20, 30);
            let touching_before = make(1, 0, 10);
            let other_track = make(2, 10, 20);
            store.insert(SESSION, &touching_after).unwrap();
            store.insert(SESSION, &touching_before).unwrap();
            store.insert(SESSION, &other_track).unwrap();
            assert_eq!(
                store.rows(SESSION).unwrap(),
                [
                    *touching_before.row(),
                    *a.row(),
                    *touching_after.row(),
                    *other_track.row()
                ]
            );
        }

        #[test]
        fn sessions_with_the_same_coordinates_coexist_and_read_only_their_own() {
            use crate::fs::fake::FakeFs;
            use crate::segment::publish::TempSegment;

            let fs = FakeFs::with_dirs(["/s", "/db"]);
            let make = |body: &[u8]| {
                let range = SampleRange::new(SampleIndex::new(10), SampleIndex::new(20)).unwrap();
                TempSegment::write(
                    &fs,
                    Path::new("/s"),
                    TrackId::new(1),
                    EpochId::new(0),
                    range,
                    body,
                    AudioDigest::new([0; 32]),
                )
                .unwrap()
                .sync()
                .unwrap()
                .rename(&fs)
                .unwrap()
                .sync_dir(&fs)
                .unwrap()
            };
            let (one, two) = (SessionId::new(1), SessionId::new(2));
            let (a, b) = (make(b"flac a"), make(b"flac b"));
            assert_ne!(a.row(), b.row());
            let mut store = FakeStore::new(&fs, Path::new("/db"));
            store.insert(one, &a).unwrap();
            store.insert(two, &b).unwrap();
            // Same session, same coordinates, different row: refused.
            assert_eq!(
                store.insert(one, &b).unwrap_err().kind(),
                io::ErrorKind::AlreadyExists
            );
            assert_eq!(store.rows(one).unwrap(), [*a.row()]);
            assert_eq!(store.rows(two).unwrap(), [*b.row()]);
            assert!(store.rows(SessionId::new(3)).unwrap().is_empty());
        }

        /// An epoch file reads back as written, and a file that's cut,
        /// flipped or has no real rate isn't one.
        #[test]
        fn epochs_round_trip_and_bad_files_are_refused() {
            let anchor = EpochAnchor {
                id: EpochId::new(4),
                start: SessionTime::from_nanos(9),
                first_sample: SampleIndex::new(u64::MAX),
                rate: SampleRate::SPEECH,
                drift: Drift::from_ppb(-123_456).unwrap(),
            };
            let bytes = encode_epoch(SESSION, TrackId::new(3), &anchor);
            assert_eq!(bytes.len(), EPOCH_LEN);
            assert_eq!(
                decode_epoch(&bytes),
                Some((SESSION, TrackId::new(3), anchor))
            );
            for cut in 0..bytes.len() {
                assert_eq!(decode_epoch(&bytes[..cut]), None);
            }
            for at in 0..bytes.len() {
                let mut flipped = bytes.clone();
                flipped[at] ^= 1;
                assert_eq!(decode_epoch(&flipped), None, "{at}");
            }
            let mut no_rate = bytes;
            no_rate[24..28].fill(0);
            let crc = crc32fast::hash(&no_rate[..EPOCH_LEN - 4]);
            no_rate[EPOCH_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
            assert_eq!(decode_epoch(&no_rate), None);
        }

        /// A damaged epoch file is reported, not skipped.
        #[test]
        fn a_damaged_epoch_file_is_an_error() {
            let fs = FakeFs::with_dirs(["/db"]);
            let mut file = fs.create(Path::new("/db/s1-t0-e0.epoch")).unwrap();
            file.write_all(b"junk").unwrap();
            let store = FakeStore::new(&fs, Path::new("/db"));
            assert_eq!(
                store.epochs(SESSION).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }

        #[test]
        fn rows_round_trip_and_bad_files_are_refused() {
            let row = SegmentRow::new(
                TrackId::new(3),
                EpochId::new(2),
                SampleRange::new(SampleIndex::new(10), SampleIndex::new(u64::MAX)).unwrap(),
                Sha256Digest::new([7; 32]),
            )
            .unwrap();
            let bytes = encode(SESSION, &row);
            assert_eq!(decode(&bytes), Some((SESSION, row)));
            let audio = row.with_audio(AudioDigest::new([5; 32]));
            assert_eq!(decode(&encode(SESSION, &audio)), Some((SESSION, audio)));
            // A digest without its flag, or a flag past 1, isn't a row.
            for (at, value) in [(64, 2), (65, 1)] {
                let mut odd = bytes.clone();
                odd[at] = value;
                let crc = crc32fast::hash(&odd[..ROW_LEN - 4]);
                odd[ROW_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
                assert_eq!(decode(&odd), None, "{at}");
            }
            for cut in 0..bytes.len() {
                assert_eq!(decode(&bytes[..cut]), None);
            }
            let mut flipped = bytes;
            flipped[17] ^= 1;
            assert_eq!(decode(&flipped), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use nota_core::{
        EpochAnchor, EpochId, SampleIndex, SampleRange, SampleRate, SessionId, SessionTime, TrackId,
    };
    use nota_store::{NewSession, Store, Writer};

    use super::SegmentStore;
    use crate::fs::fake::FakeFs;
    use crate::segment::publish::{DurableSegment, TempSegment};
    use crate::test_dir::TestDir;

    fn durable(fs: &FakeFs, start: u64) -> DurableSegment {
        let range = SampleRange::new(SampleIndex::new(start), SampleIndex::new(start + 4)).unwrap();
        TempSegment::write(
            fs,
            Path::new("/s"),
            TrackId::new(1),
            EpochId::new(0),
            range,
            b"flac",
            nota_store::AudioDigest::new([0; 32]),
        )
        .unwrap()
        .sync()
        .unwrap()
        .rename(fs)
        .unwrap()
        .sync_dir(fs)
        .unwrap()
    }

    fn session(id: u64) -> NewSession {
        NewSession {
            id: SessionId::new(id),
            title: None,
            language: None,
            started_at: None,
            tracks: vec![],
        }
    }

    /// Commits a row for session 1 through `store`, and reads each
    /// session's rows back: only session 1 holds it.
    fn round_trip(store: &mut impl SegmentStore) {
        let fs = FakeFs::with_dirs(["/s"]);
        let segment = durable(&fs, 5);
        store.insert(SessionId::new(1), &segment).unwrap();
        assert_eq!(store.rows(SessionId::new(1)).unwrap(), [*segment.row()]);
        assert_eq!(store.rows(SessionId::new(2)).unwrap(), []);
        // A session the database doesn't hold is refused.
        assert!(store.insert(SessionId::new(3), &durable(&fs, 20)).is_err());
    }

    #[test]
    fn the_library_database_is_a_store_scoped_by_session() {
        let dir = TestDir::new("sqlite-store");
        let mut store = Store::open(&dir.0.join("library.db")).unwrap();
        store.create_session(&session(1)).unwrap();
        store.create_session(&session(2)).unwrap();
        round_trip(&mut store);
    }

    /// Epoch 0 of the test's track, starting `start` ns in.
    fn anchor(start: u64) -> EpochAnchor {
        EpochAnchor {
            id: EpochId::new(0),
            start: SessionTime::from_nanos(start),
            first_sample: SampleIndex::ZERO,
            rate: SampleRate::SPEECH,
            drift: nota_core::Drift::ZERO,
        }
    }

    /// Commits a timed segment through `store`: its epoch's anchor is
    /// committed with it, which `epochs` reads back; a segment whose epoch
    /// the store times otherwise is committed, and the epoch keeps its
    /// anchor.
    fn timed_round_trip<S: SegmentStore>(
        store: &mut S,
        epochs: impl Fn(&mut S) -> Vec<(TrackId, EpochAnchor)>,
    ) {
        let fs = FakeFs::with_dirs(["/s"]);
        let one = SessionId::new(1);
        let segment = durable(&fs, 5).timed_by(Some(anchor(7)));
        assert_eq!(segment.anchor(), Some(&anchor(7)));
        store.insert(one, &segment).unwrap();
        // Again is fine.
        store.insert(one, &segment).unwrap();
        assert_eq!(epochs(store), [(TrackId::new(1), anchor(7))]);
        let other = durable(&fs, 20).timed_by(Some(anchor(8)));
        store.insert(one, &other).unwrap();
        assert_eq!(store.rows(one).unwrap(), [*segment.row(), *other.row()]);
        assert_eq!(epochs(store), [(TrackId::new(1), anchor(7))]);
        // An untimed segment leaves the epochs as they are.
        store.insert(one, &durable(&fs, 30)).unwrap();
        assert_eq!(epochs(store), [(TrackId::new(1), anchor(7))]);
        assert_eq!(store.rows(one).unwrap().len(), 3);
    }

    #[test]
    fn the_library_database_commits_an_epoch_s_anchor_with_its_rows() {
        let dir = TestDir::new("sqlite-epochs");
        let mut store = Store::open(&dir.0.join("library.db")).unwrap();
        store.create_session(&session(1)).unwrap();
        timed_round_trip(&mut store, |s| s.epochs(SessionId::new(1)).unwrap());
    }

    #[test]
    fn a_shared_writer_commits_an_epoch_s_anchor_with_its_rows() {
        let dir = TestDir::new("writer-epochs");
        let mut writer = Writer::new(&dir.0.join("library.db"));
        writer.with(|db| db.create_session(&session(1))).unwrap();
        timed_round_trip(&mut writer, |w| {
            w.with(|db| db.epochs(SessionId::new(1))).unwrap()
        });
    }

    #[test]
    fn the_fake_store_commits_an_epoch_s_anchor_with_its_rows() {
        let fs = FakeFs::with_dirs(["/db"]);
        let mut store = super::FakeStore::new(&fs, Path::new("/db"));
        timed_round_trip(&mut store, |s| s.epochs(SessionId::new(1)).unwrap());
        assert_eq!(store.epochs(SessionId::new(2)).unwrap(), []);
    }

    #[test]
    fn a_shared_writer_is_the_same_store() {
        let dir = TestDir::new("writer-store");
        let mut writer = Writer::new(&dir.0.join("library.db"));
        writer
            .with(|db| {
                db.create_session(&session(1))?;
                db.create_session(&session(2))
            })
            .unwrap();
        round_trip(&mut writer);
        // Through a clone, the same rows.
        assert_eq!(writer.clone().rows(SessionId::new(1)).unwrap().len(), 1);
    }

    /// Each store tells a full disk from its other errors.
    #[test]
    fn stores_tell_a_full_disk_from_other_errors() {
        use std::io;

        use nota_store::StoreError;

        use crate::segment::FakeStore;

        let sqlite = |code| {
            StoreError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ))
        };
        let (full, busy) = (
            sqlite(rusqlite::ffi::SQLITE_FULL),
            sqlite(rusqlite::ffi::SQLITE_BUSY),
        );
        assert!(<Store as SegmentStore>::is_disk_full(&full));
        assert!(!<Store as SegmentStore>::is_disk_full(&busy));
        assert!(<Writer as SegmentStore>::is_disk_full(&full));
        assert!(!<Writer as SegmentStore>::is_disk_full(&busy));
        assert!(<&mut Store as SegmentStore>::is_disk_full(&full));
        assert!(!<&mut Store as SegmentStore>::is_disk_full(&busy));
        // The default: an I/O error for want of space, itself or as a cause.
        let enospc = io::Error::new(io::ErrorKind::StorageFull, "full");
        let other = io::Error::other("broken");
        assert!(<FakeStore as SegmentStore>::is_disk_full(&enospc));
        assert!(!<FakeStore as SegmentStore>::is_disk_full(&other));
        let quota = io::Error::new(io::ErrorKind::QuotaExceeded, "quota");
        assert!(<FakeStore as SegmentStore>::is_disk_full(
            &io::Error::other(Wrapped(quota))
        ));
        assert!(!<FakeStore as SegmentStore>::is_disk_full(
            &io::Error::other(Wrapped(io::Error::other("deep")))
        ));
    }

    /// Each store names a row that doesn't parse from its error, or from an
    /// error that caused it.
    #[test]
    fn stores_name_a_row_that_doesnt_parse() {
        use std::io;

        use nota_store::{RowKey, StoreError};

        use crate::segment::FakeStore;

        let key = RowKey {
            track: 3,
            start: -2,
        };
        let corrupt = || StoreError::CorruptRow {
            session: SessionId::new(1),
            key,
            why: "odd".into(),
        };
        assert_eq!(
            <Store as SegmentStore>::unparsable_row(&corrupt()),
            Some(key)
        );
        assert_eq!(
            <Writer as SegmentStore>::unparsable_row(&corrupt()),
            Some(key)
        );
        assert_eq!(
            <&mut Store as SegmentStore>::unparsable_row(&corrupt()),
            Some(key)
        );
        for other in [StoreError::OutOfRange, StoreError::Corrupt("x".into())] {
            assert_eq!(<Store as SegmentStore>::unparsable_row(&other), None);
        }
        // Any store whose error was caused by one.
        let caused = io::Error::other(WrappedStore(corrupt()));
        assert_eq!(
            <FakeStore as SegmentStore>::unparsable_row(&caused),
            Some(key)
        );
        let plain = io::Error::other("broken");
        assert_eq!(<FakeStore as SegmentStore>::unparsable_row(&plain), None);
    }

    /// A store error, as the cause of another.
    #[derive(Debug)]
    struct WrappedStore(nota_store::StoreError);

    impl std::fmt::Display for WrappedStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("wrapped store error")
        }
    }

    impl std::error::Error for WrappedStore {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    /// An error caused by another.
    #[derive(Debug)]
    struct Wrapped(std::io::Error);

    impl std::fmt::Display for Wrapped {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("wrapped")
        }
    }

    impl std::error::Error for Wrapped {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }
}
