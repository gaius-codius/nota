//! Stopping a recording, in order, however it was asked for.

use nota_recorder::capture::CaptureBackend;
use nota_recorder::disk::{DiskSummary, Freed, MonitorPanicked};
use nota_store::{SessionState, StoreError};

use super::BoxError;
use super::live::LiveInput;
use super::start::Started;
use super::summary::{Outcome, Shown, note_published};

/// Stops what `started` started, in order, and finishes the session.
///
/// # Errors
///
/// If the recorder or the publisher stopped unexpectedly, or the screen
/// failed (`shown`); in the last case the recording was still finished
/// first.
pub(super) fn stop<B: CaptureBackend>(
    started: Started<B>,
    shown: std::io::Result<Shown>,
) -> Result<Outcome, BoxError> {
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
        recorder,
    } = started;
    // Stop, in order.
    drop(captures);
    let (writer, how_it_ended, failures, failed_streams) = recorder
        .join()
        .map_err(|_| "the recorder stopped unexpectedly")?;
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
    // The writer's last journals are published while the live thread
    // shuts the engine down.
    let mut log = None;
    let stopped = publisher.finish_recording(writer, || {
        let _ = live_inputs.send(LiveInput::Done);
        drop(live_inputs);
        log = live.join().ok().flatten();
    });
    let draw_ends = draws.map(|d| d.times()).unwrap_or_default();
    if let Some(Err(e)) = log.map(|log| log.write(&draw_ends)) {
        outcome.notes.push(format!("writing the latency log: {e}"));
    }
    if let Some(e) = stopped.finishing {
        outcome.notes.push(format!("finishing the recording: {e}"));
    }
    note_published(&mut outcome, &stopped.published?);
    note_disk(&mut outcome, disk.stop());
    match library
        .db()
        .with(|db| db.set_state(session, SessionState::Stopped))
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

    let shown = shown?;
    if let Some(problem) = shown.problem {
        outcome.notes.push(problem);
    }
    if shown.marks > 0 {
        outcome.notes.push(format!(
            "{} marks and notes made; they aren't saved yet",
            shown.marks
        ));
    }
    Ok(outcome)
}

/// What the summary says of the disk: a full disk, which stopped the
/// recording, and a ballast that couldn't be made.
fn note_disk(outcome: &mut Outcome, disk: Result<DiskSummary, MonitorPanicked>) {
    let summary = match disk {
        Ok(summary) => summary,
        Err(e) => {
            outcome
                .notes
                .push(format!("{e}; the disk wasn't watched to the end"));
            return;
        }
    };
    if let Some(full) = summary.full {
        let ballast = match full.ballast {
            Freed::Freed => {
                "nota freed the space it keeps for this, so the last segments were finished"
                    .to_owned()
            }
            Freed::None => "there was no room for the space nota keeps for this".to_owned(),
            Freed::Failed(kind) => {
                format!("the space nota keeps for this couldn't be freed ({kind})")
            }
        };
        outcome.notes.push(format!(
            "stopped early: the disk is full; {ballast}. Free some space before recording again"
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

    fn noted(disk: Result<DiskSummary, MonitorPanicked>) -> Vec<String> {
        let mut outcome = Outcome::default();
        note_disk(&mut outcome, disk);
        outcome.notes
    }

    #[test]
    fn a_full_disk_says_the_recording_stopped_early_and_what_became_of_the_ballast() {
        let full = |ballast| {
            Ok(DiskSummary {
                full: Some(Full {
                    path: Some(PathBuf::from("/data/sessions/1/audio/journal-000003")),
                    ballast,
                }),
                ballast_error: None,
                ballast_held: true,
            })
        };
        assert_eq!(
            noted(full(Freed::Freed)),
            [
                "stopped early: the disk is full; nota freed the space it keeps for this, \
              so the last segments were finished. Free some space before recording again"
            ]
        );
        assert_eq!(
            noted(full(Freed::None)),
            [
                "stopped early: the disk is full; there was no room for the space nota keeps \
              for this. Free some space before recording again"
            ]
        );
        assert_eq!(
            noted(full(Freed::Failed(io::ErrorKind::PermissionDenied))),
            [
                "stopped early: the disk is full; the space nota keeps for this couldn't be \
              freed (permission denied). Free some space before recording again"
            ]
        );
    }

    #[test]
    fn a_disk_that_never_filled_says_nothing_unless_the_ballast_failed() {
        assert!(noted(Ok(DiskSummary::default())).is_empty());
        let failed = DiskSummary {
            ballast_error: Some("permission denied".to_owned()),
            ..DiskSummary::default()
        };
        assert_eq!(
            noted(Ok(failed)),
            ["the space nota keeps for a full disk couldn't be set aside: permission denied"]
        );
        assert_eq!(
            noted(Err(MonitorPanicked)),
            ["the disk monitor stopped unexpectedly; the disk wasn't watched to the end"]
        );
    }
}
