//! Where segment rows are committed.
//!
//! The real store is SQLite ([`nota_store::Store`]), one library database
//! holding every session's rows. Crash tests use `FakeStore`, which keeps
//! rows as files on a `FakeFs`, so a simulated crash covers the rows and the
//! segments together.

use std::error::Error;

use nota_core::SessionId;
use nota_store::SegmentRow;

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

    /// Commits `segment`'s row for `session`, durably, before returning.
    /// Committing the same row again is fine. Takes a [`DurableSegment`], so
    /// a row can only be committed for a file that's durable under its final
    /// name.
    ///
    /// # Errors
    ///
    /// The store's error, including a different row of the session for the
    /// same track and first sample, a row of the same session and track
    /// whose samples overlap this one's, and a session the store doesn't
    /// hold.
    fn insert(&mut self, session: SessionId, segment: &DurableSegment) -> Result<(), Self::Error>;

    /// Whether `error` says the disk is full (see
    /// [`is_disk_full`](crate::fs::is_disk_full)). A store that can't tell
    /// says no.
    fn is_disk_full(error: &Self::Error) -> bool {
        let _ = error;
        false
    }
}

impl SegmentStore for nota_store::Store {
    type Error = nota_store::StoreError;

    fn rows(&mut self, session: SessionId) -> Result<Vec<SegmentRow>, Self::Error> {
        self.segments(session)
    }

    fn insert(&mut self, session: SessionId, segment: &DurableSegment) -> Result<(), Self::Error> {
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
        self.with(|store| store.insert_segment(session, segment.row()))
            .map(|_| ())
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
}

#[cfg(any(test, feature = "fake-fs"))]
pub use fake::FakeStore;

#[cfg(any(test, feature = "fake-fs"))]
mod fake {
    use std::io;
    use std::path::{Path, PathBuf};

    use nota_core::{EpochId, SampleIndex, SampleRange, SessionId, TrackId};
    use nota_store::{SegmentRow, Sha256Digest};

    use super::{DurableSegment, SegmentStore};
    use crate::fs::fake::FakeFs;
    use crate::fs::{Fs, FsFile};

    /// Bytes in a row file: session, track, epoch, start, end, SHA-256, CRC-32.
    const ROW_LEN: usize = 8 + 4 + 4 + 8 + 8 + 32 + 4;

    /// Segment rows as files on a [`FakeFs`], one per row, each published
    /// by temp file, fsync, rename and directory fsync: a commit that's
    /// atomic and durable when it returns, as SQLite's with
    /// `synchronous=FULL`, and that a simulated crash can interrupt. Like
    /// SQLite, it keeps rows by session, and refuses a row whose samples
    /// overlap another row of the same session and track.
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

        fn is_disk_full(error: &io::Error) -> bool {
            crate::fs::is_disk_full(error)
        }

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
            let temp = path.with_extension("tmp");
            match self.fs.remove(&temp) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let mut file = self.fs.create(&temp)?;
            file.write_all(&encode(session, row))?;
            file.sync()?;
            self.fs.rename(&temp, &path)?;
            if let Err(e) = self.fs.sync_dir(&self.dir) {
                // Not committed: don't leave a row a later `rows` would
                // report while its name isn't durable. Best effort.
                let _ = self.fs.remove(&path);
                return Err(e);
            }
            Ok(())
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

    use nota_core::{EpochId, SampleIndex, SampleRange, SessionId, TrackId};
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
}
