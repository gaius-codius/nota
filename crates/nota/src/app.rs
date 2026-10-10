//! `nota` with no command: the Home screen, and the flow between screens.
//!
//! ```text
//! Home ──R──▶ Recording ──s, y──▶ Processing ──esc──▶ Home
//! ```
//!
//! The terminal is set up once, for Home, and lent to each recording: the
//! Recording screen runs on it, and Home shows "finishing the recording"
//! while the recording stops, then Processing shows its jobs and transcript.
//! Home opens a session in Processing too. Until Setup exists, `R`
//! records with the last session's settings.
//!
//! At start, sessions an earlier run left are salvaged, as `nota record`
//! does. While Home is open, the jobs queued after each stop (the final
//! pass) run in the background, never while a recording is going, here or
//! in another nota (see `jobs`). What each recording's stop reports, and anything that went wrong,
//! is said on stderr once nota closes, as `nota record` says it.
//!
//! SIGHUP, SIGTERM and SIGINT close nota wherever it is: Home closes at
//! once, and a recording stops in order first. The app listens for them
//! from before the terminal is set up until it's restored
//! ([`QuitSignals`]).

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use nota_core::recorder::{Command, Input, Setup};
use nota_core::{Clock, SessionId, SessionTime, SystemClock, WallTime, wall_now};
use nota_tui::{Action, Home, InputThread, RunError, Session, Status, Theme};

use crate::final_pass::Jobs;
use crate::jobs::{Capture, FreeSpace, Runner};
use crate::library::{Library, Listed, Needs, Salvaged};
use crate::record::{
    BoxError, Lent, QuitSignals, RATE, RecordArgs, final_engine, last_setup, record_in,
    segment_length,
};
use crate::terminal::Screen;

mod processing;

/// What Home shows while a recording stops.
const STOPPING: &str = "finishing the recording";

/// How often the runner looks for a recording in another nota, at most.
const ELSEWHERE: Duration = Duration::from_secs(1);

/// How often Home lists the sessions again while it's open, so a status
/// another nota changes (a recording stopping, a salvage finishing) shows.
/// Listing reads the library database and each session's directory, so
/// it isn't done at every draw.
const RELIST: Duration = Duration::from_secs(5);

/// Runs Home until it's closed, recording each time `R` is pressed.
/// `args` says where the library is and how to record; its start command
/// is replaced by each recording's. SIGHUP, SIGTERM and SIGINT close nota,
/// on Home or during a recording (which stops in order first).
///
/// Returns what to say once the terminal is restored: each recording's
/// outcome and notes, as `nota record` gives them, even if nota then
/// failed.
///
/// # Errors
///
/// If the library can't be opened, or the terminal can't be set up or
/// fails.
pub(crate) fn app(args: &RecordArgs) -> (Vec<String>, Result<(), BoxError>) {
    let mut said = Vec::new();
    let ran = run_app(args, &mut said);
    (said, ran)
}

fn run_app(args: &RecordArgs, said: &mut Vec<String>) -> Result<(), BoxError> {
    // Before the terminal is set up: a signal from here on closes nota in
    // order, never leaving the terminal raw.
    let quit = QuitSignals::listen()?;
    let library = Library::open(&args.data)?;
    let salvaged = library.salvage_all(segment_length())?;
    let clock: Arc<dyn Clock> =
        Arc::new(SystemClock::start().map_err(|_| "the system clock can't be read")?);
    let theme = Theme::load();
    let engines = engines(args);
    let processing_engine = processing_engine(args);
    // Set while this nota records: no job runs then.
    let here = Arc::new(AtomicBool::new(false));
    let (runner, mut notice) = start_jobs(args, &library, &clock, &here);
    let jobs = Background { runner, here };
    let mut listing = Listing {
        library: &library,
        salvaged: &salvaged,
        zone: TimeZone::system,
        clock: Arc::clone(&clock),
        wall: Box::new(wall_now),
        listed_at: None,
        said: None,
    };
    let mut home = Home::new(listing.sessions()?, engines, theme);
    home.set_notice(notice.take());
    Pages {
        listing,
        home,
        engine: processing_engine,
        theme,
        quit: &quit,
        notice: None,
    }
    .run(args, said, &jobs)
}

/// The final pass engine, kept as a state until the page draws its label.
fn processing_engine(args: &RecordArgs) -> processing::EngineLine {
    final_engine(args).ok().flatten().map_or_else(
        || processing::EngineLine::Unavailable,
        |engine| {
            processing::EngineLine::Configured(format!(
                "{} · {}",
                engine.heard_by.engine, engine.heard_by.model
            ))
        },
    )
}

/// Home kept between visits to Processing and Recording.
struct Pages<'a> {
    /// The library's list, refreshed while Home is open.
    listing: Listing<'a>,
    /// The page also shown while a recording stops.
    home: Home,
    /// The engine named on Processing.
    engine: processing::EngineLine,
    /// The screen colours.
    theme: Theme,
    /// Signals that close the app.
    quit: &'a QuitSignals,
    /// A recording failure to show on the next visit to Home.
    notice: Option<String>,
}

impl Pages<'_> {
    /// Visits each page until nota closes.
    fn run(
        &mut self,
        args: &RecordArgs,
        said: &mut Vec<String>,
        jobs: &Background,
    ) -> Result<(), BoxError> {
        let mut screen = None;
        let mut processing_session = None;
        while !self.quit.asked() {
            let mut current = match screen.take() {
                Some(screen) => screen,
                None => Screen::enter(None)?,
            };
            match self.action(&mut current, &mut processing_session)? {
                nota_tui::ProcessingAction::Quit => return Ok(()),
                nota_tui::ProcessingAction::Home => {
                    screen = Some(current);
                    continue;
                }
                nota_tui::ProcessingAction::Record => {}
            }
            match self.record(args, current, said, jobs) {
                Recorded::Back(back, id) => {
                    screen = Some(back);
                    processing_session = Some(id);
                }
                Recorded::Failed(e) => self.notice = Some(format!("the recording failed: {e}")),
                Recorded::TerminalGone => return Ok(()),
            }
        }
        Ok(())
    }

    /// Starts a recording with the kept Home page for its stop.
    fn record(
        &mut self,
        args: &RecordArgs,
        screen: Screen,
        said: &mut Vec<String>,
        jobs: &Background,
    ) -> Recorded {
        self.listing.relist(&mut self.home);
        record_from(
            args,
            self.listing.library,
            screen,
            &mut self.home,
            said,
            jobs,
        )
    }

    /// Shows Processing or Home and keeps Home's list for a later stop.
    fn action(
        &mut self,
        screen: &mut Screen,
        processing_session: &mut Option<SessionId>,
    ) -> Result<nota_tui::ProcessingAction, BoxError> {
        if let Some(id) = processing_session.take() {
            return processing::show(
                screen,
                self.listing.library,
                id,
                &self.engine,
                self.theme,
                &self.listing.clock,
                self.quit,
            );
        }
        self.home.set_busy(None);
        if let Some(notice) = self.notice.take() {
            self.home.set_notice(Some(notice));
        }
        self.listing.relist(&mut self.home);
        Ok(
            match show_home(screen, &mut self.home, &mut self.listing, self.quit)? {
                Action::Quit => nota_tui::ProcessingAction::Quit,
                Action::Record => nota_tui::ProcessingAction::Record,
                Action::Open(id) => {
                    *processing_session = Some(SessionId::new(id));
                    nota_tui::ProcessingAction::Home
                }
            },
        )
    }
}

/// The jobs that run after each stop, and whether this nota is recording.
struct Background {
    /// `None` if they can't run.
    runner: Option<Runner>,
    /// Whether this nota is recording.
    here: Arc<AtomicBool>,
}

/// Starts the job runner, or says why it couldn't be.
fn start_jobs(
    args: &RecordArgs,
    library: &Library,
    clock: &Arc<dyn Clock>,
    here: &Arc<AtomicBool>,
) -> (Option<Runner>, Option<String>) {
    let cant = |e: io::Error| format!("the work after a recording can't run: {e}");
    let engine = match final_engine(args) {
        Ok(engine) => engine,
        Err(e) => return (None, Some(cant(e))),
    };
    let capture = Recordings {
        library: library.clone(),
        here: Arc::clone(here),
        clock: Arc::clone(clock),
        elsewhere: Mutex::new(None),
    };
    match Runner::spawn(
        &args.data,
        library.db().clone(),
        Jobs::new(library.clone(), engine, Arc::clone(clock)),
        capture,
        FreeSpace(args.data.clone()),
        Arc::clone(clock),
    ) {
        Ok(runner) => (Some(runner), None),
        Err(e) => (None, Some(cant(e))),
    }
}

/// Whether a recording is going: this nota's, known at once, or another
/// nota's, found by its locked session ([`Library::recording`]) at most
/// every [`ELSEWHERE`].
struct Recordings {
    /// The library checked for another recording.
    library: Library,
    /// Whether this nota is recording.
    here: Arc<AtomicBool>,
    /// Times the interval between checks.
    clock: Arc<dyn Clock>,
    /// When another nota was last looked for, and whether one was
    /// recording.
    elsewhere: Mutex<Option<(SessionTime, bool)>>,
}

impl Capture for Recordings {
    fn recording(&self) -> bool {
        if self.here.load(Ordering::SeqCst) {
            return true;
        }
        let now = self.clock.now();
        let mut seen = self
            .elsewhere
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some((at, was)) = *seen
            && now.checked_duration_since(at).unwrap_or_default() < ELSEWHERE
        {
            return was;
        }
        let is = self.library.recording();
        *seen = Some((now, is));
        is
    }
}

/// How a recording from Home went, for what comes next.
enum Recorded {
    /// It was recorded, and the terminal is back for Home.
    Back(Screen, SessionId),
    /// It failed: the terminal was restored, and Home says why.
    Failed(String),
    /// It was recorded, but the terminal failed (as after a hangup):
    /// there's nothing to show Home on.
    TerminalGone,
}

/// Records with the last session's settings on the app's terminal, `home`
/// showing while it stops, and adds what it reported to `said`. No job
/// runs meanwhile; once it has stopped, its own are queued and the runner
/// is woken for them.
fn record_from(
    args: &RecordArgs,
    library: &Library,
    screen: Screen,
    home: &mut Home,
    said: &mut Vec<String>,
    jobs: &Background,
) -> Recorded {
    let mut record = args.clone();
    record.start = Command::Start(last_setup(library).unwrap_or_else(first_setup));
    let mut stopping = |screen: &mut Screen| {
        stopping_home(home);
        // Only a courtesy: if drawing fails, the next draw says so.
        let _ = screen.clear();
        let _ = screen.terminal().draw(|frame| home.draw(frame));
    };
    let lent = Lent {
        screen,
        library: library.clone(),
        stopping: &mut stopping,
    };
    jobs.here.store(true, Ordering::SeqCst);
    // A job running now stops within a moment; the recording doesn't wait
    // for it.
    if let Some(runner) = &jobs.runner {
        runner.wake();
    }
    let recorded = record_in(&record, Some(lent));
    jobs.here.store(false, Ordering::SeqCst);
    if let Some(runner) = &jobs.runner {
        runner.wake();
    }
    recorded_result(recorded, said)
}

/// Keeps the recording's result for the next page and the closing report.
fn recorded_result(
    recorded: Result<(crate::record::Outcome, Option<Screen>), BoxError>,
    said: &mut Vec<String>,
) -> Recorded {
    match recorded {
        Ok((outcome, back)) => {
            said.push(format!(
                "nota: recorded to {} ({} segments)",
                outcome.session.display(),
                outcome.segments
            ));
            said.extend(outcome.notes.iter().map(|note| format!("  {note}")));
            back.map_or(Recorded::TerminalGone, |screen| {
                Recorded::Back(screen, outcome.id)
            })
        }
        Err(e) => {
            said.push(format!("nota: the recording failed: {e}"));
            Recorded::Failed(e.to_string())
        }
    }
}

/// Runs Home on `screen` until it asks for something, or a signal closes
/// it.
fn show_home(
    screen: &mut Screen,
    home: &mut Home,
    listing: &mut Listing<'_>,
    quit: &QuitSignals,
) -> Result<Action, BoxError> {
    let (ui, ui_events) = mpsc::channel();
    quit.show_home(Some(ui.clone()));
    // A signal before Home was there to take it.
    if quit.asked() {
        quit.show_home(None);
        return Ok(Action::Quit);
    }
    let input = InputThread::spawn(ui, Arc::clone(&listing.clock));
    let ran = input.map_err(BoxError::from).and_then(|input| {
        // A new screen: drawn whole, not as changes to the last one's cells.
        let ran = screen.clear().map_err(RunError::Terminal).and_then(|()| {
            nota_tui::run_home(screen.terminal(), home, &ui_events, &mut |home| {
                listing.refresh(home);
            })
        });
        let _ = input.stop();
        match ran {
            Ok(action) => Ok(action),
            Err(RunError::Terminal(e)) => Err(format!("the screen failed: {e}").into()),
            Err(RunError::InputLost(kind)) => Err(format!("the keyboard was lost: {kind}").into()),
            Err(RunError::CommandsClosed(_)) => Ok(Action::Quit),
        }
    });
    quit.show_home(None);
    ran
}

/// The setup of a first recording: both tracks from the system's default
/// devices.
fn first_setup() -> Setup {
    Setup {
        title: "Recording".to_owned(),
        mic: Input::Default,
        system: Input::Default,
    }
}

/// The engines named on Home's bottom border.
const fn engines(args: &RecordArgs) -> &'static str {
    if args.models.is_some() {
        "parakeet"
    } else {
        "no live text"
    }
}

/// Home's list of the sessions, and when it was last read, so it can be
/// read again while Home stays open: every [`RELIST`], and as soon as the
/// local date changes, so `today` moves on at midnight.
struct Listing<'a> {
    /// The library whose sessions are shown.
    library: &'a Library,
    /// What salvage did at start.
    salvaged: &'a [Salvaged],
    /// The local time zone, read at each use so a change to it is seen.
    zone: fn() -> TimeZone,
    /// Times the interval between listings.
    clock: Arc<dyn Clock>,
    /// The calendar's time, which says what day it is.
    wall: Box<dyn Fn() -> Option<WallTime>>,
    /// When the sessions were last listed, or a listing last failed, by
    /// `clock`, and the local date then, if the calendar's time could be
    /// read.
    listed_at: Option<(SessionTime, Option<Date>)>,
    /// While listing fails, the notice that said so on Home, once: it isn't
    /// said again at every try, and it's taken back once a listing works.
    said: Option<String>,
}

impl Listing<'_> {
    /// The sessions as Home shows them, now.
    ///
    /// # Errors
    ///
    /// If the library can't be listed.
    fn sessions(&mut self) -> io::Result<Vec<Session>> {
        let dates = self.dates();
        // Taken before listing, so time spent listing counts towards the
        // next one.
        let at = self.clock.now();
        let mut sessions: Vec<Session> = self
            .library
            .listing(RATE)?
            .into_iter()
            .map(|listed| session(listed, self.salvaged, &dates))
            .collect();
        self.job_statuses(&mut sessions);
        self.listed_at = Some((at, dates.today()));
        Ok(sessions)
    }

    /// Gives ready sessions their post-stop job status.
    fn job_statuses(&self, sessions: &mut [Session]) {
        let jobs = self.library.db().with(|db| db.jobs());
        for shown in sessions.iter_mut().filter(|s| s.status == Status::Ready) {
            match &jobs {
                Ok(jobs) => job_status(shown, jobs),
                Err(error) => {
                    shown.status = Status::NeedsYou;
                    shown.detail = Some(format!("processing status couldn't be read: {error}"));
                }
            }
        }
    }

    /// Gives `home` the sessions again if a listing is due: [`RELIST`] has
    /// passed since the last, or the local date has changed. If listing
    /// fails, Home keeps the list it has and says why, once until a
    /// listing works again, without replacing another problem it shows.
    fn refresh(&mut self, home: &mut Home) {
        if self.due() {
            self.relist(home);
        }
    }

    /// Lists now after a page change, keeping the old list if reading fails.
    fn relist(&mut self, home: &mut Home) {
        match self.sessions() {
            Ok(sessions) => {
                if self
                    .said
                    .take()
                    .is_some_and(|said| home.notice() == Some(&said))
                {
                    home.set_notice(None);
                }
                home.set_sessions(sessions);
            }
            Err(e) => {
                // Not listed: wait the interval before trying again, even
                // if the date has changed since the last listing.
                self.listed_at = Some((self.clock.now(), self.today()));
                if self.said.is_none() && home.notice().is_none() {
                    home.set_notice(Some(format!("the sessions couldn't be listed: {e}")));
                    self.said = home.notice().map(str::to_owned);
                }
            }
        }
    }

    /// Now, and the local time zone.
    fn dates(&self) -> Dates {
        Dates {
            now: (self.wall)(),
            zone: (self.zone)(),
        }
    }

    /// Today's local date, if the calendar's time can be read.
    fn today(&self) -> Option<Date> {
        self.dates().today()
    }

    fn due(&self) -> bool {
        let Some((at, day)) = self.listed_at else {
            return true;
        };
        self.clock
            .now()
            .checked_duration_since(at)
            .unwrap_or_default()
            >= RELIST
            || self.today() != day
    }
}

/// Shows the recording's stop without replacing Home's list.
fn stopping_home(home: &mut Home) {
    home.set_busy(Some(STOPPING.to_owned()));
}

/// Gives one ready session the status of its jobs.
fn job_status(shown: &mut Session, jobs: &[nota_store::Job]) {
    let own = || {
        jobs.iter()
            .filter(|job| job.session == SessionId::new(shown.id))
    };
    if let Some(why) = own().find_map(|job| match &job.state {
        nota_store::JobState::Failed(why) => Some(why),
        _ => None,
    }) {
        shown.status = Status::NeedsYou;
        shown.detail = Some(format!("processing failed: {why}"));
    } else if own().any(|job| job.state != nota_store::JobState::Done) {
        shown.status = Status::Processing;
        shown.detail = Some("final transcript queued or running".into());
    }
}

/// How Home words a session's date: `today`, `2 Oct` this year, `Oct 2025`
/// before it, in the local time zone.
struct Dates {
    /// Now, if the calendar's time can be read; without it, no day is
    /// `today`.
    now: Option<WallTime>,
    /// The local time zone for date labels.
    zone: TimeZone,
}

impl Dates {
    /// Today's local date, if now is known.
    fn today(&self) -> Option<Date> {
        let now = Timestamp::from_second(self.now?.unix_seconds()).ok()?;
        Some(now.to_zoned(self.zone.clone()).date())
    }

    /// The local date of `at`, in words.
    fn label(&self, at: WallTime) -> Option<String> {
        let date = Timestamp::from_second(at.unix_seconds())
            .ok()?
            .to_zoned(self.zone.clone());
        let today = self
            .now
            .and_then(|now| Timestamp::from_second(now.unix_seconds()).ok())
            .map(|now| now.to_zoned(self.zone.clone()));
        Some(match today {
            Some(today) if today.date() == date.date() => "today".to_owned(),
            Some(today) if today.year() != date.year() => date.strftime("%b %Y").to_string(),
            _ => date.strftime("%-d %b").to_string(),
        })
    }
}

/// How Home shows `listed`, given what salvage did to it at start.
fn session(listed: Listed, salvaged: &[Salvaged], dates: &Dates) -> Session {
    let id = listed.id;
    let salvage = salvaged.iter().find(|s| salvaged_id(s) == id);
    let (status, detail) = match (listed.needs, salvage) {
        // Salvage at start failed, and what it would have saved is still
        // there: its error says why better than the listing can.
        (Needs::Attention(_), Some(Salvaged::Failed(_, e))) => {
            (Status::NeedsYou, Some(format!("salvage failed: {e}")))
        }
        (Needs::Attention(why), Some(Salvaged::Left(_, _, held))) => {
            let notes = crate::record::held_notes(held);
            let detail = if notes.is_empty() {
                why
            } else {
                notes.join("; ")
            };
            (Status::NeedsYou, Some(detail))
        }
        (Needs::Attention(why), _) => (Status::NeedsYou, Some(why)),
        (Needs::InUse, _) => (Status::Processing, Some("being recorded".to_owned())),
        // Salvage at start failed, but a later one (each recording's start
        // salvages too) left nothing to save.
        (Needs::Nothing, Some(Salvaged::Failed(..) | Salvaged::InUse(_)) | None) => {
            (Status::Ready, None)
        }
        // Journals salvage left, or set aside as damaged, show as attention
        // above while they're there; once none are left, it's recovered.
        // "Nothing lost" only if salvage set nothing aside.
        (Needs::Nothing, Some(Salvaged::Done(_, aside) | Salvaged::Left(_, aside, _))) => {
            let recovered = if aside.is_empty() {
                "recovered after a crash · nothing lost"
            } else {
                "recovered after a crash"
            };
            (Status::Ready, Some(recovered.to_owned()))
        }
    };
    Session {
        id: id.get(),
        title: listed
            .title
            .unwrap_or_else(|| format!("session {}", id.get())),
        status,
        duration: listed.recorded,
        date: listed.started_at.and_then(|at| dates.label(at)),
        detail,
    }
}

const fn salvaged_id(salvaged: &Salvaged) -> SessionId {
    match salvaged {
        Salvaged::Done(id, _)
        | Salvaged::Left(id, _, _)
        | Salvaged::InUse(id)
        | Salvaged::Failed(id, _) => *id,
    }
}

#[cfg(test)]
mod tests;
