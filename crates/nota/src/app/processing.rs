//! Bridges the persisted post-stop jobs and heard text to Processing.

use super::{
    Arc, BoxError, Clock, Duration, InputThread, Library, QuitSignals, RunError, Screen, SessionId,
    SessionTime, Theme, mpsc,
};
use nota_store::{Job, JobKind, JobState, StoreError, Wait};
use nota_tui::{Processing, ProcessingAction, ProcessingJob, ProcessingState};

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
            let done = jobs
                .iter()
                .any(|job| job.kind == JobKind::FinalPass && job.state == JobState::Done);
            let mut final_text = if done {
                db.final_texts(self.id)?
            } else {
                Vec::new()
            };
            final_text.sort_by_key(|text| (text.range.start(), text.track));
            let mut transcript: Vec<_> = final_text
                .into_iter()
                .filter_map(|text| text.text)
                .collect();
            if transcript.is_empty() {
                transcript = db
                    .utterances(self.id)?
                    .into_iter()
                    .map(|text| text.heard.utterance.into_text())
                    .collect();
            }
            let mut steps = vec![ProcessingJob {
                name: "Recording saved".into(),
                engine: "nota".into(),
                state: ProcessingState::Done,
            }];
            steps.extend(jobs.into_iter().map(|job| step(job, self.engines)));
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

fn step(job: Job, engine: &str) -> ProcessingJob {
    let state = match job.state {
        JobState::Waiting(wait) => ProcessingState::Waiting {
            reason: match wait {
                Some(Wait::Engine) => "waiting for a working speech engine",
                Some(Wait::Audio) => "waiting for audio to be saved",
                Some(Wait::Space) => "waiting for free space",
                None => "queued or paused for a recording",
            }
            .into(),
        },
        JobState::Running => ProcessingState::Running {
            progress: if job.progress.total == 0 {
                0
            } else {
                u8::try_from(
                    (u128::from(job.progress.done) * 100 / u128::from(job.progress.total)).min(100),
                )
                .unwrap_or(100)
            },
        },
        JobState::Done => ProcessingState::Done,
        JobState::Failed(reason) => ProcessingState::Failed { reason },
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
            .with(|db| db.end_job(job.id, &JobEnd::Done))
            .unwrap();
        assert_eq!(data.read().unwrap().1[1].state, ProcessingState::Done);
        assert_eq!(data.read().unwrap().2, transcript);
    }
}
