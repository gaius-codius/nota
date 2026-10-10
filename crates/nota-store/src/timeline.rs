//! The session's timeline: every change to the recording, with its track
//! and session time, stored in the `event` table as it happens.
//!
//! A [`TimelineEvent`] is one of the recorder's events that says something
//! changed: a warning raised or cleared, a device lost or changed, the
//! transcriber going down or coming back, a stretch with no audio.
//! [`TimelineEvent::of`] picks those out of what the screens are sent, so
//! whatever sends an event to the screen stores the same thing here.
//! Levels, text, the disk's regular checks and durable progress aren't
//! changes, and epochs have their own table.
//!
//! Each row's `kind` says what happened and `detail` holds what goes with
//! it (a reason, a device's name, where a gap ends). Both are parsed back
//! into typed values in one place, and a row that doesn't parse is
//! [`StoreError::Corrupt`].

use nota_core::recorder::{Cause, DeviceChange, EngineState, Event, Warning, WarningState};
use nota_core::{SessionId, SessionTime, TrackId};
use rusqlite::params;

use crate::segments::{parse_track, session_exists};
use crate::{Store, StoreError, session_key};

/// One change to the recording, as the timeline keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineEvent {
    /// When it happened. For a warning, the time the warning carries: for
    /// the detectors' causes a raise is when the condition began.
    pub at: SessionTime,
    /// The track it's about, if it's about one.
    pub track: Option<TrackId>,
    /// What happened.
    pub happened: Happened,
}

/// What happened to the recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Happened {
    /// A warning's cause began to hold.
    Raised(Cause),
    /// A warning's cause stopped holding.
    Cleared(Cause),
    /// Something happened to the track's device.
    Device(DeviceChange),
    /// The transcriber came up or went down.
    Engine(EngineState),
    /// The track had no audio from [`TimelineEvent::at`] until `until`.
    Gap {
        /// When its audio began again.
        until: SessionTime,
    },
}

impl TimelineEvent {
    /// The timeline's entry for `event`, if it's a change, at `now` if the
    /// event doesn't carry a time of its own (the transcriber's state
    /// doesn't). `None` for levels, text, the disk's regular checks,
    /// durable progress, epochs and the stop.
    #[must_use]
    pub fn of(event: &Event, now: SessionTime) -> Option<Self> {
        let (at, track, happened) = match event {
            Event::Warning(Warning {
                cause,
                track,
                at,
                state,
            }) => {
                let happened = match state {
                    WarningState::Raised => Happened::Raised(cause.clone()),
                    WarningState::Cleared => Happened::Cleared(cause.clone()),
                };
                (*at, *track, happened)
            }
            Event::Device { track, change, at } => {
                (*at, Some(*track), Happened::Device(change.clone()))
            }
            Event::Engine(state) => (now, None, Happened::Engine(state.clone())),
            Event::Gap { track, gap } => {
                (gap.from(), Some(*track), Happened::Gap { until: gap.to() })
            }
            Event::Level { .. }
            | Event::Recorded(_)
            | Event::Text(_)
            | Event::Transcribing(_)
            | Event::Disk(_)
            | Event::Durable { .. }
            | Event::Epoch { .. }
            | Event::Stopping
            | Event::Stopped(_) => return None,
        };
        Some(Self {
            at,
            track,
            happened,
        })
    }
}

/// A timeline row as SQLite holds it: time, track, kind, detail.
type RawEvent = (i64, Option<i64>, String, Option<String>);

impl Store {
    /// Adds `event` to `session`'s timeline, committed before returning.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSession`] if the session isn't in the library,
    /// [`StoreError::OutOfRange`] if its time doesn't fit SQLite's integer,
    /// and [`StoreError::Sqlite`] for any SQLite failure.
    pub fn add_event(
        &mut self,
        session: SessionId,
        event: &TimelineEvent,
    ) -> Result<(), StoreError> {
        let key = session_key(session)?;
        let at = nanos(event.at)?;
        if !session_exists(&self.conn, key)? {
            return Err(StoreError::NoSession(session));
        }
        let (kind, detail) = encode(&event.happened)?;
        self.conn.execute(
            "INSERT INTO event (session_id, track, at_ns, kind, detail) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                key,
                event.track.map(|track| i64::from(track.get())),
                at,
                kind,
                detail
            ],
        )?;
        Ok(())
    }

    /// `session`'s timeline in session order; at one moment, in the order
    /// the events were added.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if a row doesn't parse (a negative time, a
    /// track that isn't a `u32`, an unknown kind, a kind missing its
    /// detail), and [`StoreError::Sqlite`] for any SQLite failure.
    pub fn timeline(&self, session: SessionId) -> Result<Vec<TimelineEvent>, StoreError> {
        let key = session_key(session)?;
        let mut stmt = self.conn.prepare(
            "SELECT at_ns, track, kind, detail FROM event WHERE session_id = ?1 \
             ORDER BY at_ns, id",
        )?;
        let raws = stmt
            .query_map([key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<Vec<RawEvent>, _>>()?;
        raws.into_iter().map(decode).collect()
    }
}

fn nanos(at: SessionTime) -> Result<i64, StoreError> {
    i64::try_from(at.as_nanos()).map_err(|_| StoreError::OutOfRange)
}

fn time(nanos: i64) -> Result<SessionTime, StoreError> {
    u64::try_from(nanos)
        .map(SessionTime::from_nanos)
        .map_err(|_| StoreError::Corrupt(format!("timeline event at {nanos} ns")))
}

/// The row's `kind` and `detail` for `happened`.
fn encode(happened: &Happened) -> Result<(String, Option<String>), StoreError> {
    let (kind, detail) = match happened {
        Happened::Raised(cause) => {
            let (name, detail) = cause_parts(cause);
            (format!("raised:{name}"), detail)
        }
        Happened::Cleared(cause) => {
            let (name, detail) = cause_parts(cause);
            (format!("cleared:{name}"), detail)
        }
        Happened::Device(change) => match change {
            DeviceChange::Lost => ("device:lost".to_owned(), None),
            DeviceChange::Changed(name) => ("device:changed".to_owned(), Some(name.clone())),
            DeviceChange::Format => ("device:format".to_owned(), None),
            DeviceChange::PermissionDenied => ("device:permission-denied".to_owned(), None),
        },
        Happened::Engine(EngineState::Online) => ("engine:online".to_owned(), None),
        Happened::Engine(EngineState::Offline(why)) => {
            ("engine:offline".to_owned(), Some(why.clone()))
        }
        Happened::Gap { until } => ("gap".to_owned(), Some(nanos(*until)?.to_string())),
    };
    Ok((kind, detail))
}

/// A cause's name in a row's `kind`, and the reason it carries, if any.
fn cause_parts(cause: &Cause) -> (&'static str, Option<String>) {
    match cause {
        Cause::Stalled => ("stalled", None),
        Cause::DigitalZeros => ("digital-zeros", None),
        Cause::Quiet => ("quiet", None),
        Cause::Drift => ("drift", None),
        Cause::StreamFailed(why) => ("stream-failed", Some(why.clone())),
        Cause::JournalFailed(why) => ("journal-failed", Some(why.clone())),
        Cause::DiskLow => ("disk-low", None),
        Cause::DiskFull => ("disk-full", None),
        Cause::SleepNotHeld => ("sleep-not-held", None),
        Cause::Slept => ("slept", None),
        Cause::LibraryUnavailable => ("library-unavailable", None),
    }
}

/// The cause named `name` in a row's `kind`, with the row's `detail` as
/// its reason where it has one.
fn parse_cause(name: &str, detail: Option<String>) -> Result<Cause, StoreError> {
    let reason = |detail: Option<String>| {
        detail.ok_or_else(|| StoreError::Corrupt(format!("{name} without its reason")))
    };
    Ok(match name {
        "stalled" => Cause::Stalled,
        "digital-zeros" => Cause::DigitalZeros,
        "quiet" => Cause::Quiet,
        "drift" => Cause::Drift,
        "stream-failed" => Cause::StreamFailed(reason(detail)?),
        "journal-failed" => Cause::JournalFailed(reason(detail)?),
        "disk-low" => Cause::DiskLow,
        "disk-full" => Cause::DiskFull,
        "sleep-not-held" => Cause::SleepNotHeld,
        "slept" => Cause::Slept,
        "library-unavailable" => Cause::LibraryUnavailable,
        other => return Err(StoreError::Corrupt(format!("timeline cause {other:?}"))),
    })
}

/// A stored row as a [`TimelineEvent`].
fn decode((at, track, kind, detail): RawEvent) -> Result<TimelineEvent, StoreError> {
    let missing = || StoreError::Corrupt(format!("timeline event {kind:?} without its detail"));
    let happened = if let Some(name) = kind.strip_prefix("raised:") {
        Happened::Raised(parse_cause(name, detail)?)
    } else if let Some(name) = kind.strip_prefix("cleared:") {
        Happened::Cleared(parse_cause(name, detail)?)
    } else {
        match kind.as_str() {
            "device:lost" => Happened::Device(DeviceChange::Lost),
            "device:changed" => {
                Happened::Device(DeviceChange::Changed(detail.ok_or_else(missing)?))
            }
            "device:format" => Happened::Device(DeviceChange::Format),
            "device:permission-denied" => Happened::Device(DeviceChange::PermissionDenied),
            "engine:online" => Happened::Engine(EngineState::Online),
            "engine:offline" => Happened::Engine(EngineState::Offline(detail.ok_or_else(missing)?)),
            "gap" => {
                let until = detail.ok_or_else(missing)?;
                let until = until
                    .parse::<i64>()
                    .map_err(|_| StoreError::Corrupt(format!("gap until {until:?}")))?;
                Happened::Gap {
                    until: time(until)?,
                }
            }
            other => return Err(StoreError::Corrupt(format!("timeline kind {other:?}"))),
        }
    };
    Ok(TimelineEvent {
        at: time(at)?,
        track: track.map(parse_track).transpose()?,
        happened,
    })
}

#[cfg(test)]
mod tests;
