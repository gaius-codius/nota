//! Sessions that hold files for two reasons at once, made by real
//! recording, publishing and salvage over the fake filesystem. The tests of
//! each report that names held files (the summary, the startup note, Home)
//! take their sessions from here, so they all judge the same failures.
//!
//! Each session records the mic and the system audio, one short journal
//! each. The mic's journal is fully published but can't be deleted; the
//! system track's audio is held back as [`Beside`] says.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use nota_core::{
    Clock, FakeClock, SampleCount, SampleIndex, SampleRange, SampleRate, SessionId, SessionTime,
    TrackId,
};
use nota_recorder::disk::DiskWatch;
use nota_recorder::fs::fake::{FakeFs, Fault};
use nota_recorder::fs::{Fs as _, FsFile as _};
use nota_recorder::segment::{
    FakeStore, PublishReport, Publisher, SegmentLength, publish_journals, segment_file_name,
};
use nota_recorder::session::{FinishedJournal, SessionDir, SessionStore, SessionWriter};

use super::{Salvaged, salvage_result};

/// The session every scenario records into.
pub(crate) const SESSION: SessionId = SessionId::new(1);
const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);
/// The error every refused operation gives.
const REFUSED: io::ErrorKind = io::ErrorKind::PermissionDenied;

/// What holds the system track's audio back, beside the mic's journal that
/// can't be deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Beside {
    /// A directory is under the name of the system track's segment.
    Blocked,
    /// A file under the system track's segment temp name can't be removed,
    /// so the segment can't be written.
    TempKept,
    /// The system track's journal can't be read.
    Unread,
    /// The system track's segment was published, then changed, and the
    /// changed file can't be moved aside to repair it.
    Unrepaired,
}

impl Beside {
    /// What startup and Home must say of the files salvage holds: each
    /// named once, with its cause, and the mic's journal never called
    /// unpublished.
    pub(crate) fn held_at_start(self) -> [&'static str; 2] {
        let mic = "journal-000000: couldn't delete it (permission denied); \
                   all its audio is published";
        match self {
            Self::Blocked => [
                "seg-t1-000000000000.flac: a directory is in the way; \
                 its audio is still to publish",
                mic,
            ],
            Self::TempKept => [
                "seg-t1-000000000000.flac.tmp: couldn't remove the temporary file \
                 (permission denied); its audio is still to publish",
                mic,
            ],
            Self::Unread => [
                mic,
                "journal-000001: couldn't read it (permission denied); \
                 it's kept, and nota tries again when it starts",
            ],
            Self::Unrepaired => [
                mic,
                "seg-t1-000000000000.flac: the segment file doesn't match its stored \
                 audio; couldn't repair it (permission denied); \
                 its journals are still to publish",
            ],
        }
    }

    /// Every scenario, so a test covers them all.
    pub(crate) const ALL: [Self; 4] = [
        Self::Blocked,
        Self::TempKept,
        Self::Unread,
        Self::Unrepaired,
    ];
}

/// A recorded session on a fake disk, its journals finished and left for
/// publishing, with a [`Beside`]'s failures in place.
pub(crate) struct Held {
    /// The disk holding the session's audio directory and its rows.
    fs: FakeFs,
    /// The journals the recording finished, mic first.
    journals: Vec<FinishedJournal>,
}

impl Held {
    /// Records both tracks and sets up `beside`'s failures.
    pub(crate) fn new(beside: Beside) -> Self {
        let fs = FakeFs::with_dirs([audio(), rows_dir()]);
        let journals = record(&fs);
        // Only deleting fails: the mic's audio reaches a committed row.
        fs.fail_on(
            &audio().join(journals[0].id().file_name()),
            Fault::Remove,
            REFUSED,
        );
        let held = Self { fs, journals };
        let system = audio().join(held.journals[1].id().file_name());
        match beside {
            Beside::Blocked => held.fs.create_dir(&system_segment()).unwrap(),
            Beside::TempKept => {
                let temp = system_temp();
                held.fs
                    .create(&temp)
                    .unwrap()
                    .write_all(b"a crashed run's temp")
                    .unwrap();
                held.fs.fail_on(&temp, Fault::Remove, REFUSED);
            }
            Beside::Unread => held.fs.fail_on(&system, Fault::Read, REFUSED),
            Beside::Unrepaired => held.break_system_segment(),
        }
        held
    }

    /// What startup salvage made of the session, as the library reports
    /// it.
    pub(crate) fn salvaged(&self) -> Salvaged {
        let watch = DiskWatch::new(self.fs.clone());
        let dir = SessionDir::new(SESSION, watch.fs(), &audio());
        let mut bound =
            SessionStore::new(dir.lock().unwrap(), FakeStore::new(&self.fs, &rows_dir()));
        salvage_result(&mut bound, &dir, length(), &watch)
    }

    /// What the live publisher made of the journals when the recording
    /// stopped, as the summary reports it.
    pub(crate) fn published(self) -> PublishReport {
        let publisher = Publisher::spawn(self.store(), length()).unwrap();
        assert!(publisher.queue().send(self.journals));
        publisher.finish().unwrap()
    }

    /// The session's store, locked as a recording or salvage locks it.
    fn store(&self) -> SessionStore<FakeFs, FakeStore<FakeFs>> {
        let lock = SessionDir::new(SESSION, self.fs.clone(), &audio())
            .lock()
            .unwrap();
        SessionStore::new(lock, FakeStore::new(&self.fs, &rows_dir()))
    }

    /// Publishes both tracks, keeping their journals, then changes the
    /// system track's segment and refuses to move it aside: its repair
    /// can't finish, so its journal stays the only good copy.
    fn break_system_segment(&self) {
        // The system journal stays too, so the later run still holds its
        // audio to repair from.
        let system = audio().join(self.journals[1].id().file_name());
        self.fs.fail_on(&system, Fault::Remove, REFUSED);
        let first = publish_journals(&mut self.store(), length(), &self.journals).unwrap();
        assert_eq!(first.segments().len(), 2, "{first:?}");
        let segment = system_segment();
        self.fs.remove(&segment).unwrap();
        self.fs
            .create(&segment)
            .unwrap()
            .write_all(b"changed segment")
            .unwrap();
        self.fs.fail_on(&segment, Fault::Rename, REFUSED);
    }
}

/// The session's audio directory on the fake disk.
fn audio() -> PathBuf {
    PathBuf::from("/data/sessions/1/audio")
}

/// Where the fake store keeps the session's rows.
fn rows_dir() -> PathBuf {
    PathBuf::from("/data/rows")
}

/// The windows the session records and publishes in: longer than either
/// track, so each track's audio is one segment.
fn length() -> SegmentLength {
    SegmentLength::new(SampleCount::new(1_000)).unwrap()
}

/// Where the system track's one segment is published.
pub(super) fn system_segment() -> PathBuf {
    let range = SampleRange::new(SampleIndex::ZERO, SampleIndex::new(50)).unwrap();
    audio().join(segment_file_name(SYSTEM, range))
}

/// The system track's segment temp name, where a crashed run may have
/// left part of it.
pub(super) fn system_temp() -> PathBuf {
    let mut temp = system_segment().into_os_string();
    temp.push(".tmp");
    PathBuf::from(temp)
}

/// Records 50 samples on each track and finishes the journals, mic first.
fn record(fs: &FakeFs) -> Vec<FinishedJournal> {
    let lock = SessionDir::new(SESSION, fs.clone(), &audio())
        .lock()
        .unwrap();
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let rate = SampleRate::new(1_000).unwrap();
    let mut writer = SessionWriter::open(&lock, rate, length(), clock).unwrap();
    for track in [MIC, SYSTEM] {
        let (_, epoch) = writer.open_first_epoch(track, SessionTime::ZERO).unwrap();
        writer.start_track(track, &epoch).unwrap();
        writer.append(track, &[7; 50]).unwrap();
    }
    let mut journals = writer.finish().unwrap();
    journals.sort();
    journals
}
