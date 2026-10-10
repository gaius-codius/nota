//! Stopping a recording, in order, however it was asked for.

use nota_recorder::capture::CaptureBackend;
use nota_recorder::disk::{DiskSummary, Freed, MonitorPanicked};
use nota_store::{StoreError, Wait};

use nota_recorder::segment::Stopped;
use nota_recorder::session::SessionWriter;

use super::live::LiveInput;
use super::start::{RecordFs, Recorded, Started};
use super::summary::{Outcome, Shown, note_published, note_saved};

/// Stops what `started` started, in order, and finishes the session.
/// The session is saved by now, so whatever goes wrong while stopping (the
/// recorder or the publisher stopping unexpectedly, the screen failing)
/// is a note on the outcome, which is reported all the same.
pub(super) fn stop<B: CaptureBackend>(started: Started<B>, shown: Shown) -> Outcome {
    let Started {
        mut outcome,
        signals,
        draws,
        library,
        session,
        lock,
        captures,
        disk,
        publisher,
        live_inputs,
        live,
        saver,
        recorder,
    } = started;
    // Whether the disk had filled while recording: what stopped it.
    let full_while_recording = disk.full().is_some();
    // Stop, in order.
    drop(captures);
    let writer = recorded(recorder.join(), &mut outcome);
    // The writer's last journals are published while the live thread
    // shuts the engine down.
    let mut log = None;
    let meanwhile = || {
        let _ = live_inputs.send(LiveInput::Done);
        drop(live_inputs);
        log = live.join().ok().flatten();
    };
    let stopped = if let Some(writer) = writer {
        publisher.finish_recording(writer, meanwhile)
    } else {
        meanwhile();
        Stopped {
            finishing: None,
            published: publisher.finish(),
        }
    };
    let draw_ends = draws.map(|d| d.times()).unwrap_or_default();
    if let Some(Err(e)) = log.map(|log| log.write(&draw_ends)) {
        outcome.notes.push(format!("writing the latency log: {e}"));
    }
    if let Some(e) = stopped.finishing {
        outcome.notes.push(format!("finishing the recording: {e}"));
    }
    // The disk's notes even if publishing went wrong.
    let disk = disk.stop();
    // The session's jobs wait for space if the disk filled at any point.
    let full = full_while_recording || disk.as_ref().is_ok_and(|d| d.full.is_some());
    note_disk(&mut outcome, disk, full_while_recording);
    // Everything the live thread and the screen gave is stored, or given
    // up on, before the session is marked stopped and its jobs queued (and
    // before a failed publisher returns early).
    let saved = saver.finish();
    match &stopped.published {
        Ok(report) => note_published(&mut outcome, report),
        Err(e) => outcome
            .notes
            .push(format!("{e}; the next start publishes what's left")),
    }
    match library
        .db()
        .with(|db| db.finish_recording(session, full.then_some(Wait::Space)))
    {
        Ok(()) => {}
        Err(StoreError::NoSession(_)) => outcome.notes.push(
            "the library database couldn't take this session; the next start adds it, \
             without its title"
                .to_owned(),
        ),
        Err(e) => outcome.notes.push(format!(
            "marking the session stopped in the library database failed ({e}); \
             the next start does it"
        )),
    }
    if signals.close() {
        outcome.notes.push(
            "the capture thread ran past its real-time budget at least once \
             (SIGXCPU); the recording carried on"
                .to_owned(),
        );
    }
    drop(lock);

    if let Some(problem) = shown.problem {
        outcome.notes.push(problem);
    }
    note_saved(&mut outcome, &saved, shown.marks);
    outcome
}

/// The writer the recorder thread hands back, with how recording went
/// noted on `outcome`; `None` if the thread panicked, which leaves its
/// journals for the next start's salvage.
fn recorded(
    joined: std::thread::Result<Recorded>,
    outcome: &mut Outcome,
) -> Option<SessionWriter<RecordFs>> {
    let Ok((writer, how_it_ended, failures, failed_streams)) = joined else {
        outcome.notes.push(
            "the recorder stopped unexpectedly; the next start publishes what it recorded"
                .to_owned(),
        );
        return None;
    };
    if let Err(e) = how_it_ended {
        outcome.notes.push(format!("recording stopped early: {e}"));
    }
    for stream in failed_streams {
        outcome.notes.push(format!("stopped recording {stream}"));
    }
    if failures > 0 {
        outcome.notes.push(format!(
            "{failures} journal failures; the audio around them may have gaps"
        ));
    }
    Some(writer)
}

/// What the summary says of the disk: a full disk (which stopped the
/// recording if it filled `while_recording`), a low disk, a disk that
/// couldn't be checked, and a ballast that couldn't be made.
fn note_disk(
    outcome: &mut Outcome,
    disk: Result<DiskSummary, MonitorPanicked>,
    while_recording: bool,
) {
    let summary = match disk {
        Ok(summary) => summary,
        Err(e) => {
            outcome
                .notes
                .push(format!("{e}; the disk wasn't watched to the end"));
            return;
        }
    };
    if let Some(full) = &summary.full {
        let ballast = match full.ballast {
            Freed::Freed => {
                "nota freed the space it keeps for this to finish the last segments".to_owned()
            }
            Freed::None => "nota had no space set aside for this".to_owned(),
            Freed::Failed(kind) => {
                format!("the space nota keeps for this couldn't be freed ({kind})")
            }
        };
        let what = if while_recording {
            "stopped early: the disk is full"
        } else {
            "the disk filled while the last segments were published"
        };
        outcome.notes.push(format!(
            "{what}; {ballast}. Free some space before recording again"
        ));
    } else if summary.low {
        outcome
            .notes
            .push("the disk ran low: under an hour of recording was left".to_owned());
    }
    if let Some(e) = summary.unchecked {
        outcome.notes.push(format!(
            "the free space couldn't be checked ({e}), so a low disk wasn't warned of"
        ));
    }
    if let Some(e) = summary.ballast_error {
        outcome.notes.push(format!(
            "the space nota keeps for a full disk couldn't be set aside: {e}"
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::PathBuf;

    use nota_recorder::disk::Full;

    use super::*;

    fn noted(disk: Result<DiskSummary, MonitorPanicked>, while_recording: bool) -> Vec<String> {
        let mut outcome = Outcome::new(nota_core::SessionId::new(1), PathBuf::new());
        note_disk(&mut outcome, disk, while_recording);
        outcome.notes
    }

    fn full(ballast: Freed) -> DiskSummary {
        DiskSummary {
            full: Some(Full {
                path: Some(PathBuf::from("/data/sessions/1/audio/journal-000003")),
                ballast,
            }),
            ballast_held: true,
            low: true,
            ..DiskSummary::default()
        }
    }

    /// Acceptance (GAI-212): the recorder stopping unexpectedly, after the
    /// session was saved, is a note on the outcome, which is still
    /// reported, and nothing is published from a writer that's gone.
    #[test]
    fn a_recorder_that_panicked_is_a_note_on_the_outcome() {
        let mut outcome = Outcome::new(nota_core::SessionId::new(1), PathBuf::new());
        let joined: std::thread::Result<Recorded> = Err(Box::new("the recorder panicked"));
        assert!(recorded(joined, &mut outcome).is_none());
        assert_eq!(
            outcome.notes,
            ["the recorder stopped unexpectedly; the next start publishes what it recorded"]
        );
    }

    #[test]
    fn a_full_disk_says_the_recording_stopped_early_and_what_became_of_the_ballast() {
        assert_eq!(
            noted(Ok(full(Freed::Freed)), true),
            [
                "stopped early: the disk is full; nota freed the space it keeps for this to \
              finish the last segments. Free some space before recording again"
            ]
        );
        assert_eq!(
            noted(Ok(full(Freed::None)), true),
            [
                "stopped early: the disk is full; nota had no space set aside for this. \
              Free some space before recording again"
            ]
        );
        assert_eq!(
            noted(
                Ok(full(Freed::Failed(io::ErrorKind::PermissionDenied))),
                true
            ),
            [
                "stopped early: the disk is full; the space nota keeps for this couldn't be \
              freed (permission denied). Free some space before recording again"
            ]
        );
    }

    #[test]
    fn a_disk_that_filled_after_the_stop_doesnt_say_the_recording_stopped_early() {
        assert_eq!(
            noted(Ok(full(Freed::Freed)), false),
            [
                "the disk filled while the last segments were published; nota freed the space \
              it keeps for this to finish the last segments. Free some space before \
              recording again"
            ]
        );
    }

    #[test]
    fn a_low_disk_an_unchecked_one_and_a_ballast_that_failed_are_noted() {
        assert!(noted(Ok(DiskSummary::default()), false).is_empty());
        let summary = DiskSummary {
            low: true,
            unchecked: Some("permission denied".to_owned()),
            ballast_error: Some("permission denied".to_owned()),
            ..DiskSummary::default()
        };
        assert_eq!(
            noted(Ok(summary), false),
            [
                "the disk ran low: under an hour of recording was left",
                "the free space couldn't be checked (permission denied), so a low disk \
                 wasn't warned of",
                "the space nota keeps for a full disk couldn't be set aside: permission denied",
            ]
        );
        assert_eq!(
            noted(Err(MonitorPanicked), true),
            ["the disk monitor stopped unexpectedly; the disk wasn't watched to the end"]
        );
    }
}
