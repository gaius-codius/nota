//! The live thread: feeds the engine and the screen while recording.

use std::io;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nota_core::recorder;
use nota_core::{Clock, TrackId, Utterance, Word};
use nota_recorder::capture::RecorderEvent;
use nota_recorder::engine::{EngineEvent, EngineStatus, EngineSupervisor};
use nota_tui::Event;

use super::save::ToSave;
use crate::latency::{LatencyLog, Problem};
use crate::live::{Actions, Live};

/// What the live thread is told.
#[derive(Debug)]
pub(super) enum LiveInput {
    Recorder(Option<TrackId>, RecorderEvent),
    Engine(EngineEvent),
    /// Recording has ended: shut the engine down.
    Done,
}

/// The live thread: feeds the engine and the screen until told recording
/// is done, and hands each text placed for the screen to the saver
/// (`save`). Then it shuts the engine down, and keeps handing on the text
/// the engine sends meanwhile (its answer to the last flush) until the
/// engine's events end (waiting up to [`LATE_WAIT`] for each). With a
/// latency log, notes when each text is handed to the screen, and anything
/// that keeps text from it, by `clock`, including what the engine reports
/// after the screen has closed, and returns the log.
pub(super) fn spawn_live(
    mut live: Live,
    mut engine: Option<EngineSupervisor>,
    inputs: Receiver<LiveInput>,
    ui: Sender<Event>,
    save: Sender<ToSave>,
    mut log: Option<(LatencyLog, Arc<dyn Clock>)>,
) -> io::Result<JoinHandle<Option<LatencyLog>>> {
    thread::Builder::new()
        .name("nota-live".into())
        .spawn(move || {
            for input in &inputs {
                let noted = Noted::of(&input);
                let actions = match input {
                    LiveInput::Recorder(track, event) => live.recorder(track, event),
                    LiveInput::Engine(event) => live.engine(event),
                    LiveInput::Done => break,
                };
                let texts: Vec<_> = if log.is_some() {
                    actions
                        .updates
                        .iter()
                        .filter_map(|u| match u {
                            recorder::Event::Text(text) => Some((text.start(), text.end())),
                            _ => None,
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                apply(actions, engine.as_mut(), &ui, &save);
                if let Some((log, clock)) = log.as_mut() {
                    let now = clock.now();
                    match noted {
                        Noted::Heard(track) if texts.is_empty() => {
                            log.problem(Problem::Dropped, Some(track), now);
                        }
                        Noted::Heard(track) => {
                            for (start, end) in texts {
                                log.text(track, start, end, now);
                            }
                        }
                        Noted::Problem(problem, track) => log.problem(problem, track, now),
                        Noted::Nothing => {}
                    }
                }
            }
            // The engine's events go on through their own thread while it
            // shuts down, and end once it has.
            if let Some(engine) = engine {
                engine.shutdown();
            }
            while let Ok(input) = inputs.recv_timeout(LATE_WAIT) {
                if let Some((log, clock)) = log.as_mut() {
                    // The screen has closed: text from now on never shows.
                    match Noted::of(&input) {
                        Noted::Heard(track) => log.problem(Problem::Late, Some(track), clock.now()),
                        Noted::Problem(problem, track) => log.problem(problem, track, clock.now()),
                        Noted::Nothing => {}
                    }
                }
                if let LiveInput::Engine(event) = input {
                    save_heard(live.engine(event).heard, &save);
                }
            }
            log.map(|(log, _)| log)
        })
}

/// The longest the live thread waits for each of the engine's events after
/// the engine has shut down. They pass through a thread of their own,
/// which ends once it has passed them all on, and the wait ends with it;
/// this only bounds a stop whose events thread never ends, and is long
/// enough that a thread descheduled for a while still gets its text in.
const LATE_WAIT: Duration = Duration::from_secs(10);

/// What the latency log notes about one input to the live thread.
enum Noted {
    /// A transcript of the track: its text, or that it was dropped.
    Heard(TrackId),
    /// Something that keeps text from the screen.
    Problem(Problem, Option<TrackId>),
    Nothing,
}

impl Noted {
    fn of(input: &LiveInput) -> Self {
        match input {
            LiveInput::Engine(EngineEvent::Transcript(t)) => Self::Heard(t.track()),
            LiveInput::Engine(EngineEvent::Skipped { track, .. }) => {
                Self::Problem(Problem::Skipped, Some(*track))
            }
            LiveInput::Engine(EngineEvent::Status(EngineStatus::Offline(_))) => {
                Self::Problem(Problem::Offline, None)
            }
            LiveInput::Recorder(
                track,
                RecorderEvent::Epoch(_) | RecorderEvent::EpochRefused(_),
            ) => Self::Problem(Problem::Epoch, *track),
            LiveInput::Recorder(track, RecorderEvent::CaptureFailed(_)) => {
                Self::Problem(Problem::Failed, *track)
            }
            _ => Self::Nothing,
        }
    }
}

fn apply(
    actions: Actions,
    engine: Option<&mut EngineSupervisor>,
    ui: &Sender<Event>,
    save: &Sender<ToSave>,
) {
    if let Some(engine) = engine {
        if let Some(chunk) = actions.transcribe {
            // Refused audio would only make the engine fail; the recording
            // has it.
            let _ = engine.send_audio(chunk);
        }
        if let Some(track) = actions.flush {
            engine.flush(track);
        }
    }
    save_heard(actions.heard, save);
    for update in actions.updates {
        let _ = ui.send(Event::Recorder(update));
    }
}

/// Hands `heard`, if any, to the saver.
fn save_heard(heard: Option<(Utterance, Vec<Word>)>, save: &Sender<ToSave>) {
    if let Some((text, words)) = heard {
        // A saver that has stopped has said why in its report.
        let _ = save.send(ToSave::Heard(text, words));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::PathBuf;
    use std::sync::mpsc;

    use nota_core::TrackTimeline;

    use super::super::{MIC, RATE, SYSTEM};
    use super::*;

    /// Each text is logged with its track, its chunk's span placed through
    /// the track's epoch, and when it was handed to the screen; levels and
    /// text-less events aren't.
    #[test]
    fn the_live_thread_logs_when_each_text_reached_the_screen() {
        use nota_core::messages::{AudioChunk, Transcript};
        use nota_core::{FakeClock, SampleIndex, SampleRange, SessionTime};

        let ms = |ms: u64| SessionTime::from_nanos(ms * 1_000_000);
        let fake = Arc::new(FakeClock::new(ms(0)));
        let clock: Arc<dyn Clock> = Arc::clone(&fake) as Arc<dyn Clock>;
        // The mic opened at 0 ms, the system audio at 500 ms.
        let timelines: Vec<TrackTimeline> = [(MIC, 0), (SYSTEM, 500)]
            .into_iter()
            .map(|(track, at)| {
                let mut t = TrackTimeline::new(track);
                t.open_epoch(ms(at), SampleIndex::ZERO, RATE).unwrap();
                t
            })
            .collect();
        let heard = |track, from: u64, to: u64| {
            let range = SampleRange::new(SampleIndex::new(from), SampleIndex::new(to)).unwrap();
            LiveInput::Engine(EngineEvent::Transcript(
                Transcript::new(track, range, "words".to_owned()).unwrap(),
            ))
        };
        let (inputs, received) = mpsc::channel();
        let (ui, screen) = mpsc::channel();
        let (save, saved) = mpsc::channel();
        let log = LatencyLog::new(PathBuf::new());
        let live = spawn_live(
            Live::new(&timelines),
            None,
            received,
            ui,
            save,
            Some((log, Arc::clone(&clock))),
        )
        .unwrap();
        // A second of audio, which only shows a level.
        let audio = AudioChunk::new(MIC, SampleIndex::ZERO, RATE, vec![100; 16_000]).unwrap();
        inputs
            .send(LiveInput::Recorder(Some(MIC), RecorderEvent::Audio(audio)))
            .unwrap();
        // Everything after this is handed over at 4.2 s: the clock is moved
        // only before anything is sent, so the live thread can't race it.
        fake.advance(Duration::from_millis(4_200));
        // The mic's 0.5–3.5 s.
        inputs.send(heard(MIC, 8_000, 56_000)).unwrap();
        // A track with no timeline: its text can't be placed.
        inputs.send(heard(TrackId::new(7), 0, 16_000)).unwrap();
        inputs
            .send(LiveInput::Engine(EngineEvent::Skipped {
                track: SYSTEM,
                range: SampleRange::new(SampleIndex::ZERO, SampleIndex::new(1_600)).unwrap(),
            }))
            .unwrap();
        // The system audio's first 2 s, from 0.5 s.
        inputs.send(heard(SYSTEM, 0, 32_000)).unwrap();
        inputs.send(LiveInput::Done).unwrap();
        // After the screen closed: never shown, but saved.
        inputs.send(heard(MIC, 56_000, 72_000)).unwrap();
        drop(inputs);

        let log = live.join().unwrap().unwrap();
        let shown = screen
            .try_iter()
            .filter(|e| matches!(e, Event::Recorder(recorder::Event::Text(_))))
            .count();
        assert_eq!(shown, 2);
        let saved: Vec<_> = saved
            .try_iter()
            .map(|s| match s {
                ToSave::Heard(u, _) => (u.track(), u.start(), u.end()),
                ToSave::Annotation(_) => panic!("{s:?}"),
            })
            .collect();
        assert_eq!(
            saved,
            [
                (MIC, ms(500), ms(3_500)),
                (SYSTEM, ms(500), ms(2_500)),
                (MIC, ms(3_500), ms(4_500))
            ]
        );
        // Drawn by the draw after the first to end at or after 4.2 s.
        assert_eq!(
            log.contents(&[ms(4_000), ms(4_200), ms(4_230)]),
            "kind\ttrack\tstart_ms\tend_ms\thanded_ms\tdrawn_ms\n\
             text\t0\t500\t3500\t4200\t4230\n\
             dropped\t7\t-\t-\t4200\t-\n\
             skipped\t1\t-\t-\t4200\t-\n\
             text\t1\t500\t2500\t4200\t4230\n\
             late\t0\t-\t-\t4200\t-\n"
        );
    }

    /// GAI-204: text the engine sends after recording is done (its answer
    /// to the shutdown's flush, which comes through the engine's events
    /// thread while the shutdown runs) is handed to the saver, however
    /// late, until the engine's events end.
    #[test]
    fn text_the_engine_sends_during_its_shutdown_is_saved() {
        use nota_core::messages::Transcript;
        use nota_core::{SampleIndex, SampleRange, SessionTime};

        let mut timeline = TrackTimeline::new(MIC);
        timeline
            .open_epoch(SessionTime::ZERO, SampleIndex::ZERO, RATE)
            .unwrap();
        let (inputs, received) = mpsc::channel();
        let (ui, screen) = mpsc::channel();
        let (save, saved) = mpsc::channel();
        let live = spawn_live(Live::new(&[timeline]), None, received, ui, save, None).unwrap();
        // The engine's events thread, still passing events on after Done.
        let engine_events = inputs.clone();
        inputs.send(LiveInput::Done).unwrap();
        drop(inputs);
        let late = thread::spawn(move || {
            let (_keep, never) = mpsc::channel::<()>();
            let _ = never.recv_timeout(Duration::from_millis(300));
            let range = SampleRange::new(SampleIndex::ZERO, SampleIndex::new(16_000)).unwrap();
            let text = Transcript::new(MIC, range, "the last words".to_owned()).unwrap();
            engine_events
                .send(LiveInput::Engine(EngineEvent::Transcript(text)))
                .unwrap();
        });
        assert!(live.join().unwrap().is_none());
        late.join().unwrap();
        let saved: Vec<_> = saved.try_iter().collect();
        assert_eq!(saved.len(), 1, "{saved:?}");
        let ToSave::Heard(u, _) = &saved[0] else {
            panic!("{saved:?}");
        };
        assert_eq!(u.text(), "the last words");
        assert_eq!(u.end(), SessionTime::from_nanos(1_000_000_000));
        drop(screen);
    }
}
