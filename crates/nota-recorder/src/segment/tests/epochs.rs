//! Epochs' timing through a crash: a track records three epochs, the
//! second retimed for drift straight on from the first and the third after
//! a gap, publishing live, and is crashed after every operation. Whatever
//! survived, salvage leaves each committed segment's epoch timed in the
//! store as it was recorded, drift included, so the track's samples map to
//! session time as before and the gap is still a gap; and a writer
//! reopening the session would resume its clock after the audio it kept.

use std::cell::Cell;

use nota_core::{Drift, Epoch, TrackTimeline};

use super::*;

/// When the second epoch opens, retimed for a device measured 250 ppm
/// fast: 1,000 samples in, straight on from the first.
const RETIME_AT: u64 = 1_000;
const DRIFT: Drift = match Drift::from_ppb(250_000) {
    Some(drift) => drift,
    None => Drift::ZERO,
};

/// When the third epoch opens: 1,750 samples in, with 3 s lost. It keeps
/// the second's drift.
const REOPEN_AT: u64 = 1_750;
const LOST: std::time::Duration = std::time::Duration::from_secs(3);

/// What the recording was timed by, before it stopped.
#[derive(Debug, Clone)]
struct Timed {
    /// The track's timeline as the recording opened its epochs.
    timeline: TrackTimeline,
    /// The session clock's last reading.
    clock_end: SessionTime,
}

/// Records `MIC` in 250-sample chunks, a quarter-second each: epoch 0 from
/// 0 s, epoch 1 at sample [`RETIME_AT`] under [`DRIFT`], then, at sample
/// [`REOPEN_AT`], [`LOST`] with no audio and epoch 2; 3,000 samples in
/// all, publishing each journal as it ends. Stops at the first error.
fn record_two_epochs(fs: &FakeFs) -> Timed {
    let mut timed = Timed {
        timeline: TrackTimeline::new(MIC),
        clock_end: SessionTime::ZERO,
    };
    let _ = record_into(fs, &mut timed);
    timed
}

fn record_into(fs: &FakeFs, timed: &mut Timed) -> Result<(), Box<dyn Error>> {
    let (clock, dyn_clock) = fake_clock();
    let lock = owned(fs);
    let mut writer = SessionWriter::open(&lock, rate(), length(), dyn_clock)?;
    let mut store = store_on(&lock);
    let (timeline, epoch) = writer.open_first_epoch(MIC, clock.now())?;
    timed.timeline = timeline;
    writer.start_track(MIC, &epoch)?;
    for _ in 0..12 {
        let from = writer.next_sample(MIC).ok_or("not started")?.get();
        if from == RETIME_AT {
            timed.timeline.retime(SampleIndex::new(from), DRIFT)?;
            let retimed: Epoch = *timed.timeline.current().ok_or("no epoch")?;
            writer.new_epoch(MIC, &retimed)?;
        }
        if from == REOPEN_AT {
            clock.advance(LOST);
            timed.clock_end = clock.now();
            timed
                .timeline
                .open_epoch(clock.now(), SampleIndex::new(from), rate())?;
            let reopened: Epoch = *timed.timeline.current().ok_or("no epoch")?;
            writer.new_epoch(MIC, &reopened)?;
        }
        // Audio arrives once it has been captured: the clock first.
        clock.advance(std::time::Duration::from_millis(250));
        timed.clock_end = clock.now();
        writer.append(MIC, &samples(MIC, from, 250))?;
        writer.sync_if_due()?;
        let finished = writer.take_finished();
        if !finished.is_empty() {
            publish_journals(&mut store, length(), &finished)?;
        }
    }
    let finished = writer.finish()?;
    publish_journals(&mut store, length(), &finished)?;
    Ok(())
}

/// What salvage left: the committed rows, the epochs the store times, and
/// where a writer reopening the session would resume its clock.
#[derive(Debug)]
struct Kept {
    rows: Vec<SegmentRow>,
    anchors: Vec<(TrackId, EpochAnchor)>,
    resume_from: SessionTime,
}

/// Salvages the session, then reads what the store and a reopened writer
/// say.
fn salvage_and_read(fs: &FakeFs) -> Result<Kept, String> {
    salvage(&mut session_store(fs), length()).map_err(|e| e.to_string())?;
    let mut store = FakeStore::new(fs, &db());
    let rows = store.rows(SESSION).map_err(|e| e.to_string())?;
    let anchors = store.epochs(SESSION).map_err(|e| e.to_string())?;
    let lock = owned(fs);
    let writer =
        SessionWriter::open(&lock, rate(), length(), fake_clock().1).map_err(|e| e.to_string())?;
    Ok(Kept {
        rows,
        anchors,
        resume_from: writer.resume_from(),
    })
}

/// The invariants, at any crash point: every committed row's epoch is
/// timed in the store as it was recorded; the timeline rebuilt from the
/// store times every row's samples as the recording did, gaps included;
/// and a resumed clock would start after the audio kept and no later than
/// the clock had got to.
fn check_timed(timed: &Timed, kept: &Result<Kept, String>) -> Result<(), String> {
    let kept = kept.as_ref().map_err(String::clone)?;
    for row in &kept.rows {
        let recorded = timed
            .timeline
            .epochs()
            .iter()
            .find(|e| e.id() == row.epoch())
            .map(Epoch::anchor);
        let stored = kept
            .anchors
            .iter()
            .find(|(t, a)| *t == row.track() && a.id == row.epoch())
            .map(|(_, a)| *a);
        if stored.is_none() || stored != recorded {
            return Err(format!(
                "row {row:?}: epoch stored as {stored:?}, recorded as {recorded:?}"
            ));
        }
    }
    let rebuilt = TrackTimeline::rebuild(MIC, kept.anchors.iter().map(|(_, a)| *a))
        .map_err(|e| format!("the stored anchors don't make a timeline: {e}"))?;
    for row in &kept.rows {
        let last = SampleIndex::new(row.range().end().get() - 1);
        for sample in [row.range().start(), last] {
            if rebuilt.time_of(sample) != timed.timeline.time_of(sample) {
                return Err(format!(
                    "sample {} is timed at {:?}, recorded at {:?}",
                    sample.get(),
                    rebuilt.time_of(sample),
                    timed.timeline.time_of(sample)
                ));
            }
        }
    }
    let epochs_kept: BTreeSet<EpochId> = kept.rows.iter().map(SegmentRow::epoch).collect();
    if epochs_kept.len() == timed.timeline.epochs().len()
        && rebuilt.gaps().collect::<Vec<_>>() != timed.timeline.gaps().collect::<Vec<_>>()
    {
        return Err("the gap between the epochs isn't kept".to_owned());
    }
    if let Some(end) = kept.rows.iter().map(|r| r.range().end()).max() {
        let last = SampleIndex::new(end.get() - 1);
        let audio_end = timed
            .timeline
            .epoch_of(last)
            .and_then(|e| e.time_of(end))
            .ok_or("the kept audio isn't timed")?;
        if kept.resume_from < audio_end || kept.resume_from > timed.clock_end {
            return Err(format!(
                "resumes at {:?}: before the audio's end {audio_end:?}, or after the clock's {:?}",
                kept.resume_from, timed.clock_end
            ));
        }
    }
    Ok(())
}

/// After a crash anywhere in a recording with a drift-corrected epoch and
/// a gap, salvage rebuilds every kept epoch's mapping from samples to
/// session time, drift included.
#[test]
fn salvage_rebuilds_every_epoch_s_timing_after_a_crash_anywhere() {
    let fs = FakeFs::with_dirs([session(), db()]);
    let clean = record_two_epochs(&fs);
    let kept = salvage_and_read(&fs).unwrap();
    // Not vacuous: uncrashed, every epoch publishes, the second and third
    // with their drift, with the gap after the second.
    let drifts: Vec<Drift> = kept.anchors.iter().map(|(_, a)| a.drift).collect();
    assert_eq!(drifts, [Drift::ZERO, DRIFT, DRIFT]);
    assert_eq!(
        kept.anchors,
        clean
            .timeline
            .epochs()
            .iter()
            .map(|e| (MIC, e.anchor()))
            .collect::<Vec<_>>()
    );
    let gaps: Vec<_> = clean.timeline.gaps().map(|g| (g.from(), g.to())).collect();
    // 750 samples at 1 kHz, 250 ppm fast, take 749,812,547 ns.
    assert_eq!(
        gaps,
        [(
            SessionTime::from_nanos(1_749_812_547),
            SessionTime::from_nanos(4_750_000_000)
        )]
    );
    check_timed(&clean, &Ok(kept)).unwrap();

    let both = Cell::new(0);
    let summary = CrashTest::new(
        record_two_epochs,
        salvage_and_read,
        |_: &CrashCase, timed: &Timed, kept: &Result<Kept, String>| {
            // Rows on both sides of the gap: the drifted second epoch's
            // and the third's.
            if kept.as_ref().is_ok_and(|k| {
                let epochs: BTreeSet<EpochId> = k.rows.iter().map(SegmentRow::epoch).collect();
                epochs.contains(&EpochId::new(1)) && epochs.contains(&EpochId::new(2))
            }) {
                both.set(both.get() + 1);
            }
            check_timed(timed, kept)
        },
    )
    .dirs([session(), db()])
    .recovery_outcomes(vec![CrashOutcome::LoseUnsynced, CrashOutcome::KeepAll])
    .sample_recovery(SAMPLE)
    .run()
    .unwrap_or_else(|failure| panic!("{failure}"));
    summary.scenario().interrupted_more_than(60); // check-bound
    summary.recovery().interrupted_more_than(100); // check-bound
    // Many crashes left rows either side of the gap to time across it.
    assert!(both.get() > 100, "{}", both.get()); // check-bound
}

/// What a recording that ends just after a loss was timed by.
#[derive(Debug, Clone)]
struct Trailing {
    /// The epoch opened for the loss, with no audio after it, once the
    /// writer has moved to it without an error.
    moved_to: Option<Epoch>,
    /// The session clock's last reading.
    clock_end: SessionTime,
}

/// Records `MIC` for 1 s in 250-sample chunks, publishing each journal as
/// it ends, then loses [`LOST`] and opens an epoch for it at the next
/// sample, as a stop inside a burst does, and stops. Stops at the first
/// error.
fn record_trailing(fs: &FakeFs) -> Trailing {
    let mut trailing = Trailing {
        moved_to: None,
        clock_end: SessionTime::ZERO,
    };
    let _ = record_trailing_into(fs, &mut trailing);
    trailing
}

fn record_trailing_into(fs: &FakeFs, trailing: &mut Trailing) -> Result<(), Box<dyn Error>> {
    let (clock, dyn_clock) = fake_clock();
    let lock = owned(fs);
    let mut writer = SessionWriter::open(&lock, rate(), length(), dyn_clock)?;
    let mut store = store_on(&lock);
    let (mut timeline, epoch) = writer.open_first_epoch(MIC, clock.now())?;
    writer.start_track(MIC, &epoch)?;
    for _ in 0..4 {
        let from = writer.next_sample(MIC).ok_or("not started")?.get();
        clock.advance(std::time::Duration::from_millis(250));
        trailing.clock_end = clock.now();
        writer.append(MIC, &samples(MIC, from, 250))?;
        writer.sync_if_due()?;
        let finished = writer.take_finished();
        if !finished.is_empty() {
            publish_journals(&mut store, length(), &finished)?;
        }
    }
    clock.advance(LOST);
    trailing.clock_end = clock.now();
    let next = writer.next_sample(MIC).ok_or("not started")?;
    timeline.open_epoch(clock.now(), next, rate())?;
    let lost: Epoch = *timeline.current().ok_or("no epoch")?;
    writer.new_epoch(MIC, &lost)?;
    trailing.moved_to = Some(lost);
    let finished = writer.finish()?;
    publish_journals(&mut store, length(), &finished)?;
    Ok(())
}

/// After salvage, what a writer reopening the session carries on from:
/// the track's highest epoch, and where its clock would resume.
fn salvage_and_resume(fs: &FakeFs) -> Result<(Option<EpochId>, SessionTime), String> {
    salvage(&mut session_store(fs), length()).map_err(|e| e.to_string())?;
    let lock = owned(fs);
    let writer =
        SessionWriter::open(&lock, rate(), length(), fake_clock().1).map_err(|e| e.to_string())?;
    Ok((writer.highest_epoch(MIC), writer.resume_from()))
}

/// The invariants, at any crash point: once the writer has moved to the
/// epoch no audio follows, a resumed session carries on above it and from
/// its start, so its number is never used again and its gap is never
/// overlapped; and a resumed clock never starts past where the clock got.
fn check_trailing(
    trailing: &Trailing,
    resumed: &Result<(Option<EpochId>, SessionTime), String>,
) -> Result<(), String> {
    let (highest, resume_from) = resumed.as_ref().map_err(String::clone)?;
    if let Some(lost) = trailing.moved_to
        && (*highest < Some(lost.id()) || *resume_from < lost.start())
    {
        return Err(format!(
            "moved to {lost:?}, but a resumed session carries on from {highest:?} at {resume_from:?}"
        ));
    }
    if *resume_from > trailing.clock_end {
        return Err(format!(
            "resumes at {resume_from:?}, after the clock's {:?}",
            trailing.clock_end
        ));
    }
    Ok(())
}

/// After a crash anywhere in a recording that ends just after a loss, an
/// epoch the writer moved to with no audio after it is still carried on
/// from: never numbered again, and its gap never overlapped.
#[test]
fn an_epoch_with_no_audio_after_it_survives_a_crash_anywhere() {
    let fs = FakeFs::with_dirs([session(), db()]);
    let clean = record_trailing(&fs);
    let resumed = salvage_and_resume(&fs).unwrap();
    // Not vacuous: uncrashed, the writer moved to epoch 1 at 4 s, and a
    // resumed session carries on from there, not from the audio's end at
    // 1 s.
    let lost = clean.moved_to.unwrap();
    assert_eq!(
        (lost.id(), lost.start()),
        (EpochId::new(1), SessionTime::from_nanos(4_000_000_000))
    );
    assert_eq!(resumed, (Some(EpochId::new(1)), lost.start()));
    check_trailing(&clean, &Ok(resumed)).unwrap();

    let (moved, not_moved) = (Cell::new(0), Cell::new(0));
    let summary = CrashTest::new(
        record_trailing,
        salvage_and_resume,
        |_: &CrashCase, trailing: &Trailing, resumed: &Result<_, String>| {
            let counter = if trailing.moved_to.is_some() {
                &moved
            } else {
                &not_moved
            };
            counter.set(counter.get() + 1);
            check_trailing(trailing, resumed)
        },
    )
    .dirs([session(), db()])
    .recovery_outcomes(vec![CrashOutcome::LoseUnsynced, CrashOutcome::KeepAll])
    .sample_recovery(SAMPLE)
    .run()
    .unwrap_or_else(|failure| panic!("{failure}"));
    summary.scenario().interrupted_more_than(40); // check-bound
    // Crashes both before and after the writer moved to the epoch.
    assert!(moved.get() > 400, "{}", moved.get()); // check-bound
    assert!(not_moved.get() > 300, "{}", not_moved.get()); // check-bound
}
