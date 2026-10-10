//! Sleep while recording: held off with a logind lock, and reported when it
//! comes anyway.
//!
//! A recording that the machine sleeps through has a hole in it, so
//! `nota record` asks logind for a sleep inhibitor before the first stream
//! opens and keeps it until the recording has stopped ([`hold`]). Sleep
//! can still go wrong in two ways, and each ends up as a warning for the
//! screen and a note in the summary:
//!
//! - **Logind refuses** (or can't be reached): recording goes on, with
//!   [`Cause::SleepNotHeld`] once the streams have started
//!   ([`Sleep::report`]).
//! - **The machine sleeps anyway** (a forced suspend, or a lid closed
//!   while logind's `LidSwitchIgnoreInhibited` is on, which it is by
//!   default): the capture notices it (see
//!   `nota_recorder::capture`) and opens a new epoch at the resume. The
//!   live view turns that into a [`Slept`], which is the warning
//!   ([`Slept::warning`]) and the gap between the epochs.
//!
//! The lock is a file descriptor: logind drops it when the descriptor is
//! closed, including if nota dies, so a crash can't leave the machine
//! unable to sleep. The D-Bus call is made with the `dbus` crate, which
//! cpal already links for its real-time promotion.

use std::error::Error;
use std::fmt;
use std::sync::mpsc::Sender;
use std::time::Duration;

use nota_core::recorder::{self, Cause, Warning, WarningState};
use nota_core::{Clock, Gap, SessionTime};
use nota_tui::Event;

/// Where a sleep lock comes from: logind, or a stand-in in tests.
pub(crate) trait Logind: Send + Sync {
    /// Takes a lock that keeps the machine from sleeping until it's
    /// dropped.
    ///
    /// # Errors
    ///
    /// If logind can't be reached or won't give the lock.
    fn inhibit_sleep(&self) -> Result<SleepLock, SleepNotHeld>;
}

/// A lock that keeps the machine from sleeping. Dropping it lets the
/// machine sleep again.
pub(crate) struct SleepLock {
    /// What releases the lock when it's dropped: logind's descriptor.
    _held: Box<dyn Send>,
}

impl SleepLock {
    /// A lock that lasts as long as `held` does.
    pub(crate) fn new(held: impl Send + 'static) -> Self {
        Self {
            _held: Box::new(held),
        }
    }
}

/// Why sleep couldn't be held off.
#[derive(Debug)]
pub(crate) struct SleepNotHeld {
    /// What went wrong, in logind's or D-Bus's words.
    reason: String,
}

impl SleepNotHeld {
    /// A refusal for `reason`.
    pub(crate) fn new(reason: impl fmt::Display) -> Self {
        Self {
            reason: reason.to_string(),
        }
    }

    /// A refusal for D-Bus's `error`, with its name (`AccessDenied`, say),
    /// which its own text leaves out.
    #[cfg(target_os = "linux")]
    fn from_dbus(error: &dbus::Error) -> Self {
        Self::new(dbus_reason(error.name(), error.message()))
    }
}

/// How D-Bus's error `name` and `message` read together; either may be
/// missing.
#[cfg(target_os = "linux")]
fn dbus_reason(name: Option<&str>, message: Option<&str>) -> String {
    match (name, message) {
        (Some(name), Some(message)) => format!("{name}: {message}"),
        (Some(only), None) | (None, Some(only)) => only.to_owned(),
        (None, None) => "no reason given".to_owned(),
    }
}

impl fmt::Display for SleepNotHeld {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sleep couldn't be held off: {}", self.reason)
    }
}

impl Error for SleepNotHeld {}

/// What came of asking for the sleep lock: the lock, held until this is
/// dropped, or why there is none.
pub(crate) struct Sleep {
    /// The lock, if logind gave it.
    _lock: Option<SleepLock>,
    /// Why there is no lock.
    refused: Option<SleepNotHeld>,
}

/// Asks `logind` for the lock a recording keeps. A refusal doesn't stop
/// anything: it is said once the recording has started ([`Sleep::report`]).
pub(crate) fn hold(logind: &dyn Logind) -> Sleep {
    match logind.inhibit_sleep() {
        Ok(lock) => Sleep {
            _lock: Some(lock),
            refused: None,
        },
        Err(refused) => Sleep {
            _lock: None,
            refused: Some(refused),
        },
    }
}

impl Sleep {
    /// If logind refused: [`Cause::SleepNotHeld`] for the screen, raised at
    /// `clock`'s now, and a note for the summary (`notes`). Said only once
    /// the streams have started, so a start that fails doesn't leave a
    /// warning about a recording that never was, and the screen's startup
    /// checks (which drain the channel) can't swallow it.
    pub(crate) fn report(&self, clock: &dyn Clock, ui: &Sender<Event>, notes: &mut Vec<String>) {
        let Some(refused) = &self.refused else {
            return;
        };
        notes.push(format!(
            "{refused}; if the machine sleeps, the recording has a gap there"
        ));
        let warning = Warning {
            cause: Cause::SleepNotHeld,
            track: None,
            at: clock.now(),
            state: WarningState::Raised,
        };
        // The screen may have closed already.
        let _ = ui.send(Event::Recorder(recorder::Event::Warning(warning)));
    }
}

/// The real logind, reached over the system bus.
#[cfg(target_os = "linux")]
pub(crate) struct SystemLogind;

/// The longest the `Inhibit` call waits for logind's answer. Recording
/// starts after it, so a logind that doesn't answer costs this much. Making
/// the connection isn't bounded by it: libdbus waits as long as it does for
/// the bus's own greeting.
#[cfg(target_os = "linux")]
const CALL_TIMEOUT_MS: i32 = 2_000;

#[cfg(target_os = "linux")]
impl Logind for SystemLogind {
    fn inhibit_sleep(&self) -> Result<SleepLock, SleepNotHeld> {
        use dbus::{BusType, Connection, Message, MessageItem};

        // `Inhibit(what, who, why, mode)` returns the lock as a file
        // descriptor: logind drops the lock when the last copy closes.
        let connection =
            Connection::get_private(BusType::System).map_err(|e| SleepNotHeld::from_dbus(&e))?;
        let mut call = Message::new_method_call(
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
            "Inhibit",
        )
        .map_err(SleepNotHeld::new)?;
        call.append_items(&[
            MessageItem::Str("sleep".to_owned()),
            MessageItem::Str("nota".to_owned()),
            MessageItem::Str("recording".to_owned()),
            MessageItem::Str("block".to_owned()),
        ]);
        let reply = connection
            .send_with_reply_and_block(call, CALL_TIMEOUT_MS)
            .map_err(|e| SleepNotHeld::from_dbus(&e))?;
        match reply.get_items().into_iter().next() {
            Some(MessageItem::UnixFd(lock)) => Ok(SleepLock::new(lock)),
            _ => Err(SleepNotHeld::new("logind gave no lock")),
        }
    }
}

/// A sleep the machine took during the recording, as the live view saw it:
/// the first audio after it, and the gap before that audio if the
/// timeline kept one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slept {
    /// When the audio came back.
    pub(crate) resumed: SessionTime,
    /// The time with no audio. Missing if the timeline took the stretch as
    /// drift, or refused a new epoch for it.
    pub(crate) gap: Option<Gap>,
}

impl Slept {
    /// The warning for the screen, raised at the resume.
    pub(crate) fn warning(&self) -> recorder::Event {
        recorder::Event::Warning(Warning {
            cause: Cause::Slept,
            track: None,
            at: self.resumed,
            state: WarningState::Raised,
        })
    }

    /// What the summary says of it.
    pub(crate) fn note(&self) -> String {
        match self.gap {
            Some(gap) => format!(
                "the machine slept at {} for {}; nothing was recorded then",
                clock_time(gap.from()),
                lasted(gap.duration())
            ),
            None => format!(
                "the machine slept before {}; the recording may have a gap there",
                clock_time(self.resumed)
            ),
        }
    }
}

/// A session time as the screen shows it, `H:MM:SS`.
fn clock_time(at: SessionTime) -> String {
    let secs = at.elapsed().as_secs();
    format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// A length of time in its two largest units: `12s`, `1m 32s`, `2h 5m`.
fn lasted(span: Duration) -> String {
    let secs = span.as_secs();
    match (secs / 3600, secs / 60 % 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, m, _) => format!("{h}h {m}m"),
    }
}

#[cfg(test)]
mod tests;
