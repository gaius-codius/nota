//! Bridges the saved post-stop jobs and heard text to Processing.

use super::{
    Arc, BoxError, Clock, Duration, InputThread, Library, QuitSignals, RunError, Screen, SessionId,
    SessionTime, Theme, mpsc,
};
use nota_recorder::fs::StdFs;
use nota_recorder::segment::needs_salvage;
use nota_recorder::session::SessionDir;
use nota_store::{
    FinalLine, Job, JobKind, JobState, RevisionNumber, SessionState, StoreError, TrackKind, Wait,
};
use nota_tui::{
    Processing, ProcessingAction, ProcessingFailure, ProcessingJob, ProcessingState, ProcessingWait,
};
use std::io;

/// How often Processing reads the session again.
const REFRESH: Duration = Duration::from_secs(1);

/// The speech engines named on Processing's bottom border.
pub(super) enum EngineLine {
    /// The configured engine and model.
    Configured(
        /// The engine and model names shown on the page.
        String,
    ),
    /// No speech engine can be used.
    Unavailable,
}

impl EngineLine {
    /// The words shown when no final text names an engine yet.
    fn label(&self) -> &str {
        match self {
            Self::Configured(label) => label,
            Self::Unavailable => "speech engine unavailable",
        }
    }
}

/// Opens a session on Processing until it asks for another page.
pub(super) fn show(
    screen: &mut Screen,
    library: &Library,
    id: SessionId,
    engines: &EngineLine,
    theme: Theme,
    clock: &Arc<dyn Clock>,
    quit: &QuitSignals,
) -> Result<ProcessingAction, BoxError> {
    let mut page = Processing::new(
        format!("session {}", id.get()),
        engines.label().into(),
        theme,
    );
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
    let ran = if quit.asked() {
        Ok(ProcessingAction::Quit)
    } else {
        run(screen, &mut page, &mut data, ui, &events, clock)
    };
    quit.show_home(None);
    ran
}

/// Runs the page with input timed by the app clock.
fn run(
    screen: &mut Screen,
    page: &mut Processing,
    data: &mut Data<'_>,
    ui: mpsc::Sender<nota_tui::Event>,
    events: &mpsc::Receiver<nota_tui::Event>,
    clock: &Arc<dyn Clock>,
) -> Result<ProcessingAction, BoxError> {
    let input = InputThread::spawn(ui, Arc::clone(clock))?;
    let ran = screen.clear().map_err(RunError::Terminal).and_then(|()| {
        nota_tui::run_processing(screen.terminal(), page, events, &mut |page| {
            data.refresh(page);
        })
    });
    let _ = input.stop();
    match ran {
        Ok(action) => Ok(action),
        Err(RunError::Terminal(e)) => Err(format!("the screen failed: {e}").into()),
        Err(RunError::InputLost(kind)) => Err(format!("the keyboard was lost: {kind}").into()),
        Err(RunError::CommandsClosed(_)) => Ok(ProcessingAction::Quit),
    }
}

/// The library values shown on Processing.
struct SessionView {
    /// The session's title.
    title: String,
    /// Its saved recording and the jobs that follow it.
    jobs: Vec<ProcessingJob>,
    /// The heard text, or final text if none was heard live.
    transcript: Vec<String>,
}

/// Reads one session again while Processing is open.
struct Data<'a> {
    /// The library holding this session.
    library: &'a Library,
    /// The session being shown.
    id: SessionId,
    /// The engines to name before final text is available.
    engines: &'a EngineLine,
    /// The app's clock.
    clock: &'a Arc<dyn Clock>,
    /// When the library was last read.
    at: Option<SessionTime>,
}

impl Data<'_> {
    /// Reads the session when due, keeping the page usable if reading fails.
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
            Ok(view) => {
                page.set_title(view.title);
                page.set_jobs(view.jobs);
                page.set_transcript(view.transcript);
                page.set_notice(None);
            }
            Err(e) => page.set_notice(Some(format!("the session couldn't be read: {e}"))),
        }
    }

    /// Reads the title, steps and words for this session.
    fn read(&self) -> Result<SessionView, StoreError> {
        self.library.db().with(|db| {
            let session = db.session(self.id)?.ok_or(StoreError::NoSession(self.id))?;
            let final_text = match db.revision(self.id, RevisionNumber::FINAL) {
                Ok(lines) => lines,
                // There's no final text until the pass stores its first.
                Err(StoreError::NoRevision(..)) => Vec::new(),
                Err(e) => return Err(e),
            };
            let heard = db
                .utterances(self.id)?
                .into_iter()
                .map(|text| text.heard.utterance.into_text())
                .collect();
            Ok(SessionView {
                title: session
                    .title
                    .unwrap_or_else(|| format!("session {}", self.id.get())),
                jobs: self.steps(session.state, db.session_jobs(self.id)?, &final_text),
                transcript: transcript(heard, &final_text, &db.tracks(self.id)?),
            })
        })
    }

    /// Whether the recording has stopped and all its audio is saved.
    fn saved(&self, state: SessionState) -> ProcessingState {
        if state == SessionState::Recording {
            return waiting(ProcessingWait::Recording);
        }
        let paths = self.library.session(self.id);
        let dir = SessionDir::new(self.id, StdFs, &paths.audio());
        match needs_salvage(&dir) {
            Ok(false) => ProcessingState::Done,
            Err(e) if e.kind() == io::ErrorKind::NotFound => ProcessingState::Done,
            Ok(true) => waiting(ProcessingWait::AudioSaving),
            Err(e) => waiting(ProcessingWait::AudioUnchecked(e.to_string())),
        }
    }

    /// The saved recording followed by its queued work.
    fn steps(
        &self,
        state: SessionState,
        jobs: Vec<Job>,
        final_text: &[FinalLine],
    ) -> Vec<ProcessingJob> {
        let mut steps = vec![ProcessingJob {
            name: "Recording saved".into(),
            engine: "nota".into(),
            state: self.saved(state),
        }];
        let engine = final_text
            .first()
            .map(|line| &line.heard.heard_by)
            .map(|by| format!("{} · {}", by.engine, by.model));
        let engine = engine.as_deref().unwrap_or_else(|| self.engines.label());
        steps.extend(jobs.into_iter().map(|job| step(job, engine)));
        steps
    }
}

/// A step that cannot run yet, with no progress to show.
fn waiting(reason: ProcessingWait) -> ProcessingState {
    ProcessingState::Waiting {
        reason,
        progress: 0,
    }
}

/// The heard text, in session order, or if nothing was heard live the final
/// pass's text, as revision 0 gives it: in session order across tracks,
/// with chunks it can't place in time after the rest. Each run of one
/// track's text is headed by the track's name, since the tracks
/// interleave. Audio the engine couldn't transcribe has no line.
fn transcript(
    heard: Vec<String>,
    final_text: &[FinalLine],
    tracks: &[nota_store::Track],
) -> Vec<String> {
    if !heard.is_empty() {
        return heard;
    }
    let mut lines = Vec::new();
    let mut previous = None;
    for line in final_text {
        let Some(words) = &line.text else {
            continue;
        };
        let track = line.heard.track;
        if previous != Some(track) {
            lines.push(format!("{} · final pass", track_name(track, tracks)));
            previous = Some(track);
        }
        lines.push(words.clone());
    }
    lines
}

/// The track name beside final words, or its number if it is unknown.
fn track_name(id: nota_core::TrackId, tracks: &[nota_store::Track]) -> String {
    tracks.iter().find(|track| track.track == id).map_or_else(
        || format!("Track {}", id.get()),
        |track| match track.kind {
            TrackKind::Microphone => "Microphone".into(),
            TrackKind::System => "System audio".into(),
        },
    )
}

/// A stored job as one Processing step.
fn step(job: Job, engine: &str) -> ProcessingJob {
    let progress = progress(job.progress);
    let state = match job.state {
        JobState::Waiting(wait) => ProcessingState::Waiting {
            reason: match wait {
                Some(Wait::Engine) => ProcessingWait::Engine,
                Some(Wait::Audio) => ProcessingWait::Audio,
                Some(Wait::Space) => ProcessingWait::Space,
                None => ProcessingWait::Queued,
            },
            progress,
        },
        JobState::EngineDown(reason) => ProcessingState::Waiting {
            reason: ProcessingWait::EngineDown(reason),
            progress,
        },
        JobState::Running => ProcessingState::Running { progress },
        JobState::Retrying(reason) => ProcessingState::Retrying { progress, reason },
        JobState::Done => ProcessingState::Done,
        JobState::Failed(reason) => ProcessingState::Failed {
            reason: ProcessingFailure::new(reason),
            progress,
        },
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

/// The completed fraction in percent, limited to 100.
fn progress(progress: nota_store::Progress) -> u8 {
    if progress.total == 0 {
        return 0;
    }
    u8::try_from((u128::from(progress.done) * 100 / u128::from(progress.total)).min(100))
        .unwrap_or(100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nota_core::{
        Drift, EpochAnchor, EpochId, FakeClock, SampleCount, SampleIndex, SampleRange, SampleRate,
        TrackId, Utterance,
    };
    use nota_recorder::fs::Fs as _;
    use nota_store::{
        FinalText, Heard, HeardBy, JobEnd, NewSession, Progress, SegmentRow, Sha256Digest, Track,
    };

    const MIC: TrackId = TrackId::new(0);
    const SYSTEM: TrackId = TrackId::new(1);

    /// A library with one session, kept until each test ends.
    struct Fixture {
        /// The directory kept while its library is open.
        _dir: crate::app::tests::TestDir,
        /// The library being read.
        library: Library,
        /// The session being shown.
        id: SessionId,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            Self::create(name, Vec::new())
        }

        /// A session recorded on the microphone and system tracks.
        fn with_tracks(name: &str) -> Self {
            Self::create(name, tracks())
        }

        /// A session titled "Joinery" with `tracks`.
        fn create(name: &str, tracks: Vec<Track>) -> Self {
            let dir = crate::app::tests::TestDir::new(name);
            let library = Library::open(&dir.0).unwrap();
            let paths = library.create().unwrap();
            library
                .db()
                .with(|db| {
                    db.create_session(&NewSession {
                        title: Some("Joinery".into()),
                        tracks,
                        ..NewSession::bare(paths.id)
                    })
                })
                .unwrap();
            Self {
                _dir: dir,
                library,
                id: paths.id,
            }
        }

        fn read(&self) -> SessionView {
            let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
            Data {
                library: &self.library,
                id: self.id,
                engines: &EngineLine::Configured("parakeet".into()),
                clock: &clock,
                at: None,
            }
            .read()
            .unwrap()
        }

        fn finish(&self) -> nota_store::JobId {
            self.library
                .db()
                .with(|db| {
                    db.finish_recording(self.id, None)?;
                    Ok(db.session_jobs(self.id)?.remove(0).id)
                })
                .unwrap()
        }

        fn heard(&self) {
            self.library
                .db()
                .with(|db| {
                    db.add_utterance(
                        self.id,
                        &Heard {
                            utterance: Utterance::new(
                                TrackId::new(0),
                                SessionTime::ZERO,
                                SessionTime::from_nanos(1),
                                "Keep the board flat.".into(),
                            )
                            .unwrap(),
                            engine: "test".into(),
                            model: "test".into(),
                            words: vec![],
                        },
                    )
                })
                .unwrap();
        }

        fn final_text(&self) {
            self.library
                .db()
                .with(|db| {
                    db.add_final_text(
                        self.id,
                        TrackId::new(0),
                        SampleIndex::new(100),
                        &[text(TrackId::new(0), SampleIndex::ZERO, "Final words.")],
                    )
                })
                .unwrap();
        }
    }

    fn text(track: TrackId, start: SampleIndex, words: &str) -> FinalText {
        FinalText {
            track,
            range: SampleRange::new(start, start.checked_add(SampleCount::new(100)).unwrap())
                .unwrap(),
            text: Some(words.into()),
            words: vec![],
            heard_by: HeardBy {
                engine: "stored-engine".into(),
                model: "stored-model".into(),
            },
        }
    }

    /// Processing reads the title saved in the library.
    #[test]
    fn session_title_is_read_from_the_library() {
        // A named session gives the screen a title to read.
        let fixture = Fixture::new("session_title_is_read_from_the_library");
        assert_eq!(fixture.read().title, "Joinery");
    }

    /// Heard text can be read while the final pass waits.
    #[test]
    fn heard_text_is_readable_before_the_pass() {
        let fixture = Fixture::new("heard_text_is_readable_before_the_pass");
        // Stopping queues a pass, but the live words are already there.
        fixture.heard();
        fixture.finish();
        assert_eq!(fixture.read().transcript, ["Keep the board flat."]);
    }

    /// A queued job keeps its waiting reason as a value.
    #[test]
    fn queued_job_has_a_typed_waiting_reason() {
        let fixture = Fixture::new("queued_job_has_a_typed_waiting_reason");
        // A stop queues the pass without starting it.
        fixture.finish();
        assert_eq!(
            fixture.read().jobs[1].state,
            ProcessingState::Waiting {
                reason: ProcessingWait::Queued,
                progress: 0
            }
        );
    }

    /// A running job shows the fraction reported by the worker.
    #[test]
    fn running_job_shows_its_progress() {
        let fixture = Fixture::new("running_job_shows_its_progress");
        let job = fixture.finish();
        // The worker has finished two of its five parts.
        fixture
            .library
            .db()
            .with(|db| {
                db.start_job(job)?;
                db.job_progress(job, Progress { done: 2, total: 5 })
            })
            .unwrap();
        assert_eq!(
            fixture.read().jobs[1].state,
            ProcessingState::Running { progress: 40 }
        );
    }

    /// A completed job shows Done on the next read.
    #[test]
    fn completed_job_shows_done() {
        let fixture = Fixture::new("completed_job_shows_done");
        let job = fixture.finish();
        // Starting and finishing the worker follows the job's allowed steps.
        fixture
            .library
            .db()
            .with(|db| {
                db.start_job(job)?;
                db.end_job(job, &JobEnd::Done)
            })
            .unwrap();
        assert_eq!(fixture.read().jobs[1].state, ProcessingState::Done);
    }

    /// Final text names the engine that heard it.
    #[test]
    fn final_text_names_its_stored_engine() {
        let fixture = Fixture::new("final_text_names_its_stored_engine");
        fixture.finish();
        // The final pass may have used a different engine from today's setting.
        fixture.final_text();
        assert_eq!(
            fixture.read().jobs[1].engine,
            "stored-engine · stored-model"
        );
    }

    /// Final rows do not replace the chronological heard text.
    #[test]
    fn heard_text_stays_after_the_final_pass() {
        let fixture = Fixture::new("heard_text_stays_after_the_final_pass");
        fixture.heard();
        fixture.finish();
        // Different final words make a mistaken replacement visible.
        fixture.final_text();
        assert_eq!(fixture.read().transcript, ["Keep the board flat."]);
    }

    /// The microphone and system tracks, named as the library stores them.
    fn tracks() -> Vec<Track> {
        vec![
            Track {
                track: MIC,
                kind: TrackKind::Microphone,
                source: None,
            },
            Track {
                track: SYSTEM,
                kind: TrackKind::System,
                source: None,
            },
        ]
    }

    /// `track`'s first epoch, from its sample `first`, starting `secs`
    /// seconds into the session.
    fn anchor(first: u64, secs: u64) -> EpochAnchor {
        EpochAnchor {
            id: EpochId::new(0),
            start: SessionTime::from_nanos(secs * 1_000_000_000),
            first_sample: SampleIndex::new(first),
            rate: SampleRate::SPEECH,
            drift: Drift::ZERO,
        }
    }

    /// Publishes `track`'s audio from `a` to `b` in its first epoch.
    fn segment(fixture: &Fixture, track: TrackId, a: u64, b: u64) {
        let row = SegmentRow::new(
            track,
            EpochId::new(0),
            SampleRange::new(SampleIndex::new(a), SampleIndex::new(b)).unwrap(),
            Sha256Digest::new([0; 32]),
        )
        .unwrap();
        fixture
            .library
            .db()
            .with(|db| db.insert_segment(fixture.id, &row))
            .unwrap();
    }

    /// Stores `texts` as `track`'s final text, each 100 samples long.
    fn finals(fixture: &Fixture, track: TrackId, texts: &[(u64, &str)]) {
        let rows: Vec<FinalText> = texts
            .iter()
            .map(|&(start, words)| text(track, SampleIndex::new(start), words))
            .collect();
        fixture
            .library
            .db()
            .with(|db| db.add_final_text(fixture.id, track, SampleIndex::new(100_000), &rows))
            .unwrap();
    }

    /// Acceptance: final-only text from two tracks reads in session order,
    /// the tracks interleaved, each run headed by its track's name.
    #[test]
    fn final_only_text_interleaves_its_tracks_in_session_order() {
        let fixture = Fixture::with_tracks("final_only_text_interleaves");
        // The system track's samples start later on its own clock, so
        // sample order alone would put it first or last, never between.
        for (track, epoch) in [(MIC, anchor(0, 1)), (SYSTEM, anchor(16_000, 2))] {
            fixture
                .library
                .db()
                .with(|db| db.insert_epoch(fixture.id, track, &epoch))
                .unwrap();
        }
        segment(&fixture, MIC, 0, 100_000);
        segment(&fixture, SYSTEM, 16_000, 100_000);
        finals(
            &fixture,
            MIC,
            &[(0, "mark the face side"), (48_000, "now plane it")],
        );
        finals(
            &fixture,
            SYSTEM,
            &[(32_000, "the clip from the last session")],
        );
        assert_eq!(
            fixture.read().transcript,
            [
                "Microphone · final pass",
                "mark the face side",
                "System audio · final pass",
                "the clip from the last session",
                "Microphone · final pass",
                "now plane it",
            ]
        );
    }

    /// Acceptance: a chunk whose epoch has no anchor reads after the timed
    /// ones, though its samples come first.
    #[test]
    fn final_text_without_a_session_time_reads_last() {
        let fixture = Fixture::with_tracks("final_text_without_a_session_time");
        // Only the system track's epoch is anchored.
        fixture
            .library
            .db()
            .with(|db| db.insert_epoch(fixture.id, SYSTEM, &anchor(0, 5)))
            .unwrap();
        segment(&fixture, MIC, 0, 100_000);
        segment(&fixture, SYSTEM, 0, 100_000);
        finals(&fixture, MIC, &[(0, "said first, but untimed")]);
        finals(&fixture, SYSTEM, &[(16_000, "placed in time")]);
        assert_eq!(
            fixture.read().transcript,
            [
                "System audio · final pass",
                "placed in time",
                "Microphone · final pass",
                "said first, but untimed",
            ]
        );
    }

    /// A stopped session the final pass hasn't stored text for yet reads
    /// with no transcript, not as an error.
    #[test]
    fn a_session_without_final_text_reads_empty() {
        let fixture = Fixture::new("a_session_without_final_text_reads_empty");
        // Stopped, so the pass is queued but has stored nothing.
        fixture.finish();
        assert!(fixture.read().transcript.is_empty());
    }

    /// Acceptance: a pass waiting after its engine failed shows why.
    #[test]
    fn an_engine_failure_shows_its_reason_while_waiting() {
        let fixture = Fixture::new("an_engine_failure_shows_its_reason");
        let job = fixture.finish();
        // The pass ran and its engine couldn't start.
        fixture
            .library
            .db()
            .with(|db| {
                db.start_job(job)?;
                db.end_job(job, &JobEnd::EngineDown("couldn't start: gone".into()))
            })
            .unwrap();
        assert_eq!(
            fixture.read().jobs[1].state,
            ProcessingState::Waiting {
                reason: ProcessingWait::EngineDown("couldn't start: gone".into()),
                progress: 0
            }
        );
    }

    /// The retry after an engine failure keeps showing the reason while
    /// it runs.
    #[test]
    fn a_retry_after_an_engine_failure_shows_the_reason() {
        let fixture = Fixture::new("a_retry_after_an_engine_failure");
        let job = fixture.finish();
        // Failed once, now running again.
        fixture
            .library
            .db()
            .with(|db| {
                db.start_job(job)?;
                db.end_job(job, &JobEnd::EngineDown("exited: no model".into()))?;
                db.start_job(job)
            })
            .unwrap();
        assert_eq!(
            fixture.read().jobs[1].state,
            ProcessingState::Retrying {
                progress: 0,
                reason: "exited: no model".into()
            }
        );
    }

    /// An open recording cannot yet be labelled saved.
    #[test]
    fn live_audio_waits_for_recording_to_end() {
        // A new session still has its recording open.
        let fixture = Fixture::new("live_audio_waits_for_recording_to_end");
        assert_eq!(
            fixture.read().jobs[0].state,
            ProcessingState::Waiting {
                reason: ProcessingWait::Recording,
                progress: 0
            }
        );
    }

    /// A stopped recording with no journal is saved.
    #[test]
    fn stopped_audio_without_a_journal_is_saved() {
        let fixture = Fixture::new("stopped_audio_without_a_journal_is_saved");
        // There is no remaining journal for the publisher to finish.
        fixture.finish();
        assert_eq!(fixture.read().jobs[0].state, ProcessingState::Done);
    }

    /// A remaining journal keeps the saved step waiting.
    #[test]
    fn unpublished_audio_waits_to_be_saved() {
        let fixture = Fixture::new("unpublished_audio_waits_to_be_saved");
        fixture.finish();
        // A journal still on disk means some audio has not been published.
        let paths = fixture.library.session(fixture.id);
        drop(StdFs.create(&paths.audio().join("journal-000001")).unwrap());
        assert_eq!(
            fixture.read().jobs[0].state,
            ProcessingState::Waiting {
                reason: ProcessingWait::AudioSaving,
                progress: 0
            }
        );
    }

    /// The unavailable engine line has a separate state from its wording.
    #[test]
    fn unavailable_engine_line_has_its_display_words() {
        // No engine is configured, so the screen says why it cannot run.
        assert_eq!(EngineLine::Unavailable.label(), "speech engine unavailable");
    }
}
