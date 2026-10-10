//! Resuming a session: its tracks carry on above the epochs they used, in
//! session time after the audio they recorded, from what the marks and
//! journals kept.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nota_core::{
    Clock, Epoch, EpochAnchor, EpochError, EpochId, FakeClock, SampleCount, SampleIndex,
    SampleRate, SessionId, SessionTime, TrackId, TrackTimeline,
};

use super::*;
use crate::fs::fake::FakeFs;
use crate::fs::{Fs, FsFile};
use crate::journal::read_journal;
use crate::segment::{FakeStore, SegmentLength, salvage};

const MIC: TrackId = TrackId::new(0);
const SESSION: SessionId = SessionId::new(1);

/// 1 kHz: a sample is a millisecond.
fn rate() -> SampleRate {
    SampleRate::new(1_000).unwrap()
}

fn length() -> SegmentLength {
    SegmentLength::new(SampleCount::new(1_000)).unwrap()
}

fn dir() -> PathBuf {
    PathBuf::from("/session")
}

fn ms(ms: u64) -> SessionTime {
    SessionTime::from_nanos(ms * 1_000_000)
}

fn s(index: u64) -> SampleIndex {
    SampleIndex::new(index)
}

/// A writer on the session in `fs`, timed by a fake clock at `now`.
fn writer_at(fs: &FakeFs, now: SessionTime) -> (SessionLock<FakeFs>, Arc<FakeClock>) {
    let lock = SessionDir::new(SESSION, fs.clone(), &dir()).lock().unwrap();
    (lock, Arc::new(FakeClock::new(now)))
}

/// Records `MIC` as an earlier run would: epoch 0 from 0 s, 1,500 samples
/// (to 1.5 s), then epoch 1 opened at 4 s, 500 more (to 4.5 s). Leaves
/// its journals unpublished and returns the timeline it was timed by.
fn record_earlier(fs: &FakeFs) -> TrackTimeline {
    let (lock, clock) = writer_at(fs, SessionTime::ZERO);
    let mut writer = SessionWriter::open(
        &lock,
        rate(),
        length(),
        Arc::clone(&clock) as Arc<dyn Clock>,
    )
    .unwrap();
    let (mut timeline, epoch) = writer.open_first_epoch(MIC, clock.now()).unwrap();
    writer.start_track(MIC, &epoch).unwrap();
    writer.append(MIC, &[1; 1_500]).unwrap();
    timeline.open_epoch(ms(4_000), s(1_500), rate()).unwrap();
    writer.new_epoch(MIC, timeline.current().unwrap()).unwrap();
    writer.append(MIC, &[2; 500]).unwrap();
    writer.finish().unwrap();
    timeline
}

/// Publishes every journal in `fs`, as salvage at start does.
fn publish_all(fs: &FakeFs) {
    let mut store = FakeStore::new(fs, Path::new("/db"));
    let lock = SessionDir::new(SESSION, fs.clone(), &dir()).lock().unwrap();
    salvage(&mut SessionStore::new(lock, &mut store), length()).unwrap();
}

/// A resumed session's track carries on through the timeline and the
/// writer: its new epoch is above every earlier one, starts after the
/// earlier audio ends, and times its samples from where the resumed clock
/// opened it. Whether the earlier journals are still there or were
/// published, the marks say the same.
#[test]
fn a_resumed_track_carries_on_above_its_epochs_and_after_its_audio() {
    for published in [false, true] {
        let fs = FakeFs::with_dirs([dir(), PathBuf::from("/db")]);
        let earlier = record_earlier(&fs);
        if published {
            publish_all(&fs);
            assert!(
                fs.paths()
                    .iter()
                    .all(|p| !p.to_string_lossy().contains("journal"))
            );
        }
        let (lock, _) = writer_at(&fs, SessionTime::ZERO);
        let probe =
            SessionWriter::open(&lock, rate(), length(), Arc::new(FakeClock::default())).unwrap();
        // The earlier audio ends 500 samples into epoch 1, at 4.5 s.
        assert_eq!(probe.highest_epoch(MIC), Some(EpochId::new(1)));
        assert_eq!(probe.first_free_sample(MIC), s(2_000));
        assert_eq!(probe.earlier_end(MIC), Some(ms(4_500)));
        assert_eq!(probe.resume_from(), ms(4_500));
        let resumed_from = probe.resume_from();
        drop(probe);

        // The clock resumes there; the stream's first audio is 2 s later.
        let clock = Arc::new(FakeClock::new(resumed_from));
        clock.advance(Duration::from_secs(2));
        let mut writer = SessionWriter::open(
            &lock,
            rate(),
            length(),
            Arc::clone(&clock) as Arc<dyn Clock>,
        )
        .unwrap();
        let (mut timeline, epoch) = writer.open_first_epoch(MIC, clock.now()).unwrap();
        assert_eq!(
            (epoch.id(), epoch.start(), epoch.first_sample()),
            (EpochId::new(2), ms(6_500), s(2_000)),
            "published: {published}"
        );
        writer.start_track(MIC, &epoch).unwrap();
        writer.append(MIC, &[3; 100]).unwrap();
        // A later reopening in the same run follows on from it.
        timeline.open_epoch(ms(7_000), s(2_100), rate()).unwrap();
        writer.new_epoch(MIC, timeline.current().unwrap()).unwrap();
        let journals = writer.finish().unwrap();

        // Its samples map through it; the earlier epoch it carried on from
        // times the earlier audio as before, and the stretch between is a
        // gap, as is the one before the later reopening.
        assert_eq!(timeline.time_of(s(2_000)), Some(ms(6_500)));
        assert_eq!(timeline.time_of(s(2_050)), Some(ms(6_550)));
        assert_eq!(timeline.time_of(s(1_999)), earlier.time_of(s(1_999)));
        let gaps: Vec<_> = timeline.gaps().map(|g| (g.from(), g.to())).collect();
        assert_eq!(gaps, [(ms(4_500), ms(6_500)), (ms(6_600), ms(7_000))]);
        // Its journal is stamped with the new epoch's anchor.
        let headers: Vec<_> = journals
            .iter()
            .map(|j| {
                let bytes = fs.read(&dir().join(j.id().file_name())).unwrap();
                read_journal(&bytes).header().unwrap().anchor()
            })
            .collect();
        assert_eq!(headers, [Some(epoch.anchor())]);
    }
}

/// A track can't start before its earlier audio ends, as it would with a
/// clock started at zero again: the timeline refuses the epoch, and so
/// does the writer.
#[test]
fn a_track_resumed_on_a_clock_that_wasnt_is_refused() {
    let fs = FakeFs::with_dirs([dir()]);
    let earlier = record_earlier(&fs);
    let (lock, _) = writer_at(&fs, SessionTime::ZERO);
    let mut writer =
        SessionWriter::open(&lock, rate(), length(), Arc::new(FakeClock::default())).unwrap();
    assert_eq!(
        writer.open_first_epoch(MIC, ms(1_000)).map(|(_, e)| e.id()),
        Err(EpochError::ImplausibleOverrun {
            previous_end: ms(4_500),
            start: ms(1_000)
        })
    );
    // An epoch built on a timeline that forgot the earlier ones.
    let mut fresh = TrackTimeline::starting_after(MIC, EpochId::new(1)).unwrap();
    fresh.open_epoch(ms(4_499), s(2_000), rate()).unwrap();
    let early: Epoch = *fresh.current().unwrap();
    assert!(matches!(
        writer.start_track(MIC, &early),
        Err(SessionError::TimeWentBack { track: MIC, earlier_end }) if earlier_end == ms(4_500)
    ));
    let text = SessionError::TimeWentBack {
        track: MIC,
        earlier_end: ms(4_500),
    }
    .to_string();
    assert!(text.contains("4500000000 ns"), "{text}");
    // At the end exactly, it may.
    fresh = TrackTimeline::starting_after(MIC, EpochId::new(1)).unwrap();
    fresh.open_epoch(ms(4_500), s(2_000), rate()).unwrap();
    writer.start_track(MIC, fresh.current().unwrap()).unwrap();
    assert_eq!(earlier.epochs().len(), 2);
}

/// A track whose newest epoch kept no audio ends where that epoch starts.
#[test]
fn an_epoch_with_no_audio_kept_ends_the_track_at_its_start() {
    let fs = FakeFs::with_dirs([dir()]);
    let (lock, clock) = writer_at(&fs, SessionTime::ZERO);
    let mut writer = SessionWriter::open(
        &lock,
        rate(),
        length(),
        Arc::clone(&clock) as Arc<dyn Clock>,
    )
    .unwrap();
    let (mut timeline, epoch) = writer.open_first_epoch(MIC, ms(0)).unwrap();
    writer.start_track(MIC, &epoch).unwrap();
    writer.append(MIC, &[1; 100]).unwrap();
    // Epoch 1 opens at 9 s and its journal starts, but its audio is never
    // kept: the journal holds its header only.
    timeline.open_epoch(ms(9_000), s(100), rate()).unwrap();
    writer.new_epoch(MIC, timeline.current().unwrap()).unwrap();
    writer.append(MIC, &[2; 10]).unwrap();
    writer.finish().unwrap();
    let journals: Vec<PathBuf> = fs
        .paths()
        .into_iter()
        .filter(|p| p.to_string_lossy().contains("journal"))
        .collect();
    let last = journals.last().unwrap();
    let bytes = fs.read(last).unwrap();
    let header_len = read_journal(&bytes).header().unwrap().encoded_len();
    fs.remove(last).unwrap();
    let mut cut = fs.create(last).unwrap();
    cut.write_all(&bytes[..header_len]).unwrap();
    drop(lock);

    let (lock, _) = writer_at(&fs, SessionTime::ZERO);
    let writer =
        SessionWriter::open(&lock, rate(), length(), Arc::new(FakeClock::default())).unwrap();
    assert_eq!(writer.first_free_sample(MIC), s(100));
    assert_eq!(writer.earlier_end(MIC), Some(ms(9_000)));
}

/// A crash can keep a new epoch's anchor and lose the end of the epoch
/// before it: the track still resumes, numbered above the anchor's epoch,
/// from the sample it holds up to, and no earlier than that anchor's start.
#[test]
fn a_track_resumes_when_its_newest_anchor_is_past_its_audio() {
    let fs = FakeFs::with_dirs([dir()]);
    // Epoch 0 kept samples up to 1,000; epoch 1, opened at 5 s from sample
    // 1,200, was marked, but its journal and the 200 samples before it
    // were lost.
    let mut marks = fs.create(&dir().join(MARKS_FILE_NAME)).unwrap();
    marks
        .write_all(b"nota session marks 2\njournals-below 64\nepoch 0 1 1200 1000 5000000000\n")
        .unwrap();
    let mut journal = fs
        .create(&dir().join(JournalId::new(3).file_name()))
        .unwrap();
    let header = JournalHeader::new(
        JournalId::new(3),
        MIC,
        EpochAnchor {
            id: EpochId::new(0),
            start: ms(0),
            first_sample: s(0),
            rate: rate(),
        },
    );
    let mut bytes = crate::journal::format::encode_header(header);
    crate::journal::format::encode_frame(&mut bytes, 0, MIC, s(0), &[1; 1_000]);
    journal.write_all(&bytes).unwrap();
    let (lock, _) = writer_at(&fs, SessionTime::ZERO);
    let mut writer =
        SessionWriter::open(&lock, rate(), length(), Arc::new(FakeClock::default())).unwrap();
    assert_eq!(writer.first_free_sample(MIC), s(1_000));
    assert_eq!(writer.earlier_end(MIC), Some(ms(5_000)));
    assert_eq!(writer.resume_from(), ms(5_000));
    // Too early is refused by the writer, as for any resumed track.
    let (_, early) = writer.open_first_epoch(MIC, ms(4_000)).unwrap();
    assert!(matches!(
        writer.start_track(MIC, &early),
        Err(SessionError::TimeWentBack { track: MIC, .. })
    ));
    let (_, epoch) = writer.open_first_epoch(MIC, ms(6_000)).unwrap();
    assert_eq!(
        (epoch.id(), epoch.start(), epoch.first_sample()),
        (EpochId::new(2), ms(6_000), s(1_000))
    );
    writer.start_track(MIC, &epoch).unwrap();
    writer.append(MIC, &[2; 10]).unwrap();
    writer.finish().unwrap();
}

/// Marks from an older nota name the epoch but not its time: the track
/// carries on above it, untimed, and nothing holds the clock back.
#[test]
fn untimed_marks_carry_on_above_their_epoch() {
    let fs = FakeFs::with_dirs([dir()]);
    let mut file = fs.create(&dir().join(MARKS_FILE_NAME)).unwrap();
    file.write_all(b"nota session marks 1\njournals-below 64\nepoch 0 3\n")
        .unwrap();
    let (lock, _) = writer_at(&fs, SessionTime::ZERO);
    let mut writer =
        SessionWriter::open(&lock, rate(), length(), Arc::new(FakeClock::default())).unwrap();
    assert_eq!(writer.earlier_end(MIC), None);
    assert_eq!(writer.resume_from(), SessionTime::ZERO);
    let timeline = writer.resumed_timeline(MIC).unwrap();
    assert!(timeline.epochs().is_empty());
    let (_, epoch) = writer.open_first_epoch(MIC, ms(0)).unwrap();
    assert_eq!(epoch.id(), EpochId::new(4));
    writer.start_track(MIC, &epoch).unwrap();
    writer.append(MIC, &[1; 10]).unwrap();
    writer.finish().unwrap();
    // The marks now time the track's newest epoch.
    let marks = String::from_utf8(fs.read(&dir().join(MARKS_FILE_NAME)).unwrap()).unwrap();
    assert!(marks.ends_with("epoch 0 4 0 1000 0\n"), "{marks}");
}

/// A journal's anchor times an epoch the marks name untimed, and a newer
/// one in the marks wins over an older journal's.
#[test]
fn the_newest_timed_epoch_is_carried_on_from() {
    let fs = FakeFs::with_dirs([dir()]);
    let mut file = fs.create(&dir().join(MARKS_FILE_NAME)).unwrap();
    file.write_all(b"nota session marks 1\njournals-below 64\nepoch 0 3\n")
        .unwrap();
    let anchor = EpochAnchor {
        id: EpochId::new(3),
        start: ms(2_000),
        first_sample: s(40),
        rate: rate(),
    };
    // Its journal holds 10 samples from the epoch's first, to 2.01 s.
    let mut journal = fs
        .create(&dir().join(JournalId::new(9).file_name()))
        .unwrap();
    let mut bytes =
        crate::journal::format::encode_header(JournalHeader::new(JournalId::new(9), MIC, anchor));
    crate::journal::format::encode_frame(&mut bytes, 0, MIC, s(40), &[1; 10]);
    journal.write_all(&bytes).unwrap();
    let (lock, _) = writer_at(&fs, SessionTime::ZERO);
    let writer =
        SessionWriter::open(&lock, rate(), length(), Arc::new(FakeClock::default())).unwrap();
    assert_eq!(writer.highest_epoch(MIC), Some(EpochId::new(3)));
    let timeline = writer.resumed_timeline(MIC).unwrap();
    assert_eq!(
        timeline
            .epochs()
            .iter()
            .map(Epoch::anchor)
            .collect::<Vec<_>>(),
        [anchor]
    );
    assert_eq!(writer.earlier_end(MIC), Some(ms(2_010)));
    drop(writer);

    // Marks naming epoch 4 timed outrank the journal's epoch 3.
    let newer = EpochAnchor {
        id: EpochId::new(4),
        start: ms(5_000),
        first_sample: s(40),
        rate: rate(),
    };
    fs.remove(&dir().join(MARKS_FILE_NAME)).unwrap();
    let mut file = fs.create(&dir().join(MARKS_FILE_NAME)).unwrap();
    file.write_all(b"nota session marks 2\njournals-below 64\nepoch 0 4 40 1000 5000000000\n")
        .unwrap();
    let writer =
        SessionWriter::open(&lock, rate(), length(), Arc::new(FakeClock::default())).unwrap();
    let timeline = writer.resumed_timeline(MIC).unwrap();
    assert_eq!(
        timeline
            .epochs()
            .iter()
            .map(Epoch::anchor)
            .collect::<Vec<_>>(),
        [newer]
    );
}

/// An epoch at another rate than the writer's, or a new one away from the
/// track's next sample, doesn't fit, and nothing changes.
#[test]
fn an_epoch_that_doesnt_fit_the_track_is_refused() {
    let fs = FakeFs::with_dirs([dir()]);
    let (lock, _) = writer_at(&fs, SessionTime::ZERO);
    let mut writer =
        SessionWriter::open(&lock, rate(), length(), Arc::new(FakeClock::default())).unwrap();
    let mut other_rate = TrackTimeline::new(MIC);
    other_rate
        .open_epoch(ms(0), s(0), SampleRate::SPEECH)
        .unwrap();
    assert!(matches!(
        writer.start_track(MIC, other_rate.current().unwrap()),
        Err(SessionError::EpochMisplaced { track: MIC, epoch }) if epoch == EpochId::new(0)
    ));
    let (mut timeline, epoch) = writer.open_first_epoch(MIC, ms(0)).unwrap();
    writer.start_track(MIC, &epoch).unwrap();
    writer.append(MIC, &[1; 10]).unwrap();
    for (first, hz) in [(9, 1_000), (11, 1_000), (10, 16_000)] {
        let mut later = timeline.clone();
        later
            .open_epoch(ms(100), s(first), SampleRate::new(hz).unwrap())
            .unwrap();
        assert!(
            matches!(
                writer.new_epoch(MIC, later.current().unwrap()),
                Err(SessionError::EpochMisplaced { track: MIC, .. })
            ),
            "{first} {hz}"
        );
    }
    assert_eq!(writer.epoch(MIC), Some(epoch));
    timeline.open_epoch(ms(100), s(10), rate()).unwrap();
    writer.new_epoch(MIC, timeline.current().unwrap()).unwrap();
    let text = SessionError::EpochMisplaced {
        track: MIC,
        epoch: EpochId::new(7),
    }
    .to_string();
    assert!(
        text.contains("epoch 7") && text.contains("track 0"),
        "{text}"
    );
}
