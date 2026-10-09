//! The session writer's break handling: what the crash tests don't reach
//! because they never fail an operation without crashing.

use std::error::Error as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use nota_core::{
    Clock, EpochId, FakeClock, SampleCount, SampleIndex, SampleRate, SessionId, SessionTime,
    TrackId,
};

use super::*;
use crate::fs::Fs;
use crate::fs::fake::{CrashOutcome, FakeFile, FakeFs, FakeLock, Op};
use crate::journal::read_journal;
use crate::segment::{FakeStore, SegmentLength, SegmentStore, salvage, segment_file_name};

const MIC: TrackId = TrackId::new(0);
const SESSION: SessionId = SessionId::new(1);

fn rate() -> SampleRate {
    SampleRate::new(1_000).unwrap()
}

fn length() -> SegmentLength {
    SegmentLength::new(SampleCount::new(1_000)).unwrap()
}

fn dir() -> PathBuf {
    PathBuf::from("/session")
}

fn session_dir<S: Fs + Clone>(fs: &S) -> SessionDir<S> {
    SessionDir::new(SESSION, fs.clone(), &dir())
}

fn sample(index: u64) -> i16 {
    i16::from_le_bytes([index.to_le_bytes()[0], index.to_le_bytes()[1]])
}

fn samples(from: u64, len: u64) -> Vec<i16> {
    (from..from + len).map(sample).collect()
}

/// The finished journals numbered `ids`.
fn finished(ids: &[u64]) -> Vec<FinishedJournal> {
    ids.iter()
        .map(|&n| FinishedJournal::new(SESSION, JournalId::new(n)))
        .collect()
}

fn clock() -> (Arc<FakeClock>, Arc<dyn Clock>) {
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let dyn_clock: Arc<dyn Clock> = Arc::clone(&clock) as Arc<dyn Clock>;
    (clock, dyn_clock)
}

/// The audio salvage publishes from `fs`: each segment's range and
/// whether its samples are the ones recorded.
fn salvaged(fs: &FakeFs) -> Vec<(u64, u64)> {
    let mut store = FakeStore::new(fs, Path::new("/db"));
    salvage(
        &mut SessionStore::new(session_dir(fs).lock().unwrap(), &mut store),
        length(),
    )
    .unwrap();
    let rows = store.rows(SESSION).unwrap();
    let mut out = Vec::new();
    for row in rows {
        let bytes = fs
            .read(&dir().join(segment_file_name(row.track(), row.range())))
            .unwrap();
        let mut reader = claxon::FlacReader::new(io::Cursor::new(bytes)).unwrap();
        let got: Vec<i16> = reader
            .samples()
            .map(|s| i16::try_from(s.unwrap()).unwrap())
            .collect();
        let r = row.range();
        assert_eq!(got, samples(r.start().get(), r.len().get()), "{r:?}");
        out.push((r.start().get(), r.end().get()));
    }
    out
}

/// Merges touching ranges.
fn joined(ranges: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &(s, e) in ranges {
        match out.last_mut() {
            Some(last) if last.1 == s => last.1 = e,
            _ => out.push((s, e)),
        }
    }
    out
}

/// The index of the `n`th operation matching `wanted` in a clean run of
/// `run`.
fn nth_op(run: impl Fn(&FakeFs), n: usize, wanted: impl Fn(&Op) -> bool) -> usize {
    let fs = FakeFs::with_dirs([dir(), PathBuf::from("/db")]);
    run(&fs);
    fs.ops()
        .iter()
        .enumerate()
        .filter(|(_, op)| wanted(op))
        .nth(n)
        .map(|(i, _)| i)
        .unwrap()
}

/// 2,500 samples in uneven chunks with time passing: rotations at 1,000 and
/// 2,000, and timed syncs between.
fn record(fs: &FakeFs) -> Result<(), Box<dyn std::error::Error>> {
    let (clock, dyn_clock) = clock();
    let mut w = SessionWriter::open(
        &session_dir(fs).lock().unwrap(),
        rate(),
        length(),
        dyn_clock,
    )?;
    w.start_track(MIC, EpochId::new(0), SampleIndex::ZERO)?;
    let mut at = 0;
    for len in [300_u64, 300, 300, 300, 300, 300, 700] {
        w.append(MIC, &samples(at, len))?;
        at += len;
        clock.advance(SampleCount::new(len).duration_at(rate()).unwrap());
        w.sync_if_due()?;
    }
    w.finish()?;
    Ok(())
}

#[test]
fn a_failed_fsync_at_a_rotation_is_replayed_into_a_new_journal() {
    // The sync that ends the first journal, at sample 1,000.
    let journal0 = dir().join(JournalId::FIRST.file_name());
    let at = nth_op(
        |fs| record(fs).unwrap(),
        1,
        |op| matches!(op, Op::Sync(p) if *p == journal0),
    );
    for outcome in [CrashOutcome::LoseUnsynced, CrashOutcome::KeepAll] {
        let fs = FakeFs::with_dirs([dir(), PathBuf::from("/db")]);
        fs.fail_after(at, io::ErrorKind::Other);
        record(&fs).unwrap();
        // The failed fsync dropped journal 0's unsynced tail; the
        // replacement holds it, so nothing is lost.
        let disk = fs.crash(outcome);
        assert_eq!(joined(&salvaged(&disk)), [(0, 2_500)], "{outcome:?}");
    }
}

#[test]
fn a_failed_timed_sync_is_replayed_into_a_new_journal() {
    let at = nth_op(|fs| record(fs).unwrap(), 2, |op| matches!(op, Op::Sync(_)));
    let fs = FakeFs::with_dirs([dir(), PathBuf::from("/db")]);
    fs.fail_after(at, io::ErrorKind::Other);
    record(&fs).unwrap();
    let journals = fs
        .paths()
        .into_iter()
        .filter(|p| p.file_name().and_then(JournalId::from_file_name).is_some())
        .count();
    // One more journal than the three windows need.
    assert_eq!(journals, 4);
    assert_eq!(
        joined(&salvaged(&fs.crash(CrashOutcome::LoseUnsynced))),
        [(0, 2_500)]
    );
}

/// A filesystem that refuses to create files while told to.
#[derive(Debug, Clone)]
struct NoCreates {
    inner: FakeFs,
    refuse: Arc<AtomicBool>,
}

impl Fs for NoCreates {
    type File = FakeFile;
    type Lock = FakeLock;

    fn create(&self, path: &Path) -> io::Result<FakeFile> {
        if self.refuse.load(Ordering::SeqCst) {
            return Err(io::Error::new(io::ErrorKind::StorageFull, "full"));
        }
        self.inner.create(path)
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.create_dir(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.rename(from, to)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.inner.sync_dir(dir)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        self.inner.remove(path)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.inner.read(path)
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        self.inner.list(dir)
    }

    fn lock_dir(&self, dir: &Path) -> io::Result<FakeLock> {
        self.inner.lock_dir(dir)
    }
}

/// A filesystem that doesn't say how much space it has says it can't
/// tell, rather than a figure.
#[test]
fn free_space_by_default_says_the_filesystem_cant_tell() {
    let fs = NoCreates {
        inner: FakeFs::with_dirs([dir()]),
        refuse: Arc::new(AtomicBool::new(false)),
    };
    assert_eq!(
        fs.free_space(&dir()).unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
}

/// A filesystem that doesn't pass on fsyncing a file by name, or renaming
/// without replacing, refuses both, and changes nothing.
#[test]
fn syncing_by_name_and_renaming_without_replacing_by_default_refuse() {
    let inner = FakeFs::with_dirs([dir()]);
    inner.create(&dir().join("a")).unwrap();
    let fs = NoCreates {
        inner: inner.clone(),
        refuse: Arc::new(AtomicBool::new(false)),
    };
    assert_eq!(
        fs.sync_file(&dir().join("a")).unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(
        fs.rename_new(&dir().join("a"), &dir().join("b"))
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(inner.paths(), [dir().join("a")]);
}

#[test]
fn when_the_replacement_fails_too_the_samples_are_a_gap_and_recording_goes_on() {
    let inner = FakeFs::with_dirs([dir(), PathBuf::from("/db")]);
    let fs = NoCreates {
        inner: inner.clone(),
        refuse: Arc::new(AtomicBool::new(false)),
    };
    let (clock, dyn_clock) = clock();
    let mut w = SessionWriter::open(
        &session_dir(&fs).lock().unwrap(),
        rate(),
        length(),
        dyn_clock,
    )
    .unwrap();
    w.start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    w.append(MIC, &samples(0, 300)).unwrap();
    clock.advance(std::time::Duration::from_secs(1));
    w.sync_if_due().unwrap();
    assert_eq!(w.durable(MIC).map(|d| d.end().get()), Some(300));
    // Unsynced samples, then a write that fails, and no room for a new
    // journal: 300..700 can't be kept.
    w.append(MIC, &samples(300, 200)).unwrap();
    inner.fail_after(0, io::ErrorKind::Other);
    fs.refuse.store(true, Ordering::SeqCst);
    let err = w.append(MIC, &samples(500, 200)).unwrap_err();
    assert!(matches!(err, SessionError::Journal(_)), "{err}");
    // The track still moved on, so later samples keep their numbers.
    assert_eq!(w.next_sample(MIC), Some(SampleIndex::new(700)));
    assert_eq!(w.durable(MIC), None);
    // Room again: the next samples start a new journal.
    fs.refuse.store(false, Ordering::SeqCst);
    w.append(MIC, &samples(700, 600)).unwrap();
    let ended = w.finish().unwrap();
    // Ids 1 and 2 went to the journal that couldn't be created, tried twice
    // for want of space: ids are in order, not dense.
    assert_eq!(ended, finished(&[0, 3, 4]));
    // 300..500 was written but never synced: kept by salvage if it
    // survives, lost if the crash drops it.
    let kept = inner.copy_disk();
    assert_eq!(joined(&salvaged(&kept)), [(0, 500), (700, 1_300)]);
    let lost = inner.crash(CrashOutcome::LoseUnsynced);
    assert_eq!(joined(&salvaged(&lost)), [(0, 300), (700, 1_300)]);
}

#[test]
fn a_new_journal_whose_first_write_fails_is_replaced() {
    let fs = FakeFs::with_dirs([dir(), PathBuf::from("/db")]);
    let (_, dyn_clock) = clock();
    let mut w = SessionWriter::open(
        &session_dir(&fs).lock().unwrap(),
        rate(),
        length(),
        dyn_clock,
    )
    .unwrap();
    w.start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    // The marks' write (0-4), then the journal's create (5), header write
    // (6), sync (7), directory sync (8), then the first frame write (9)
    // fails: one replacement takes the samples over.
    fs.fail_after(9, io::ErrorKind::Other);
    w.append(MIC, &samples(0, 100)).unwrap();
    assert_eq!(w.next_sample(MIC), Some(SampleIndex::new(100)));
    let durable = w.durable(MIC).unwrap();
    assert_eq!(
        (durable.journal(), durable.end().get()),
        (JournalId::new(1), 0)
    );
    // The broken journal is held back until its replacement ends, so both
    // are published together.
    assert!(w.take_finished().is_empty());
    w.append(MIC, &samples(100, 100)).unwrap();
    assert_eq!(w.finish().unwrap(), finished(&[0, 1]));
    // Journal 0 holds only its header, and is deleted as empty.
    assert_eq!(
        joined(&salvaged(&fs.crash(CrashOutcome::LoseUnsynced))),
        [(0, 200)]
    );
}

#[test]
fn a_replacement_replays_only_what_wasnt_synced() {
    let fs = FakeFs::with_dirs([dir(), PathBuf::from("/db")]);
    let (clock, dyn_clock) = clock();
    let mut w = SessionWriter::open(
        &session_dir(&fs).lock().unwrap(),
        rate(),
        length(),
        dyn_clock,
    )
    .unwrap();
    w.start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    w.append(MIC, &samples(0, 300)).unwrap();
    clock.advance(std::time::Duration::from_secs(1));
    w.sync_if_due().unwrap();
    w.append(MIC, &samples(300, 200)).unwrap();
    // The next frame write fails: the replacement starts at 300 and holds
    // 300..600, and nothing from before 300.
    fs.fail_after(0, io::ErrorKind::Other);
    w.append(MIC, &samples(500, 100)).unwrap();
    let replacement = fs.read(&dir().join(JournalId::new(1).file_name())).unwrap();
    let read = read_journal(&replacement);
    assert_eq!(
        read.audio(),
        Some((
            nota_core::SampleRange::new(SampleIndex::new(300), SampleIndex::new(600)).unwrap(),
            samples(300, 300)
        ))
    );
    w.finish().unwrap();
    assert_eq!(
        joined(&salvaged(&fs.crash(CrashOutcome::LoseUnsynced))),
        [(0, 600)]
    );
}

#[test]
fn track_errors() {
    let fs = FakeFs::with_dirs([dir()]);
    let (_, dyn_clock) = clock();
    let mut w = SessionWriter::open(
        &session_dir(&fs).lock().unwrap(),
        rate(),
        length(),
        dyn_clock,
    )
    .unwrap();
    assert!(matches!(
        w.append(MIC, &[1]),
        Err(SessionError::UnknownTrack(MIC))
    ));
    assert!(matches!(
        w.new_epoch(MIC, EpochId::new(1)),
        Err(SessionError::UnknownTrack(MIC))
    ));
    w.start_track(MIC, EpochId::new(0), SampleIndex::new(u64::MAX - 1))
        .unwrap();
    assert!(matches!(
        w.start_track(MIC, EpochId::new(0), SampleIndex::ZERO),
        Err(SessionError::TrackExists(MIC))
    ));
    assert!(matches!(
        w.append(MIC, &[1, 2]),
        Err(SessionError::Overflow)
    ));
    assert_eq!(w.next_sample(MIC), Some(SampleIndex::new(u64::MAX - 1)));
    assert!(fs.paths().is_empty());
    assert_eq!(
        SessionDir::new(SESSION, fs, Path::new("/missing"))
            .lock()
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
    assert!(SessionError::Io(io::Error::other("x")).source().is_some());
    assert!(
        SessionError::Journal(JournalError::Broken)
            .source()
            .is_some()
    );
    assert!(SessionError::Overflow.source().is_none());
    for e in [
        SessionError::UnknownTrack(MIC),
        SessionError::TrackExists(MIC),
        SessionError::Overflow,
        SessionError::LastJournalId(PathBuf::from("/s/journal-x")),
        SessionError::Io(io::Error::other("x")),
        SessionError::Journal(JournalError::Broken),
        SessionError::InUse(Use::Recording),
        SessionError::InUse(Use::Salvaging),
        SessionError::InUse(Use::Publishing),
        SessionError::Marks(io::Error::other("x")),
        SessionError::EpochUsed {
            track: MIC,
            epoch: EpochId::new(1),
        },
        SessionError::Covered {
            track: MIC,
            first_free: SampleIndex::new(5),
        },
    ] {
        assert!(!e.to_string().is_empty());
    }
}

#[test]
fn the_last_journal_id_is_refused_naming_the_file() {
    for name in [
        JournalId::new(u64::MAX).file_name(),
        format!("{}.unreadable", JournalId::new(u64::MAX).file_name()),
    ] {
        let fs = FakeFs::with_dirs([dir()]);
        let path = dir().join(&name);
        let _ = fs.create(&path).unwrap();
        let refused = SessionWriter::open(
            &session_dir(&fs).lock().unwrap(),
            rate(),
            length(),
            clock().1,
        );
        let Err(SessionError::LastJournalId(named)) = refused else {
            panic!("{name}: {refused:?}");
        };
        assert_eq!(named, path);
        assert!(
            SessionError::LastJournalId(named)
                .to_string()
                .contains(&name),
            "{name}"
        );
    }
}

#[test]
fn sync_if_due_reports_durable_positions_per_journal() {
    let fs = FakeFs::with_dirs([dir()]);
    let (clock, dyn_clock) = clock();
    let mut w = SessionWriter::open(
        &session_dir(&fs).lock().unwrap(),
        rate(),
        length(),
        dyn_clock,
    )
    .unwrap();
    w.start_track(MIC, EpochId::new(0), SampleIndex::new(1_900))
        .unwrap();
    w.append(MIC, &samples(1_900, 50)).unwrap();
    assert_eq!(w.durable(MIC).map(|d| d.end().get()), Some(1_900));
    clock.advance(std::time::Duration::from_secs(1));
    w.sync_if_due().unwrap();
    let first = w.durable(MIC).unwrap();
    assert_eq!(
        (first.journal(), first.end().get()),
        (JournalId::new(0), 1_950)
    );
    // Crossing 2,000 ends journal 0 and starts journal 1 there.
    w.append(MIC, &samples(1_950, 100)).unwrap();
    let second = w.durable(MIC).unwrap();
    assert_eq!(
        (second.journal(), second.end().get()),
        (JournalId::new(1), 2_000)
    );
    assert_eq!(w.take_finished(), finished(&[0]));
    assert!(w.take_finished().is_empty());
    let read = read_journal(&fs.read(&dir().join(JournalId::new(1).file_name())).unwrap());
    assert_eq!(
        read.header().map(JournalHeader::id),
        Some(JournalId::new(1))
    );
}

#[test]
fn a_failed_finish_still_hands_out_its_journals() {
    // The last fsync fails and the replacement can't be created: `finish`
    // fails, but the journal it ended is still handed out for publishing.
    let inner = FakeFs::with_dirs([dir(), PathBuf::from("/db")]);
    let fs = NoCreates {
        inner: inner.clone(),
        refuse: Arc::new(AtomicBool::new(false)),
    };
    let (_, dyn_clock) = clock();
    let mut w = SessionWriter::open(
        &session_dir(&fs).lock().unwrap(),
        rate(),
        length(),
        dyn_clock,
    )
    .unwrap();
    w.start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    w.append(MIC, &samples(0, 100)).unwrap();
    inner.fail_after(0, io::ErrorKind::Other);
    fs.refuse.store(true, Ordering::SeqCst);
    let err = w.finish().unwrap_err();
    assert!(matches!(err.error(), SessionError::Journal(_)), "{err}");
    assert_eq!(err.to_string(), err.error().to_string());
    assert!(err.source().is_some());
    assert_eq!(err.into_finished(), finished(&[0]));
}

// ---------------------------------------------------------------------------
// Fsyncs that complete later than they start: `Syncing::Manual` holds each
// until the test runs it, as a slow disk or a sync thread would.

/// A writer whose fsyncs wait for the test, recording `MIC` from 0.
fn manual_writer(fs: &FakeFs) -> (Arc<FakeClock>, SessionWriter<FakeFs>) {
    let (clock, dyn_clock) = clock();
    let mut w = SessionWriter::open(
        &session_dir(fs).lock().unwrap(),
        rate(),
        length(),
        dyn_clock,
    )
    .unwrap()
    .with_syncing(Syncing::Manual);
    w.start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    (clock, w)
}

fn durable_end(w: &SessionWriter<FakeFs>) -> u64 {
    w.durable(MIC).unwrap().end().get()
}

/// The range of journal `id`'s valid frames, and its epoch.
fn journal_range(fs: &FakeFs, id: u64) -> (u64, u64, u32) {
    let read = read_journal(
        &fs.read(&dir().join(JournalId::new(id).file_name()))
            .unwrap(),
    );
    let range = read.range().unwrap();
    (
        range.start().get(),
        range.end().get(),
        read.header().unwrap().epoch().get(),
    )
}

#[test]
fn a_full_journal_holds_the_rest_back_until_its_fsync_completes() {
    let fs = FakeFs::with_dirs([dir(), PathBuf::from("/db")]);
    let (_, mut w) = manual_writer(&fs);
    // The budget (850 samples at 1 kHz) fills, its fsync starts and is held.
    w.append(MIC, &samples(0, 850)).unwrap();
    w.append(MIC, &samples(850, 100)).unwrap();
    assert_eq!(w.waiting(MIC), 100);
    assert_eq!(w.next_sample(MIC), Some(SampleIndex::new(950)));
    assert_eq!(
        journal_range(&fs, 0).1,
        850,
        "nothing past the budget written"
    );
    assert_eq!(durable_end(&w), 0);
    // Still held: nothing moves.
    w.sync_if_due().unwrap();
    assert_eq!((w.waiting(MIC), durable_end(&w)), (100, 0));

    assert_eq!(w.run_syncs(MIC, 5), 1, "one fsync was started");
    w.sync_if_due().unwrap();
    assert_eq!(durable_end(&w), 850);
    assert_eq!(w.waiting(MIC), 0);
    assert_eq!(journal_range(&fs, 0).1, 950);
    assert_eq!(w.finish().unwrap(), finished(&[0]));
    assert_eq!(
        joined(&salvaged(&fs.crash(CrashOutcome::LoseUnsynced))),
        [(0, 950)]
    );
}

#[test]
fn a_sync_covers_only_what_was_written_before_it_started() {
    let fs = FakeFs::with_dirs([dir(), PathBuf::from("/db")]);
    let (clock, mut w) = manual_writer(&fs);
    w.append(MIC, &samples(0, 300)).unwrap();
    clock.advance(crate::journal::SYNC_INTERVAL);
    w.sync_if_due().unwrap();
    // Appended while the fsync is held: it doesn't cover these.
    w.append(MIC, &samples(300, 200)).unwrap();
    assert_eq!(w.run_syncs(MIC, 5), 1);
    w.sync_if_due().unwrap();
    assert_eq!(durable_end(&w), 300);
    // None due yet: the interval runs from when that fsync started.
    assert_eq!(w.run_syncs(MIC, 5), 0);
    clock.advance(crate::journal::SYNC_INTERVAL);
    w.sync_if_due().unwrap();
    assert_eq!(w.run_syncs(MIC, 5), 1);
    w.sync_if_due().unwrap();
    assert_eq!(durable_end(&w), 500);
}

#[test]
fn an_ended_journal_is_handed_out_once_its_last_fsync_completes() {
    let fs = FakeFs::with_dirs([dir(), PathBuf::from("/db")]);
    let (_, mut w) = manual_writer(&fs);
    w.append(MIC, &samples(0, 1_100)).unwrap();
    // 850 written; their fsync is held, so the rest waits.
    assert_eq!(w.waiting(MIC), 250);
    assert_eq!(w.run_syncs(MIC, 5), 1);
    w.sync_if_due().unwrap();
    // The window's last 150 end journal 0, whose last fsync is held; the
    // next 100 start journal 1.
    assert_eq!(w.waiting(MIC), 0);
    assert_eq!(journal_range(&fs, 1), (1_000, 1_100, 0));
    assert!(w.take_finished().is_empty());
    // What's known durable is journal 0's, not journal 1's start.
    let durable = w.durable(MIC).unwrap();
    assert_eq!(
        (durable.journal(), durable.end().get()),
        (JournalId::FIRST, 850)
    );

    assert_eq!(w.run_syncs(MIC, 5), 1);
    w.sync_if_due().unwrap();
    assert_eq!(w.take_finished(), finished(&[0]));
    let durable = w.durable(MIC).unwrap();
    assert_eq!(
        (durable.journal(), durable.end().get()),
        (JournalId::new(1), 1_000)
    );
    assert_eq!(w.finish().unwrap(), finished(&[1]));
}

#[test]
fn a_last_fsync_failing_after_the_next_journal_started_is_replayed_and_handed_out_together() {
    let fs = FakeFs::with_dirs([dir(), PathBuf::from("/db")]);
    let (_, mut w) = manual_writer(&fs);
    w.append(MIC, &samples(0, 1_100)).unwrap();
    assert_eq!(w.run_syncs(MIC, 1), 1);
    w.sync_if_due().unwrap();
    // Journal 0's last fsync (850..1,000) fails, after journal 1 started.
    fs.fail_after(0, io::ErrorKind::Other);
    assert_eq!(w.run_syncs(MIC, 1), 1);
    w.sync_if_due().unwrap();
    // Journal 2 takes over 850..1,000 and is ended in its place.
    assert_eq!(journal_range(&fs, 2), (850, 1_000, 0));
    assert!(w.take_finished().is_empty());
    assert_eq!(durable_end(&w), 850);
    assert_eq!(w.run_syncs(MIC, 1), 1);
    w.sync_if_due().unwrap();
    assert_eq!(w.take_finished(), finished(&[0, 2]));
    assert_eq!(durable_end(&w), 1_000);
    assert_eq!(w.finish().unwrap(), finished(&[1]));
    assert_eq!(
        joined(&salvaged(&fs.crash(CrashOutcome::LoseUnsynced))),
        [(0, 1_100)]
    );
}

#[test]
fn a_failed_fsync_found_late_replays_what_was_appended_since() {
    let fs = FakeFs::with_dirs([dir(), PathBuf::from("/db")]);
    let (clock, mut w) = manual_writer(&fs);
    w.append(MIC, &samples(0, 300)).unwrap();
    clock.advance(crate::journal::SYNC_INTERVAL);
    w.sync_if_due().unwrap();
    w.append(MIC, &samples(300, 200)).unwrap();
    fs.fail_after(0, io::ErrorKind::Other);
    assert_eq!(w.run_syncs(MIC, 1), 1);
    w.sync_if_due().unwrap();
    // Journal 1 holds everything since journal 0's durable start.
    assert_eq!(journal_range(&fs, 1), (0, 500, 0));
    let durable = w.durable(MIC).unwrap();
    assert_eq!(
        (durable.journal(), durable.end().get()),
        (JournalId::new(1), 0)
    );
    assert_eq!(w.finish().unwrap(), finished(&[0, 1]));
    assert_eq!(
        joined(&salvaged(&fs.crash(CrashOutcome::LoseUnsynced))),
        [(0, 500)]
    );
}

#[test]
fn a_replacement_failing_too_leaves_a_gap_and_hands_out_both() {
    let fs = FakeFs::with_dirs([dir(), PathBuf::from("/db")]);
    let (clock, mut w) = manual_writer(&fs);
    w.append(MIC, &samples(0, 300)).unwrap();
    clock.advance(crate::journal::SYNC_INTERVAL);
    w.sync_if_due().unwrap();
    // The fsync fails, and so does creating the replacement.
    fs.fail_after(0, io::ErrorKind::Other);
    assert_eq!(w.run_syncs(MIC, 1), 1);
    fs.fail_after(0, io::ErrorKind::Other);
    assert!(matches!(w.sync_if_due(), Err(SessionError::Journal(_))));
    assert_eq!(w.take_finished(), finished(&[0]));
    assert_eq!(w.durable(MIC), None);
    // The next audio starts a new journal after the gap.
    w.append(MIC, &samples(300, 100)).unwrap();
    assert_eq!(journal_range(&fs, 2), (300, 400, 0));
    assert_eq!(w.finish().unwrap(), finished(&[2]));
}

#[test]
fn a_new_epoch_writes_audio_held_back_in_the_old_epoch_without_waiting() {
    let fs = FakeFs::with_dirs([dir(), PathBuf::from("/db")]);
    let (_, mut w) = manual_writer(&fs);
    w.append(MIC, &samples(0, 900)).unwrap();
    assert_eq!(w.waiting(MIC), 50);
    // The full journal ends with its fsync still held; the 50 go to
    // another journal in the old epoch, which ends too.
    w.new_epoch(MIC, EpochId::new(1)).unwrap();
    assert_eq!(w.waiting(MIC), 0);
    assert_eq!(w.epoch(MIC), Some((EpochId::new(1), SampleIndex::new(900))));
    w.append(MIC, &samples(900, 10)).unwrap();
    assert_eq!(journal_range(&fs, 0), (0, 850, 0));
    assert_eq!(journal_range(&fs, 1), (850, 900, 0));
    assert_eq!(journal_range(&fs, 2), (900, 910, 1));
    assert!(w.take_finished().is_empty(), "nothing waited for an fsync");
    assert_eq!(w.finish().unwrap(), finished(&[0, 1, 2]));
    assert_eq!(
        joined(&salvaged(&fs.crash(CrashOutcome::LoseUnsynced))),
        [(0, 910)]
    );
}

#[test]
fn a_replacement_of_a_whole_budget_leaves_its_fsync_to_the_sync_thread() {
    let fs = FakeFs::with_dirs([dir(), PathBuf::from("/db")]);
    let (_, mut w) = manual_writer(&fs);
    w.append(MIC, &samples(0, 850)).unwrap();
    // The full budget's fsync fails: journal 1 takes all 850 over, and its
    // own fsync is started, held, not run here.
    fs.fail_after(0, io::ErrorKind::Other);
    assert_eq!(w.run_syncs(MIC, 1), 1);
    let syncs = |fs: &FakeFs| {
        fs.ops()
            .iter()
            .filter(|op| matches!(op, Op::Sync(_)))
            .count()
    };
    w.sync_if_due().unwrap();
    let before = syncs(&fs);
    assert_eq!(journal_range(&fs, 1), (0, 850, 0));
    assert_eq!(durable_end(&w), 0);
    assert_eq!(w.run_syncs(MIC, 1), 1);
    assert_eq!(syncs(&fs), before + 1);
    w.sync_if_due().unwrap();
    assert_eq!(durable_end(&w), 850);
}

#[test]
fn finish_waits_for_every_held_fsync() {
    let fs = FakeFs::with_dirs([dir(), PathBuf::from("/db")]);
    let (_, mut w) = manual_writer(&fs);
    w.append(MIC, &samples(0, 1_900)).unwrap();
    assert_eq!(w.waiting(MIC), 1_050);
    assert_eq!(w.finish().unwrap(), finished(&[0, 1]));
    assert_eq!(
        joined(&salvaged(&fs.crash(CrashOutcome::LoseUnsynced))),
        [(0, 1_900)]
    );
}

#[test]
fn syncs_on_threads_record_the_same_audio_as_inline() {
    for syncing in [Syncing::Inline, Syncing::Threads] {
        let fs = FakeFs::with_dirs([dir(), PathBuf::from("/db")]);
        let (clock, dyn_clock) = clock();
        let mut w = SessionWriter::open(
            &session_dir(&fs).lock().unwrap(),
            rate(),
            length(),
            dyn_clock,
        )
        .unwrap()
        .with_syncing(syncing);
        w.start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
            .unwrap();
        let mut at = 0;
        for len in [300_u64, 300, 300, 300, 300, 300, 700] {
            w.append(MIC, &samples(at, len)).unwrap();
            at += len;
            clock.advance(SampleCount::new(len).duration_at(rate()).unwrap());
            w.sync_if_due().unwrap();
        }
        assert_eq!(w.finish().unwrap(), finished(&[0, 1, 2]), "{syncing:?}");
        assert_eq!(
            joined(&salvaged(&fs.crash(CrashOutcome::LoseUnsynced))),
            [(0, 2_500)],
            "{syncing:?}"
        );
    }
}

/// A [`FakeFs`] whose journals' data fsyncs, through their syncers, always
/// fail, while creating a journal (its header fsync) still works: a disk
/// that takes names but not data.
#[derive(Debug, Clone)]
struct NoDataSyncs(FakeFs);

#[derive(Debug)]
struct NoDataFile(FakeFile);

#[derive(Debug)]
struct FailingSyncer;

impl crate::fs::FileSyncer for FailingSyncer {
    fn sync(&self) -> io::Result<crate::fs::Synced> {
        Err(io::Error::other("EIO"))
    }
}

impl FsFile for NoDataFile {
    type Syncer = FailingSyncer;

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0.write_all(bytes)
    }

    fn sync(&mut self) -> io::Result<crate::fs::Synced> {
        self.0.sync()
    }

    fn syncer(&self) -> io::Result<FailingSyncer> {
        Ok(FailingSyncer)
    }
}

impl Fs for NoDataSyncs {
    type File = NoDataFile;
    type Lock = FakeLock;

    fn create(&self, path: &Path) -> io::Result<NoDataFile> {
        self.0.create(path).map(NoDataFile)
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.0.create_dir(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.0.rename(from, to)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.0.sync_dir(dir)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        self.0.remove(path)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.0.read(path)
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        self.0.list(dir)
    }

    fn lock_dir(&self, dir: &Path) -> io::Result<FakeLock> {
        self.0.lock_dir(dir)
    }
}

#[test]
fn when_no_data_fsync_ever_succeeds_each_failure_is_replaced_once_then_a_gap() {
    for syncing in [Syncing::Inline, Syncing::Manual] {
        let fs = NoDataSyncs(FakeFs::with_dirs([dir(), PathBuf::from("/db")]));
        let (_, dyn_clock) = clock();
        let mut w = SessionWriter::open(
            &session_dir(&fs).lock().unwrap(),
            rate(),
            length(),
            dyn_clock,
        )
        .unwrap()
        .with_syncing(syncing);
        w.start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
            .unwrap();
        // The full budget's fsync fails, and so does its replacement's: a
        // gap, not a replacement of the replacement.
        let mut failed = w.append(MIC, &samples(0, 850)).is_err();
        for _ in 0..3 {
            w.run_syncs(MIC, usize::MAX);
            failed |= w.sync_if_due().is_err();
        }
        assert!(failed, "{syncing:?}: the second failure is reported");
        assert_eq!(w.take_finished(), finished(&[0, 1]), "{syncing:?}");
        // Ending a journal the same way: one replacement, then the error.
        w.append(MIC, &samples(850, 100)).unwrap();
        let err = w.finish().unwrap_err();
        assert!(
            matches!(err.error(), SessionError::Journal(_)),
            "{syncing:?}: {err}"
        );
        assert_eq!(err.into_finished(), finished(&[2, 3]), "{syncing:?}");
    }
}
