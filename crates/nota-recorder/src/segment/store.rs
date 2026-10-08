//! Where segment rows are committed.
//!
//! The real store is SQLite ([`nota_store::Store`]). Crash tests use
//! `FakeStore`, which keeps rows as files on a `FakeFs`, so a simulated
//! crash covers the rows and the segments together.

use std::error::Error;

use nota_store::SegmentRow;

use super::publish::DurableSegment;

/// A store of segment rows.
pub trait SegmentStore {
    /// Why an operation failed.
    type Error: Error + Send + Sync + 'static;

    /// Every committed row.
    ///
    /// # Errors
    ///
    /// The store's error.
    fn rows(&mut self) -> Result<Vec<SegmentRow>, Self::Error>;

    /// Commits `segment`'s row, durably, before returning. Committing the
    /// same row again is fine. Takes a [`DurableSegment`], so a row can only
    /// be committed for a file that's durable under its final name.
    ///
    /// # Errors
    ///
    /// The store's error, including a different row for the same track and
    /// first sample, and a row of the same track whose samples overlap this
    /// one's.
    fn insert(&mut self, segment: &DurableSegment) -> Result<(), Self::Error>;
}

impl SegmentStore for nota_store::Store {
    type Error = nota_store::StoreError;

    fn rows(&mut self) -> Result<Vec<SegmentRow>, Self::Error> {
        self.segments()
    }

    fn insert(&mut self, segment: &DurableSegment) -> Result<(), Self::Error> {
        self.insert_segment(segment.row()).map(|_| ())
    }
}

/// A store lent to a [`SessionStore`](crate::session::SessionStore), so the
/// caller keeps it when the session ends.
impl<T: SegmentStore + ?Sized> SegmentStore for &mut T {
    type Error = T::Error;

    fn rows(&mut self) -> Result<Vec<SegmentRow>, Self::Error> {
        (**self).rows()
    }

    fn insert(&mut self, segment: &DurableSegment) -> Result<(), Self::Error> {
        (**self).insert(segment)
    }
}

#[cfg(any(test, feature = "fake-fs"))]
pub use fake::FakeStore;

#[cfg(any(test, feature = "fake-fs"))]
mod fake {
    use std::io;
    use std::path::{Path, PathBuf};

    use nota_core::{EpochId, SampleIndex, SampleRange, TrackId};
    use nota_store::{SegmentRow, Sha256Digest};

    use super::{DurableSegment, SegmentStore};
    use crate::fs::fake::FakeFs;
    use crate::fs::{Fs, FsFile};

    /// Bytes in a row file: track, epoch, start, end, SHA-256, CRC-32.
    const ROW_LEN: usize = 4 + 4 + 8 + 8 + 32 + 4;

    /// Segment rows as files on a [`FakeFs`], one per row, each published
    /// by temp file, fsync, rename and directory fsync: a commit that's
    /// atomic and durable when it returns, as SQLite's with
    /// `synchronous=FULL`, and that a simulated crash can interrupt. Like
    /// SQLite, it refuses a row whose samples overlap another row of the
    /// same track.
    #[derive(Debug, Clone)]
    pub struct FakeStore {
        fs: FakeFs,
        dir: PathBuf,
    }

    impl FakeStore {
        /// A store keeping its rows in `dir` on `fs`. The directory must
        /// exist.
        #[must_use]
        pub fn new(fs: &FakeFs, dir: &Path) -> Self {
            Self {
                fs: fs.clone(),
                dir: dir.to_path_buf(),
            }
        }

        fn row_path(&self, row: &SegmentRow) -> PathBuf {
            self.dir.join(format!(
                "t{}-{:020}.row",
                row.track().get(),
                row.range().start().get()
            ))
        }
    }

    fn encode(row: &SegmentRow) -> Vec<u8> {
        let mut out = Vec::with_capacity(ROW_LEN);
        out.extend_from_slice(&row.track().get().to_le_bytes());
        out.extend_from_slice(&row.epoch().get().to_le_bytes());
        out.extend_from_slice(&row.range().start().get().to_le_bytes());
        out.extend_from_slice(&row.range().end().get().to_le_bytes());
        out.extend_from_slice(row.sha256().as_bytes());
        let crc = crc32fast::hash(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Option<SegmentRow> {
        if bytes.len() != ROW_LEN {
            return None;
        }
        let (body, crc) = bytes.split_at(ROW_LEN - 4);
        if crc32fast::hash(body).to_le_bytes() != crc {
            return None;
        }
        let u32_at = |at: usize| Some(u32::from_le_bytes(body.get(at..at + 4)?.try_into().ok()?));
        let u64_at = |at: usize| Some(u64::from_le_bytes(body.get(at..at + 8)?.try_into().ok()?));
        let range = SampleRange::new(SampleIndex::new(u64_at(8)?), SampleIndex::new(u64_at(16)?))?;
        SegmentRow::new(
            TrackId::new(u32_at(0)?),
            EpochId::new(u32_at(4)?),
            range,
            Sha256Digest::new(body.get(24..56)?.try_into().ok()?),
        )
    }

    fn corrupt(path: &Path) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bad row file {}", path.display()),
        )
    }

    impl SegmentStore for FakeStore {
        type Error = io::Error;

        fn rows(&mut self) -> io::Result<Vec<SegmentRow>> {
            let mut rows = Vec::new();
            for path in self.fs.list(&self.dir)? {
                if path.extension().is_some_and(|e| e == "row") {
                    let bytes = self.fs.read(&path)?;
                    rows.push(decode(&bytes).ok_or_else(|| corrupt(&path))?);
                }
            }
            rows.sort_by_key(|r| (r.track(), r.range().start()));
            Ok(rows)
        }

        fn insert(&mut self, segment: &DurableSegment) -> io::Result<()> {
            let row = segment.row();
            let path = self.row_path(row);
            match self.fs.read(&path) {
                Ok(bytes) if decode(&bytes).as_ref() == Some(row) => return Ok(()),
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
            let overlaps = self.rows()?.iter().any(|other| {
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
            file.write_all(&encode(row))?;
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
            SegmentStore::insert(&mut lent, &durable).unwrap();
            let rows = SegmentStore::rows(&mut lent).unwrap();
            assert_eq!(rows, [*durable.row()]);
            assert_eq!(store.rows().unwrap(), rows);
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
            store.insert(&a).unwrap();
            store.insert(&a).unwrap();
            let before = fs.paths();

            for (start, end) in [(15, 25), (5, 11), (12, 18)] {
                let err = store.insert(&make(1, start, end)).unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{start}..{end}");
            }
            assert_eq!(store.rows().unwrap(), [*a.row()]);
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
            store.insert(&touching_after).unwrap();
            store.insert(&touching_before).unwrap();
            store.insert(&other_track).unwrap();
            assert_eq!(
                store.rows().unwrap(),
                [
                    *touching_before.row(),
                    *a.row(),
                    *touching_after.row(),
                    *other_track.row()
                ]
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
            let bytes = encode(&row);
            assert_eq!(decode(&bytes), Some(row));
            for cut in 0..bytes.len() {
                assert_eq!(decode(&bytes[..cut]), None);
            }
            let mut flipped = bytes;
            flipped[9] ^= 1;
            assert_eq!(decode(&flipped), None);
        }
    }
}
