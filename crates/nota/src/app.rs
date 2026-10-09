//! `nota` with no command: the Home screen, and the flow between screens.
//!
//! ```text
//! Home ──R──▶ Recording ──s, y──▶ (stopping) ──▶ Home
//! ```
//!
//! The terminal is set up once, for Home, and lent to each recording: the
//! Recording screen runs on it, and Home shows "finishing the recording"
//! while the recording stops, then lists the sessions again. Setup (`r`),
//! Processing and Review join the flow as they're built; until then `R`
//! records with the last session's settings, and stopping returns to Home.
//!
//! At start, sessions an earlier run left are salvaged, as `nota record`
//! does. What each recording's stop reports, and anything that went wrong,
//! is said on stderr once nota closes, as `nota record` says it.
//!
//! SIGHUP, SIGTERM and SIGINT close nota wherever it is: Home closes at
//! once, and a recording stops in order first. The app listens for them
//! from before the terminal is set up until it's restored
//! ([`QuitSignals`]).

use std::sync::Arc;
use std::sync::mpsc;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use nota_core::recorder::{Command, Input, Setup};
use nota_core::{Clock, SessionId, SystemClock, WallTime, wall_now};
use nota_tui::{Action, Home, InputThread, RunError, Session, Status, Theme};

use crate::library::{Library, Listed, Needs, Salvaged};
use crate::record::{
    BoxError, Lent, QuitSignals, RATE, RecordArgs, last_setup, record_in, segment_length,
};
use crate::terminal::Screen;

/// What Home shows while a recording stops.
const STOPPING: &str = "finishing the recording";

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
    let mut screen: Option<Screen> = None;
    let mut notice = None;
    while !quit.asked() {
        let mut current = match screen.take() {
            Some(screen) => screen,
            None => Screen::enter(None)?,
        };
        let dates = Dates {
            now: wall_now(),
            zone: TimeZone::system(),
        };
        let sessions = library
            .listing(RATE)?
            .into_iter()
            .map(|listed| session(listed, &salvaged, &dates))
            .collect();
        let mut home = Home::new(sessions, engines, theme);
        home.set_notice(notice.take());
        match show_home(&mut current, &mut home, &clock, &quit)? {
            Action::Quit => return Ok(()),
            Action::Record => {}
        }
        match record_from(args, &library, current, &mut home, said) {
            Recorded::Back(back) => screen = Some(back),
            Recorded::Failed(e) => notice = Some(format!("the recording failed: {e}")),
            Recorded::TerminalGone => return Ok(()),
        }
    }
    Ok(())
}

/// How a recording from Home went, for what comes next.
enum Recorded {
    /// It was recorded, and the terminal is back for Home.
    Back(Screen),
    /// It failed: the terminal was restored, and Home says why.
    Failed(String),
    /// It was recorded, but the terminal failed (as after a hangup):
    /// there's nothing to show Home on.
    TerminalGone,
}

/// Records with the last session's settings on the app's terminal, `home`
/// showing while it stops, and adds what it reported to `said`.
fn record_from(
    args: &RecordArgs,
    library: &Library,
    screen: Screen,
    home: &mut Home,
    said: &mut Vec<String>,
) -> Recorded {
    let mut record = args.clone();
    record.start = Command::Start(last_setup(library).unwrap_or_else(first_setup));
    let mut stopping = |screen: &mut Screen| {
        home.set_busy(Some(STOPPING.to_owned()));
        // Only a courtesy: if drawing fails, the next draw says so.
        let _ = screen.clear();
        let _ = screen.terminal().draw(|frame| home.draw(frame));
    };
    let lent = Lent {
        screen,
        library: library.clone(),
        stopping: &mut stopping,
    };
    match record_in(&record, Some(lent)) {
        Ok((outcome, back)) => {
            said.push(format!(
                "nota: recorded to {} ({} segments)",
                outcome.session.display(),
                outcome.segments
            ));
            said.extend(outcome.notes.iter().map(|note| format!("  {note}")));
            back.map_or(Recorded::TerminalGone, Recorded::Back)
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
    clock: &Arc<dyn Clock>,
    quit: &QuitSignals,
) -> Result<Action, BoxError> {
    let (ui, ui_events) = mpsc::channel();
    quit.show_home(Some(ui.clone()));
    // A signal before Home was there to take it.
    if quit.asked() {
        quit.show_home(None);
        return Ok(Action::Quit);
    }
    let input = InputThread::spawn(ui, Arc::clone(clock));
    let ran = input.map_err(BoxError::from).and_then(|input| {
        // A new screen: drawn whole, not as changes to the last one's cells.
        let ran = screen
            .clear()
            .map_err(RunError::Terminal)
            .and_then(|()| nota_tui::run_home(screen.terminal(), home, &ui_events));
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

/// How Home words a session's date: `today`, `2 Oct` this year, `Oct 2025`
/// before it, in the local time zone.
struct Dates {
    /// Now, if the calendar's time can be read; without it, no day is
    /// `today`.
    now: Option<WallTime>,
    zone: TimeZone,
}

impl Dates {
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
        (Needs::Attention(why), _) => (Status::NeedsYou, Some(why)),
        (Needs::InUse, _) => (Status::Processing, Some("being recorded".to_owned())),
        // Salvage at start failed, but a later one (each recording's start
        // salvages too) left nothing to save.
        (Needs::Nothing, Some(Salvaged::Failed(..) | Salvaged::InUse(_)) | None) => {
            (Status::Ready, None)
        }
        // Journals salvage left show as attention above; once none are
        // left, it's recovered.
        (Needs::Nothing, Some(Salvaged::Done(_) | Salvaged::Left(_))) => (
            Status::Ready,
            Some("recovered after a crash · nothing lost".to_owned()),
        ),
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
        Salvaged::Done(id) | Salvaged::Left(id) | Salvaged::InUse(id) | Salvaged::Failed(id, _) => {
            *id
        }
    }
}

#[cfg(test)]
mod tests;
