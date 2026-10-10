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
//! cpal already links for its real-time promotion. Connecting to the bus
//! can't be given a short timeout, so the whole attempt runs on a thread
//! that the start waits two seconds for ([`bounded`]).

use std::error::Error;
use std::fmt;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use nota_core::recorder::{self, Cause, Warning, WarningState};
use nota_core::{Clock, SessionTime};
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

/// The longest the whole attempt waits for the lock, connecting to the
/// bus included. Recording starts after it, so a bus or a logind that
/// doesn't answer costs this much. The connection can't be given a bound
/// of its own: libdbus waits about 25 s for the bus's greeting.
#[cfg(target_os = "linux")]
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);

/// The longest the `Inhibit` call waits for logind's answer, in the
/// milliseconds D-Bus takes. It matches [`ATTEMPT_TIMEOUT`], so a late
/// thread ends itself soon after the start has given up on it.
#[cfg(target_os = "linux")]
const CALL_TIMEOUT_MS: i32 = 2_000;

/// Which bus the lock is asked for on.
#[cfg(target_os = "linux")]
enum Bus {
    /// The system bus, where logind lives.
    System,
    /// A bus at this address, which tests use to stand one in.
    #[cfg(test)]
    At(String),
}

#[cfg(target_os = "linux")]
impl Logind for SystemLogind {
    fn inhibit_sleep(&self) -> Result<SleepLock, SleepNotHeld> {
        bounded(ATTEMPT_TIMEOUT, move || take_lock(&Bus::System))
    }
}

/// Runs `attempt` on a thread of its own and waits `wait` for what it
/// gives. If that takes longer the start goes on without a lock, and a
/// lock that arrives afterwards is dropped on the thread, so it's never
/// held for a recording that began without it. A thread that dies without
/// an answer is a refusal too.
#[cfg(target_os = "linux")]
fn bounded<T: Send + 'static>(
    wait: Duration,
    attempt: impl FnOnce() -> Result<T, SleepNotHeld> + Send + 'static,
) -> Result<T, SleepNotHeld> {
    let (give, given) = mpsc::channel();
    thread::Builder::new()
        .name("nota-logind".to_owned())
        .spawn(move || {
            // If the start has given up, the receiver is gone and the
            // answer, a late lock included, is dropped here.
            let _ = give.send(attempt());
        })
        .map_err(|e| SleepNotHeld::new(format!("couldn't ask logind: {e}")))?;
    match given.recv_timeout(wait) {
        Ok(answer) => answer,
        Err(RecvTimeoutError::Timeout) => Err(SleepNotHeld::new(format!(
            "logind didn't answer within {}",
            lasted(wait)
        ))),
        Err(RecvTimeoutError::Disconnected) => {
            Err(SleepNotHeld::new("the request to logind stopped early"))
        }
    }
}

/// Connects to `bus` and asks logind for the sleep lock.
#[cfg(target_os = "linux")]
fn take_lock(bus: &Bus) -> Result<SleepLock, SleepNotHeld> {
    use dbus::{BusType, Connection, Message, MessageItem};

    let connection = match bus {
        Bus::System => Connection::get_private(BusType::System),
        #[cfg(test)]
        Bus::At(address) => Connection::open_private(address).and_then(|connection| {
            connection.register()?;
            Ok(connection)
        }),
    }
    .map_err(|e| SleepNotHeld::from_dbus(&e))?;
    // `Inhibit(what, who, why, mode)` returns the lock as a file
    // descriptor: logind drops the lock when the last copy closes.
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

/// How far apart two tracks' first audio after a sleep can be and still be
/// the same sleep when the timelines can't say otherwise: each stream wakes
/// on its own, and a slow one (Bluetooth, say) a few seconds after the rest.
const SAME_SLEEP: Duration = Duration::from_secs(2);

/// A stretch of the recording in which no track captured anything, as a
/// sleep leaves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Unrecorded {
    /// When the audio before it ended.
    pub(crate) from: SessionTime,
    /// When the audio after it began.
    pub(crate) to: SessionTime,
}

impl Unrecorded {
    /// How long it lasted.
    fn duration(self) -> Duration {
        self.to
            .checked_duration_since(self.from)
            .unwrap_or_default()
    }

    /// Whether `at` is in it, or within [`SAME_SLEEP`] after it.
    fn reaches(self, at: SessionTime) -> bool {
        at >= self.from
            && self
                .to
                .checked_add(SAME_SLEEP)
                .is_none_or(|latest| at <= latest)
    }

    /// What this and `other` have in common, if they overlap.
    fn overlap(self, other: Self) -> Option<Self> {
        let from = self.from.max(other.from);
        let to = self.to.min(other.to);
        (from < to).then_some(Self { from, to })
    }
}

/// A sleep the machine took during the recording, as the live view saw it:
/// when the audio first came back, and the stretch with no audio before it
/// if the timelines kept one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slept {
    /// When the audio first came back, on any track.
    pub(crate) resumed: SessionTime,
    /// The time in which no track recorded. Missing if the timeline took
    /// the stretch as drift, refused a new epoch for it, or none of the
    /// gaps it kept began where the track's audio ended.
    pub(crate) unrecorded: Option<Unrecorded>,
}

impl Slept {
    /// Whether `other`, from another track, is the same sleep as this:
    /// their stretches overlap, or one has none and its track woke in or
    /// just after the other's, or neither has one and the tracks woke
    /// within [`SAME_SLEEP`] of each other.
    pub(crate) fn same_sleep_as(&self, other: &Self) -> bool {
        match (self.unrecorded, other.unrecorded) {
            (Some(ours), Some(theirs)) => ours.overlap(theirs).is_some(),
            (Some(only), None) => only.reaches(other.resumed),
            (None, Some(only)) => only.reaches(self.resumed),
            (None, None) => {
                let apart = self
                    .resumed
                    .checked_duration_since(other.resumed)
                    .or_else(|| other.resumed.checked_duration_since(self.resumed));
                apart.is_some_and(|apart| apart <= SAME_SLEEP)
            }
        }
    }

    /// This sleep and `other`, which is the same one seen from another
    /// track, as one: the machine was asleep only while no track recorded,
    /// so the stretch is what both have in common.
    pub(crate) fn merged_with(&self, other: &Self) -> Self {
        let unrecorded = match (self.unrecorded, other.unrecorded) {
            (Some(ours), Some(theirs)) => ours.overlap(theirs),
            (ours, theirs) => ours.or(theirs),
        };
        Self {
            resumed: self.resumed.min(other.resumed),
            unrecorded,
        }
    }

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
        match self.unrecorded {
            Some(unrecorded) => format!(
                "the machine slept at {} for {}; nothing was recorded then",
                clock_time(unrecorded.from),
                lasted(unrecorded.duration())
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
        (0, 0, 0) => "less than a second".to_owned(),
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, m, _) => format!("{h}h {m}m"),
    }
}

#[cfg(test)]
mod tests;
