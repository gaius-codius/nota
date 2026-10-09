//! The recorder protocol: what the screens and the recorder tell each
//! other while a session is recorded.
//!
//! The screens send [`Command`]s; the recorder sends [`Event`]s back. For
//! now both sides live in one process (`nota record`) and the messages are
//! plain Rust values on channels. They're the whole conversation, though:
//! the screens know the recorder only through them, so moving the recorder
//! into a daemon later puts these types on a wire without changing what
//! either side says.
//!
//! # Commands, from a screen to the recorder
//!
//! | Message | Sent by | When |
//! |---|---|---|
//! | [`Command::Start`] | `nota record`, from its arguments (later, the Setup screen) | Once, to start a session with a [`Setup`] |
//! | [`Command::Mark`] | the Recording screen | `m`: a [`Mark`] at the moment of the key |
//! | [`Command::Note`] | the Recording screen | `⏎` after `n`, or the screen closing with a note half typed: a [`Note`] pinned to the moment of `n` |
//! | [`Command::Stop`] | the Recording screen | The stop is confirmed (`s` or Ctrl+C, then `y`) |
//!
//! # Events, from the recorder to the screens
//!
//! | Message | Sent by | When |
//! |---|---|---|
//! | [`Event::Level`] | the live thread | At most every 100 ms per track while it captures: the peak since the last one |
//! | [`Event::Recorded`] | the live thread | With each level: how much audio has been recorded |
//! | [`Event::Text`] | the live thread | Each stretch of speech the engine heard, placed in session time |
//! | [`Event::Transcribing`] | the live thread | Speech is with the engine, not yet text, or no longer is |
//! | [`Event::Engine`] | the live thread | The transcriber came up or went down |
//! | [`Event::Warning`] | the recorder | Something is wrong with the recording ([`Raised`](WarningState::Raised)), and again once it isn't ([`Cleared`](WarningState::Cleared)) |
//! | [`Event::Device`] | the recorder | A track's device went away, changed, or changed format |
//! | [`Event::Disk`] | the recorder | The free space was checked, before and during the recording |
//! | [`Event::Durable`] | the recorder | A track's audio is on disk, fsynced, up to a moment |
//! | [`Event::Epoch`] | the recorder | A track's stream was reopened, or audio was lost: a new epoch |
//! | [`Event::Gap`] | the recorder | With an epoch after the first: the time with no audio before it |
//! | [`Event::Stopping`] | the recorder | The recording is stopping without a [`Command::Stop`]: on a signal, or once every stream has ended. The screens close |
//! | [`Event::Stopped`] | the recorder | Last: the session is finished, with its [`Outcome`] |
//!
//! Every kind of event M2's plan names is here, including those nothing
//! sends yet: warnings (drift among them), device and disk events, durable
//! progress, epochs and gaps, engine status and transcribing. The work that
//! produces each fills in the sending side, and the screens' handling,
//! without adding a variant. [`Event::Stopped`] isn't sent yet either: in
//! `nota record` the screen has closed before the session is finished, so
//! the [`Outcome`] is returned and printed as the summary instead.

use std::path::PathBuf;
use std::time::Duration;

use crate::epoch;
use crate::ids::TrackId;
use crate::time::SessionTime;
use crate::utterance::Utterance;

/// What a screen asks the recorder to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Start recording a new session.
    Start(Setup),
    /// Mark this moment (◆).
    Mark(Mark),
    /// Pin a note to a moment (◇).
    Note(Note),
    /// Stop the recording and finish the session.
    Stop,
}

/// What a session records, and what it's called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setup {
    /// The session's title.
    pub title: String,
    /// Where the microphone's track records from.
    pub mic: Input,
    /// Where the system audio's track records from.
    pub system: Input,
}

/// Where a track records from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// The system's default for the track: the default input for the
    /// microphone, what the default output plays for the system audio.
    Default,
    /// One device, by the audio server's name for it.
    Device(String),
}

/// A mark (◆): "this matters", at the moment `m` was pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    /// When `m` was pressed.
    pub at: SessionTime,
}

/// A note (◇): the listener's text, pinned to the moment `n` was pressed,
/// not the moment typing finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    at: SessionTime,
    text: String,
}

impl Note {
    /// A note at `at` with `text`, trimmed; `None` if it's blank.
    #[must_use]
    pub fn new(at: SessionTime, text: &str) -> Option<Self> {
        let text = text.trim();
        (!text.is_empty()).then(|| Self {
            at,
            text: text.to_owned(),
        })
    }

    /// When `n` was pressed.
    #[must_use]
    pub const fn at(&self) -> SessionTime {
        self.at
    }

    /// What was typed, trimmed; never empty.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// What the recorder tells the screens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The level a track heard at a moment of the session. Sent at least
    /// every 250 ms while the track captures: a stretch with no level draws
    /// as a gap in the band.
    Level {
        /// The track.
        track: TrackId,
        /// When it was heard.
        at: SessionTime,
        /// How loud it was.
        level: Level,
    },
    /// How much audio has been recorded so far, in bytes: two a sample,
    /// as the journals hold it, not counting their framing.
    Recorded(u64),
    /// New live text.
    Text(Utterance),
    /// Whether a chunk of speech is with the engine, not yet text.
    Transcribing(bool),
    /// The transcriber came up or went down.
    Engine(EngineState),
    /// Something is wrong with the recording, or no longer is.
    Warning(Warning),
    /// Something happened to a track's device.
    Device {
        /// The track.
        track: TrackId,
        /// What happened.
        change: DeviceChange,
        /// When it was noticed.
        at: SessionTime,
    },
    /// How much room the recording has left.
    Disk(Disk),
    /// A track's audio is fsynced up to `up_to`: a crash from now on loses
    /// none of it.
    Durable {
        /// The track.
        track: TrackId,
        /// The session time its durable audio reaches.
        up_to: SessionTime,
    },
    /// A track moved to a new epoch.
    Epoch {
        /// The track.
        track: TrackId,
        /// The epoch it moved to.
        epoch: epoch::Epoch,
    },
    /// A track had no audio between two epochs.
    Gap {
        /// The track.
        track: TrackId,
        /// The time without audio.
        gap: epoch::Gap,
    },
    /// The recording is stopping, though no screen asked: the screens
    /// close.
    Stopping,
    /// The session is finished.
    Stopped(Outcome),
}

/// The peak level of a run of 16-bit samples: their largest magnitude, from 0
/// (silence) to [`Level::FULL_SCALE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Level(u16);

impl Level {
    /// Silence.
    pub const SILENT: Self = Self(0);

    /// The loudest a 16-bit sample can be.
    pub const FULL_SCALE: Self = Self(i16::MAX.unsigned_abs());

    /// A level from a peak magnitude, capped at [`Level::FULL_SCALE`].
    #[must_use]
    pub fn from_peak(peak: u16) -> Self {
        Self(peak.min(Self::FULL_SCALE.0))
    }

    /// The peak of `samples`; silence if there are none. `i16::MIN` counts as
    /// full scale.
    #[must_use]
    pub fn of_samples(samples: &[i16]) -> Self {
        let peak = samples
            .iter()
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap_or(0);
        Self::from_peak(peak)
    }

    /// The peak magnitude.
    #[must_use]
    pub const fn peak(self) -> u16 {
        self.0
    }
}

/// Whether the transcriber is working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineState {
    /// It's transcribing.
    Online,
    /// It's down and will be restarted, for this reason.
    Offline(String),
}

/// A warning about the recording, raised or cleared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// What's wrong.
    pub cause: Cause,
    /// The track it's about, if it's about one.
    pub track: Option<TrackId>,
    /// When it was raised or cleared.
    pub at: SessionTime,
    /// Whether it's raised or cleared.
    pub state: WarningState,
}

/// Whether a warning holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarningState {
    /// The cause holds from now.
    Raised,
    /// The cause no longer holds.
    Cleared,
}

/// What a warning is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// The track's stream has delivered no samples for a while.
    Stalled,
    /// The track has had nothing but exact zeros for a while: nothing
    /// playing, a suspended sink, or a denied permission.
    DigitalZeros,
    /// The track has been below its noise floor for a while.
    Quiet,
    /// The track's clock has drifted from the session clock past the
    /// stated limit.
    Drift,
    /// The track's stream failed and delivers nothing more, for this
    /// reason. The other tracks record on.
    StreamFailed(String),
    /// A journal broke, for this reason: the audio around it may have a
    /// gap. Recording goes on in a new one.
    JournalFailed(String),
    /// The disk is nearly full.
    DiskLow,
    /// The disk is full.
    DiskFull,
    /// Sleep couldn't be held off while recording.
    SleepNotHeld,
    /// The machine slept anyway, during the recording.
    Slept,
    /// The library database can't be written; the audio still is, and it's
    /// added later.
    LibraryUnavailable,
}

/// What happened to a track's device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceChange {
    /// The device went away. A device chosen by name is never swapped for
    /// another quietly: the track stops instead.
    Lost,
    /// The track follows the default, and the default is now this device.
    Changed(String),
    /// The device's format changed.
    Format,
    /// Recording from the device was refused.
    PermissionDenied,
}

/// The room the recording has left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disk {
    /// Free space on the recording's disk, in bytes.
    pub free_bytes: u64,
    /// How long the recording can go on at its rate, if known.
    pub left: Option<Duration>,
}

/// How a recording went: what `nota record` says after it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// The session's directory.
    pub session: PathBuf,
    /// Things worth saying: sessions salvaged, streams that didn't start,
    /// journals left for salvage.
    pub notes: Vec<String>,
    /// Segments published.
    pub segments: usize,
    /// Whether everything recorded was published, with nothing left for
    /// the next start's salvage.
    pub complete: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_are_trimmed_and_never_blank() {
        let at = SessionTime::from_nanos(9);
        let note = Note::new(at, "  bring clamps \t").unwrap();
        assert_eq!((note.at(), note.text()), (at, "bring clamps"));
        assert_eq!(Note::new(at, " \n\t "), None);
        assert_eq!(Note::new(at, ""), None);
    }

    #[test]
    fn peak_of_samples_is_the_largest_magnitude() {
        assert_eq!(Level::of_samples(&[]), Level::SILENT);
        assert_eq!(Level::of_samples(&[3, -700, 12]).peak(), 700);
        assert_eq!(Level::of_samples(&[i16::MIN]), Level::FULL_SCALE);
        assert_eq!(Level::of_samples(&[i16::MAX]), Level::FULL_SCALE);
    }

    #[test]
    fn from_peak_caps_at_full_scale() {
        assert_eq!(Level::from_peak(u16::MAX), Level::FULL_SCALE);
        assert_eq!(Level::from_peak(5).peak(), 5);
    }
}
