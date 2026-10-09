//! Bridges the persisted post-stop jobs and heard text to Processing.

use super::{
    Arc, BoxError, Clock, Duration, InputThread, Library, QuitSignals, RunError, Screen, SessionId,
    SessionTime, Theme, mpsc,
};
use nota_recorder::fs::StdFs;
use nota_recorder::segment::needs_salvage;
use nota_recorder::session::SessionDir;
use nota_store::{FinalText, Job, JobKind, JobState, SessionState, StoreError, TrackKind, Wait};
use nota_tui::{Processing, ProcessingAction, ProcessingJob, ProcessingState};
use std::io;

const REFRESH: Duration = Duration::from_secs(1);

pub(super) fn show(
    screen: &mut Screen,
    library: &Library,
    id: SessionId,
    engines: &str,
    theme: Theme,
    clock: &Arc<dyn Clock>,
    quit: &QuitSignals,
) -> Result<ProcessingAction, BoxError> {
    let mut page = Processing::new(format!("session {}", id.get()), engines.into(), theme);
    let mut data = Data {
        library,
        id,
        engines,
        clock,
        at: None,
    };
    data.refresh(&mut page);
    let (ui, events) = mpsc::channel();
    quit.show_home(Some(ui.clone()));
    if quit.asked() {
        quit.show_home(None);
        return Ok(ProcessingAction::Quit);
    }
    let ran = InputThread::spawn(ui, Arc::clone(clock))
        .map_err(BoxError::from)
        .and_then(|input| {
            let ran = screen.clear().map_err(RunError::Terminal).and_then(|()| {
                nota_tui::run_processing(screen.terminal(), &mut page, &events, &mut |page| {
                    data.refresh(page);
                })
            });
            let _ = input.stop();
            match ran {
                Ok(action) => Ok(action),
                Err(RunError::Terminal(e)) => Err(format!("the screen failed: {e}").into()),
                Err(RunError::InputLost(kind)) => {
                    Err(format!("the keyboard was lost: {kind}").into())
                }
                Err(RunError::CommandsClosed(_)) => Ok(ProcessingAction::Quit),
            }
        });
    quit.show_home(None);
    ran
}

struct Data<'a> {
    library: &'a Library,
    id: SessionId,
    engines: &'a str,
    clock: &'a Arc<dyn Clock>,
    at: Option<SessionTime>,
}

impl Data<'_> {
    fn refresh(&mut self, page: &mut Processing) {
        let now = self.clock.now();
        page.tick(now);
        if self
            .at
            .is_some_and(|at| now.checked_duration_since(at).unwrap_or_default() < REFRESH)
        {
            return;
        }
        self.at = Some(now);
        match self.read() {
            Ok((title, jobs, transcript)) => {
                page.set_title(title);
                page.set_jobs(jobs);
                page.set_transcript(transcript);
                page.set_notice(None);
            }
            Err(e) => page.set_notice(Some(format!("the session couldn't be read: {e}"))),
        }
    }

    fn read(&self) -> Result<(String, Vec<ProcessingJob>, Vec<String>), StoreError> {
        self.library.db().with(|db| {
            let session = db.session(self.id)?.ok_or(StoreError::NoSession(self.id))?;
            let jobs = db.session_jobs(self.id)?;
            let final_text = db.final_texts(self.id)?;
            let heard = db
                .utterances(self.id)?
                .into_iter()
                .map(|text| text.heard.utterance.into_text())
                .collect();
            let transcript = transcript(heard, &final_text, &db.tracks(self.id)?);
            let saved = if session.state == SessionState::Recording {
                ProcessingState::Waiting {
                    reason: "recording is still underway".into(),
                    progress: 0,
                }
            } else {
                let paths = self.library.session(self.id);
                let dir = SessionDir::new(self.id, StdFs, &paths.audio());
                match needs_salvage(&dir) {
                    Ok(false) => ProcessingState::Done,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => ProcessingState::Done,
                    Ok(true) => ProcessingState::Waiting {
                        reason: "audio still to save".into(),
                        progress: 0,
                    },
                    Err(e) => ProcessingState::Waiting {
                        reason: format!("saved audio can't be checked: {e}"),
                        progress: 0,
                    },
                }
            };
            let mut steps = vec![ProcessingJob {
                name: "Recording saved".into(),
                engine: "nota".into(),
                state: saved,
            }];
            let engine = final_text
                .first()
                .map(|text| format!("{} · {}", text.heard_by.engine, text.heard_by.model));
            let engine = engine
                .as_deref()
                .unwrap_or(if self.engines == "no live text" {
                    "speech engine"
                } else {
                    self.engines
                });
            steps.extend(jobs.into_iter().map(|job| step(job, engine)));
            Ok((
                session
                    .title
                    .unwrap_or_else(|| format!("session {}", self.id.get())),
                steps,
                transcript,
            ))
        })
    }
}

/// The live record has session times; final rows have only track-local samples.
/// Keep the chronological heard transcript. If none exists, label final text
/// by track rather than suggesting that sample indices align across tracks.
fn transcript(
    heard: Vec<String>,
    final_text: &[FinalText],
    tracks: &[nota_store::Track],
) -> Vec<String> {
    if !heard.is_empty() {
        return heard;
    }
    let mut lines = Vec::new();
    let mut previous = None;
    for text in final_text {
        let Some(words) = &text.text else {
            continue;
        };
        if previous != Some(text.track) {
            let name =
                tracks
                    .iter()
                    .find(|track| track.track == text.track)
                    .map(|track| match track.kind {
                        TrackKind::Microphone => "Microphone",
                        TrackKind::System => "System audio",
                    });
            lines.push(format!(
                "{} · final pass",
                name.map_or_else(|| format!("Track {}", text.track.get()), str::to_owned)
            ));
            previous = Some(text.track);
        }
        lines.push(words.clone());
    }
    lines
}

fn step(job: Job, engine: &str) -> ProcessingJob {
    let progress = if job.progress.total == 0 {
        0
    } else {
        u8::try_from(
            (u128::from(job.progress.done) * 100 / u128::from(job.progress.total)).min(100),
        )
        .unwrap_or(100)
    };
    let state = match job.state {
        JobState::Waiting(wait) => ProcessingState::Waiting {
            reason: match wait {
                Some(Wait::Engine) => "waiting for a working speech engine",
                Some(Wait::Audio) => "waiting for audio to be saved",
                Some(Wait::Space) => "waiting for free space",
                None => "queued or paused for a recording",
            }
            .into(),
            progress,
        },
        JobState::Running => ProcessingState::Running { progress },
        JobState::Done => ProcessingState::Done,
        JobState::Failed(reason) => ProcessingState::Failed { reason, progress },
    };
    ProcessingJob {
        name: match job.kind {
            JobKind::FinalPass => "Final transcript",
        }
        .into(),
        engine: engine.into(),
        state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nota_core::{FakeClock, Utterance};
    use nota_recorder::fs::Fs as _;
    use nota_store::{Heard, JobEnd, NewSession, Progress};

    #[test]
    fn stored_jobs_move_the_screen_and_heard_text_is_readable_before_the_pass() {
        let dir = crate::app::tests::TestDir::new("processing");
        let library = Library::open(&dir.0).unwrap();
        let id = SessionId::new(1);
        library
            .db()
            .with(|db| {
                db.create_session(&NewSession {
                    id,
                    title: Some("Joinery".into()),
                    language: None,
                    started_at: None,
                    tracks: vec![],
                })?;
                db.add_utterance(
                    id,
                    &Heard {
                        utterance: Utterance::new(
                            nota_core::TrackId::new(0),
                            SessionTime::ZERO,
                            SessionTime::from_nanos(1),
                            "Keep the board flat.".into(),
                        )
                        .unwrap(),
                        engine: "test".into(),
                        model: "test".into(),
                        words: vec![],
                    },
                )?;
                db.finish_recording(id, None)
            })
            .unwrap();
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
        let data = Data {
            library: &library,
            id,
            engines: "parakeet",
            clock: &clock,
            at: None,
        };
        let (title, waiting, transcript) = data.read().unwrap();
        assert_eq!(title, "Joinery");
        assert_eq!(transcript, ["Keep the board flat."]);
        assert!(matches!(waiting[1].state, ProcessingState::Waiting { .. }));
        let job = library
            .db()
            .with(|db| db.session_jobs(id))
            .unwrap()
            .remove(0);
        library
            .db()
            .with(|db| {
                db.start_job(job.id)?;
                db.job_progress(job.id, Progress { done: 2, total: 5 })
            })
            .unwrap();
        assert_eq!(
            data.read().unwrap().1[1].state,
            ProcessingState::Running { progress: 40 }
        );
        library
            .db()
            .with(|db| {
                db.add_final_text(
                    id,
                    nota_core::TrackId::new(0),
                    nota_core::SampleIndex::new(100),
                    &[FinalText {
                        track: nota_core::TrackId::new(0),
                        range: nota_core::SampleRange::new(
                            nota_core::SampleIndex::ZERO,
                            nota_core::SampleIndex::new(100),
                        )
                        .unwrap(),
                        text: Some("Final text has track-local samples.".into()),
                        words: vec![],
                        heard_by: nota_store::HeardBy {
                            engine: "stored-engine".into(),
                            model: "stored-model".into(),
                        },
                    }],
                )
            })
            .unwrap();
        library
            .db()
            .with(|db| db.end_job(job.id, &JobEnd::Done))
            .unwrap();
        assert_eq!(data.read().unwrap().1[1].state, ProcessingState::Done);
        assert_eq!(data.read().unwrap().2, transcript);
        assert_eq!(
            data.read().unwrap().1[1].engine,
            "stored-engine · stored-model"
        );
    }
    #[test]
    fn final_only_text_is_grouped_by_track_without_assuming_aligned_clocks() {
        use nota_core::{SampleIndex, SampleRange, TrackId};
        use nota_store::{HeardBy, Track};
        let text = |track, start, words: &str| FinalText {
            track: TrackId::new(track),
            range: SampleRange::new(SampleIndex::new(start), SampleIndex::new(start + 100))
                .unwrap(),
            text: Some(words.into()),
            words: vec![],
            heard_by: HeardBy {
                engine: "test".into(),
                model: "test".into(),
            },
        };
        let tracks = [
            Track {
                track: TrackId::new(0),
                kind: TrackKind::Microphone,
                source: None,
            },
            Track {
                track: TrackId::new(1),
                kind: TrackKind::System,
                source: None,
            },
        ];
        let finals = [
            text(0, 4800, "earlier mic speech"),
            text(1, 1600, "later system speech"),
        ];
        assert_eq!(
            transcript(vec![], &finals, &tracks),
            [
                "Microphone · final pass",
                "earlier mic speech",
                "System audio · final pass",
                "later system speech"
            ]
        );
        let heard = vec!["earlier mic speech".into(), "later system speech".into()];
        assert_eq!(transcript(heard.clone(), &finals, &tracks), heard);
    }

    #[test]
    fn live_or_unpublished_audio_does_not_claim_to_be_saved() {
        let dir = crate::app::tests::TestDir::new("processing-unsaved");
        let library = Library::open(&dir.0).unwrap();
        let paths = library.create().unwrap();
        library
            .db()
            .with(|db| db.create_session(&NewSession::bare(paths.id)))
            .unwrap();
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
        let data = Data {
            library: &library,
            id: paths.id,
            engines: "test",
            clock: &clock,
            at: None,
        };
        assert!(matches!(
            data.read().unwrap().1[0].state,
            ProcessingState::Waiting { .. }
        ));
        library
            .db()
            .with(|db| db.finish_recording(paths.id, None))
            .unwrap();
        assert_eq!(data.read().unwrap().1[0].state, ProcessingState::Done);
        drop(StdFs.create(&paths.audio().join("journal-000001")).unwrap());
        assert!(
            matches!(data.read().unwrap().1[0].state, ProcessingState::Waiting { ref reason, .. } if reason == "audio still to save")
        );
    }
}
