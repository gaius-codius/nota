//! Stopping a recording, in order, however it was asked for.

use nota_recorder::capture::CaptureBackend;
use nota_store::{SessionState, StoreError};

use super::BoxError;
use super::live::LiveInput;
use super::start::Started;
use super::summary::{Outcome, Shown, note_published, note_saved};

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
        publisher,
        live_inputs,
        live,
        saver,
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
    // Everything the live thread and the screen gave is stored, or given
    // up on, before the session is marked stopped (and before a failed
    // publisher returns early).
    let saved = saver.finish();
    note_published(&mut outcome, &stopped.published?);
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
    note_saved(&mut outcome, &saved, shown.marks);
    Ok(outcome)
}
