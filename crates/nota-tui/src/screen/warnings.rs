//! What the Recording screen warns about (the UI spec's "Warnings"),
//! worked out from the recorder's events.
//!
//! - **Conditions** hold while their cause does and clear with it. The
//!   most severe shows in the top border with a count of the rest. Faults
//!   (lost, not recording, not responding, muted) warn per track; quiet
//!   and nothing-playing warn only while every track is silent, since a
//!   quiet mic while the system audio carries the lecture is normal.
//! - **Events** happen once (a route change, a device back, a sleep) and
//!   show for [`EVENT_SHOWN`]. They give way to a condition in the accent
//!   colour, so audio being lost is never hidden behind news.
//! - **The end:** a full disk stops the recording, and the screen says so.
//!
//! The band marks every change (a device lost, back or changed, a fault
//! starting) at its session time, from [`Warnings::changes`].

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use nota_core::recorder::{
    Cause, DeviceChange, EngineState, Event, Track, TrackRole, Warning, WarningState,
};
use nota_core::{SessionTime, TrackId};

use crate::text::is_drawn;

/// How long an event shows in the top border: the spec's "about 10 s".
/// A broken journal's `not recording` shows as long.
pub(crate) const EVENT_SHOWN: Duration = Duration::from_secs(10);

/// The time left on the disk under which its warning is in the accent
/// colour: the spec's 15 minutes.
const DISK_CRITICAL: Duration = Duration::from_mins(15);

/// A condition the screen warns about while it holds. Declared most
/// severe first, as the spec's table orders them, so the derived order is
/// the order they show in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Condition {
    /// The track's device went away.
    Lost(TrackId),
    /// The track's stream failed or was refused, or its journal broke
    /// moments ago.
    NotRecording(TrackId),
    /// The track's stream has stalled.
    NotResponding(TrackId),
    /// Under 15 minutes of disk left, or the time left isn't known yet.
    DiskCritical,
    /// Exact zeros from a track that isn't the system audio.
    Muted(TrackId),
    /// Exact zeros from the system audio, while every track is silent.
    NothingPlaying(TrackId),
    /// Every track silent, at least one below its noise floor.
    Quiet,
    /// Under an hour of disk left.
    DiskLow,
    /// The library database can't be written.
    LibraryOffline,
    /// The transcriber is offline.
    LiveTextOff,
    /// Sleep couldn't be held off.
    MaySleep,
}

impl Condition {
    /// Whether audio is being lost (the accent colour) rather than
    /// something being worth a look (gold).
    const fn is_loss(self) -> bool {
        matches!(
            self,
            Self::Lost(_) | Self::NotRecording(_) | Self::NotResponding(_) | Self::DiskCritical
        )
    }
}

/// Something that happened once, shown for [`EVENT_SHOWN`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Happening {
    /// A track that follows the default moved to the device named.
    Route(TrackId, String),
    /// A lost track's device is back.
    Back(TrackId),
    /// The machine slept, for as long as the gap after it, once one comes.
    Slept(Option<Duration>),
}

/// How a warning is coloured. Each also has its glyph, so none relies on
/// colour alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    /// Audio is being lost.
    Accent,
    /// Worth a look.
    Gold,
    /// Plain news: a route change.
    Plain,
    /// Good news: a device back.
    Good,
}

/// What takes `● REC`'s place in the top border.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Shown {
    /// The words, glyph first: `⚠ mic lost`.
    pub(crate) text: String,
    /// Its colour.
    pub(crate) tone: Tone,
    /// How many other conditions hold besides it.
    pub(crate) more: usize,
}

/// One track's state, as far as warnings go.
#[derive(Debug, Default)]
struct TrackState {
    /// What it records, if the screen was told.
    role: Option<TrackRole>,
    /// The footer's name for its source, if the screen was told or a
    /// route change named it.
    source: Option<String>,
    lost: bool,
    /// Its stream failed or recording was refused; neither clears.
    failed: bool,
    /// When its journal last broke, by the screen's clock.
    journal_broke: Option<SessionTime>,
    stalled: bool,
    /// Since when it has had only exact zeros.
    zeros: Option<SessionTime>,
    /// Since when it has been below its noise floor.
    quiet: Option<SessionTime>,
}

impl TrackState {
    /// Whether it records nothing worth hearing: silent, or not recording
    /// at all.
    fn is_silent(&self) -> bool {
        self.lost || self.failed || self.stalled || self.zeros.is_some() || self.quiet.is_some()
    }

    /// Whether its zeros mean nothing is playing rather than a muted mic.
    fn is_system(&self) -> bool {
        self.role == Some(TrackRole::System)
    }
}

/// The Recording screen's warnings, kept from the recorder's events.
#[derive(Debug, Default)]
pub(crate) struct Warnings {
    /// Every track the screen was told of or has heard from.
    tracks: BTreeMap<TrackId, TrackState>,
    disk_low: bool,
    /// The time left on the disk, from its last check.
    disk_left: Option<Duration>,
    /// The conditions that hold without a track and aren't the disk's:
    /// the library offline, live text off, may sleep.
    standing: BTreeSet<Condition>,
    /// When the full disk stopped the recording.
    disk_full: Option<SessionTime>,
    /// The latest event, and when the screen got it.
    happening: Option<(Happening, SessionTime)>,
    /// The session times of every change, in order.
    changes: Vec<SessionTime>,
}

impl Warnings {
    /// Notes `tracks`: what each records and its source's name.
    pub(crate) fn set_tracks(&mut self, tracks: Vec<Track>) {
        for track in tracks {
            let state = self.tracks.entry(track.id).or_default();
            state.role = Some(track.role);
            state.source = Some(track.source);
        }
    }

    /// Applies `event`, which the screen got at `now`.
    pub(crate) fn update(&mut self, event: &Event, now: SessionTime) {
        match event {
            Event::Level { track, .. } => {
                self.tracks.entry(*track).or_default();
            }
            Event::Warning(warning) => self.warning(warning, now),
            Event::Device { track, change, at } => self.device(*track, change, *at, now),
            Event::Engine(state) => {
                let offline = matches!(state, EngineState::Offline(_));
                self.stand(Condition::LiveTextOff, offline);
            }
            Event::Disk(disk) => self.disk_left = disk.left,
            Event::Gap { gap, .. } => {
                // The sleep's length is the gap after it: the longest, if
                // tracks woke at different times.
                if let Some((Happening::Slept(slept), at)) = &mut self.happening
                    && shows_at(*at, now)
                {
                    *slept = Some(slept.map_or(gap.duration(), |s| s.max(gap.duration())));
                }
            }
            Event::Recorded(_)
            | Event::Text(_)
            | Event::Transcribing(_)
            | Event::Durable { .. }
            | Event::Epoch { .. }
            | Event::Stopping
            | Event::Stopped(_) => {}
        }
    }

    /// Applies a warning raised or cleared, which the screen got at `now`.
    fn warning(&mut self, warning: &Warning, now: SessionTime) {
        let raised = warning.state == WarningState::Raised;
        let since = raised.then_some(warning.at);
        match (&warning.cause, warning.track) {
            (Cause::Stalled, Some(track)) => {
                self.track(track).stalled = raised;
                self.change_if(raised, warning.at);
            }
            (Cause::DigitalZeros, Some(track)) => {
                let state = self.track(track);
                state.zeros = since;
                // Zeros on a mic are a fault; on the system audio they're
                // only silence.
                let fault = !state.is_system();
                self.change_if(raised && fault, warning.at);
            }
            (Cause::Quiet, Some(track)) => self.track(track).quiet = since,
            (Cause::StreamFailed(_), Some(track)) => {
                self.track(track).failed = raised;
                self.change_if(raised, warning.at);
            }
            (Cause::JournalFailed(_), Some(track)) if raised => {
                self.track(track).journal_broke = Some(now);
                self.changes_at(warning.at);
            }
            // Broken on the regular fsync, with no track to name: a change
            // on the band, but no track to say isn't recording.
            (Cause::JournalFailed(_), None) if raised => self.changes_at(warning.at),
            (Cause::DiskLow, _) => self.disk_low = raised,
            (Cause::DiskFull, _) if raised => self.disk_full = Some(warning.at),
            (Cause::SleepNotHeld, _) => self.stand(Condition::MaySleep, raised),
            (Cause::Slept, _) if raised => self.happening = Some((Happening::Slept(None), now)),
            (Cause::LibraryUnavailable, _) => self.stand(Condition::LibraryOffline, raised),
            // Drift is corrected as it's measured: the timeline keeps it,
            // and there's nothing for the listener to do. The rest are a
            // track's causes without a track, or clears that never come.
            _ => {}
        }
    }

    /// Applies a change to `track`'s device, noticed at `at` and got at
    /// `now`.
    fn device(&mut self, track: TrackId, change: &DeviceChange, at: SessionTime, now: SessionTime) {
        let state = self.track(track);
        match change {
            DeviceChange::Lost => state.lost = true,
            DeviceChange::Changed(name) => {
                // The name is the audio server's, which a Bluetooth device
                // sets itself: only what's drawn is kept, as in the title.
                let name: String = name.chars().filter(|&c| is_drawn(c)).collect();
                let happening = if std::mem::take(&mut state.lost) {
                    Happening::Back(track)
                } else {
                    Happening::Route(track, name.clone())
                };
                state.source = Some(name);
                self.happening = Some((happening, now));
            }
            DeviceChange::Format => {}
            DeviceChange::PermissionDenied => state.failed = true,
        }
        self.changes_at(at);
    }

    /// Notes whether `condition`, one without a track, holds.
    fn stand(&mut self, condition: Condition, holds: bool) {
        if holds {
            self.standing.insert(condition);
        } else {
            self.standing.remove(&condition);
        }
    }

    /// `track`'s state, made if it's new.
    fn track(&mut self, track: TrackId) -> &mut TrackState {
        self.tracks.entry(track).or_default()
    }

    fn change_if(&mut self, changed: bool, at: SessionTime) {
        if changed {
            self.changes_at(at);
        }
    }

    /// Notes a change at `at`, keeping them in time order.
    fn changes_at(&mut self, at: SessionTime) {
        let index = self.changes.partition_point(|&c| c <= at);
        self.changes.insert(index, at);
    }

    /// The session times of every change so far, in order.
    pub(crate) fn changes(&self) -> &[SessionTime] {
        &self.changes
    }

    /// When the full disk stopped the recording, if it did.
    pub(crate) const fn stopped_by_full_disk(&self) -> Option<SessionTime> {
        self.disk_full
    }

    /// The footer's names for the tracks' sources, in track order, if the
    /// screen knows any: `Headphones + system audio`.
    pub(crate) fn sources(&self) -> Option<String> {
        let names: Vec<&str> = self
            .tracks
            .values()
            .filter_map(|state| state.source.as_deref())
            .collect();
        (!names.is_empty()).then(|| names.join(" + "))
    }

    /// What takes `● REC`'s place at `now`, if anything: the most severe
    /// condition if it's a loss, else the latest event while it shows,
    /// else the most severe condition. Either way with a count of the
    /// other conditions.
    pub(crate) fn top(&self, now: SessionTime) -> Option<Shown> {
        let conditions = self.conditions(now);
        let first = conditions.first().copied();
        let happening = self
            .happening
            .as_ref()
            .filter(|(_, at)| shows_at(*at, now))
            .map(|(happening, _)| happening);
        let condition = first.filter(|first| first.is_loss() || happening.is_none());
        if let Some(first) = condition {
            return Some(Shown {
                text: self.words(first, now),
                tone: if first.is_loss() {
                    Tone::Accent
                } else {
                    Tone::Gold
                },
                more: conditions.len() - 1,
            });
        }
        let (text, tone) = self.happening_words(happening?);
        Some(Shown {
            text,
            tone,
            more: conditions.len(),
        })
    }

    /// Every condition holding at `now`, most severe first.
    pub(crate) fn conditions(&self, now: SessionTime) -> Vec<Condition> {
        let mut held = Vec::new();
        let all_silent = self.tracks.values().all(TrackState::is_silent);
        for (&id, track) in &self.tracks {
            if track.lost {
                held.push(Condition::Lost(id));
            }
            let journal = track.journal_broke.is_some_and(|at| shows_at(at, now));
            if track.failed || journal {
                held.push(Condition::NotRecording(id));
            }
            if track.stalled {
                held.push(Condition::NotResponding(id));
            }
            match track.zeros {
                Some(_) if !track.is_system() => held.push(Condition::Muted(id)),
                Some(_) if all_silent => held.push(Condition::NothingPlaying(id)),
                _ => {}
            }
        }
        if all_silent && self.tracks.values().any(|track| track.quiet.is_some()) {
            held.push(Condition::Quiet);
        }
        if self.disk_low {
            let critical = self.disk_left.is_none_or(|left| left < DISK_CRITICAL);
            held.push(if critical {
                Condition::DiskCritical
            } else {
                Condition::DiskLow
            });
        }
        held.extend(&self.standing);
        held.sort_unstable();
        held
    }

    /// The words for `condition` at `now`, glyph first.
    fn words(&self, condition: Condition, now: SessionTime) -> String {
        let text = match condition {
            Condition::Lost(id) => format!("{} lost", self.name(id)),
            Condition::NotRecording(id) => format!("{} not recording", self.name(id)),
            Condition::NotResponding(id) => format!("{} not responding", self.name(id)),
            Condition::DiskCritical | Condition::DiskLow => match self.disk_left {
                Some(left) => format!("disk: {}m left", left.as_secs() / 60),
                None => "disk: nearly full".to_owned(),
            },
            Condition::Muted(id) => format!("{} muted", self.name(id)),
            Condition::NothingPlaying(id) => format!("{}: nothing playing", self.name(id)),
            Condition::Quiet => format!("quiet {}", minutes_seconds(self.silent_for(now))),
            Condition::LibraryOffline => "library offline".to_owned(),
            Condition::LiveTextOff => "live text off".to_owned(),
            Condition::MaySleep => "may sleep".to_owned(),
        };
        format!("⚠ {text}")
    }

    /// The words and colour for `happening`, glyph first.
    fn happening_words(&self, happening: &Happening) -> (String, Tone) {
        match happening {
            Happening::Route(id, name) => (format!("↪ {}: {name}", self.name(*id)), Tone::Plain),
            Happening::Back(id) => (format!("✓ {} back", self.name(*id)), Tone::Good),
            Happening::Slept(Some(slept)) => {
                (format!("⚠ slept {}", duration_words(*slept)), Tone::Gold)
            }
            Happening::Slept(None) => ("⚠ slept".to_owned(), Tone::Gold),
        }
    }

    /// How long every track has been silent at `now`: since the last of
    /// them went quiet or to zeros.
    fn silent_for(&self, now: SessionTime) -> Duration {
        self.tracks
            .values()
            .filter_map(|track| track.quiet.max(track.zeros))
            .max()
            .and_then(|since| now.checked_duration_since(since))
            .unwrap_or_default()
    }

    /// The track's short name: `mic`, `system`, or its number if the
    /// screen wasn't told what it records.
    fn name(&self, id: TrackId) -> String {
        self.tracks
            .get(&id)
            .and_then(|track| track.role)
            .map_or_else(
                || format!("track {}", id.get()),
                |role| role.name().to_owned(),
            )
    }
}

/// Whether an event got at `at` still shows at `now`.
fn shows_at(at: SessionTime, now: SessionTime) -> bool {
    now.checked_duration_since(at)
        .is_none_or(|after| after < EVENT_SHOWN)
}

/// `0:42`, `12:05`: minutes and seconds.
fn minutes_seconds(duration: Duration) -> String {
    let secs = duration.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// `1m 32s`, `45s`, `2h 3m`: a sleep's length in the spec's words.
fn duration_words(duration: Duration) -> String {
    let secs = duration.as_secs();
    match (secs / 3600, secs / 60 % 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, m, _) => format!("{h}h {m}m"),
    }
}

#[cfg(test)]
mod tests;
