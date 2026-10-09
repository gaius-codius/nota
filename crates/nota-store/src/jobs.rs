//! The job queue: work that runs after a recording stops.
//!
//! Only the live pass runs while recording. Everything else is a job,
//! queued when the recording stops, in the same transaction that marks the
//! session stopped (by its stop, by salvage of one that never finished, or
//! by adopting one found on disk), so a stopped session always has its
//! jobs. A session has at most one job of each [`JobKind`].
//!
//! A job is [`JobState::Waiting`] until a runner takes it, then
//! [`JobState::Running`], with its progress, until it ends
//! [`JobState::Done`], [`JobState::Failed`] with the reason, or waiting
//! again: paused for a recording, or waiting for something it lacks
//! ([`Wait`]). Jobs are taken in the order they were queued. A job left
//! running by a runner that died is taken again ([`Store::recover_jobs`]);
//! every job is safe to run again, carrying on from what it committed.
//!
//! The Processing screen shows what this holds.

use nota_core::SessionId;
use rusqlite::{Connection, TransactionBehavior, params};

use crate::sessions::SessionState;
use crate::{Store, StoreError, parse_session, session_key};

/// What a job does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JobKind {
    /// The final transcription pass over the session's published audio.
    FinalPass,
}

impl JobKind {
    /// Every kind, in the order a stopped session's jobs are queued.
    pub const ALL: [Self; 1] = [Self::FinalPass];

    const fn as_str(self) -> &'static str {
        match self {
            Self::FinalPass => "final-pass",
        }
    }

    fn parse(text: &str) -> Result<Self, StoreError> {
        match text {
            "final-pass" => Ok(Self::FinalPass),
            other => Err(StoreError::Corrupt(format!("unknown job kind {other:?}"))),
        }
    }
}

/// What a waiting job waits for before it can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Wait {
    /// Free space: its session stopped on a full disk, or the job ran out
    /// of space. It runs once there's space again.
    Space,
    /// A speech engine: nota was started without its models.
    Engine,
}

impl Wait {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Space => "space",
            Self::Engine => "engine",
        }
    }

    fn parse(text: &str) -> Result<Self, StoreError> {
        match text {
            "space" => Ok(Self::Space),
            "engine" => Ok(Self::Engine),
            other => Err(StoreError::Corrupt(format!("unknown job wait {other:?}"))),
        }
    }
}

/// Where a job is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    /// Queued, or paused: it runs when a runner takes it, once what it
    /// waits for (if anything) is there.
    Waiting(Option<Wait>),
    /// A runner is running it.
    Running,
    /// Finished.
    Done,
    /// Given up on, for the reason given. It isn't run again.
    Failed(String),
}

/// How far a job has got: `done` of `total`, in whatever its kind counts
/// (the final pass counts samples). Both zero before it has started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    /// What's done.
    pub done: u64,
    /// All there is to do.
    pub total: u64,
}

/// A job's number. Only the store gives one out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(i64);

impl JobId {
    /// The number.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// A queued job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Its number; jobs run in number order.
    pub id: JobId,
    /// The session it works on.
    pub session: SessionId,
    /// What it does.
    pub kind: JobKind,
    /// Where it is.
    pub state: JobState,
    /// How many times a runner died while running it.
    pub attempts: u32,
    /// How far it has got.
    pub progress: Progress,
}

/// How a run of a job ended, for [`Store::end_job`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEnd {
    /// It finished.
    Done,
    /// It stopped before finishing and is to run again: paused for a
    /// recording (`None`), or waiting for what it lacks.
    Waiting(Option<Wait>),
    /// It can't be done, for the reason given.
    Failed(String),
}

type RawJob = (
    i64,
    i64,
    String,
    String,
    i64,
    i64,
    i64,
    Option<String>,
    Option<String>,
);

const JOB_COLUMNS: &str =
    "id, session_id, kind, state, attempts, progress, total, waits_for, detail";

fn raw_job(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawJob> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
    ))
}

fn count(n: i64, what: &str) -> Result<u64, StoreError> {
    u64::try_from(n).map_err(|_| StoreError::Corrupt(format!("job {what} {n} is negative")))
}

fn parse_job(
    (id, session, kind, state, attempts, progress, total, waits, detail): RawJob,
) -> Result<Job, StoreError> {
    let state = match (state.as_str(), waits, detail) {
        ("waiting", waits, _) => JobState::Waiting(waits.as_deref().map(Wait::parse).transpose()?),
        ("running", None, _) => JobState::Running,
        ("done", None, _) => JobState::Done,
        ("failed", None, detail) => JobState::Failed(detail.unwrap_or_default()),
        (state, waits, _) => {
            return Err(StoreError::Corrupt(format!(
                "job {id} is {state:?} waiting for {waits:?}"
            )));
        }
    };
    Ok(Job {
        id: JobId(id),
        session: parse_session(session)?,
        kind: JobKind::parse(&kind)?,
        state,
        attempts: u32::try_from(attempts)
            .map_err(|_| StoreError::Corrupt(format!("job {id} attempts {attempts}")))?,
        progress: Progress {
            done: count(progress, "progress")?,
            total: count(total, "total")?,
        },
    })
}

/// Queues every kind of job for session `key`, waiting for `waits`,
/// inside the caller's transaction. A kind the session has already is
/// left as it is.
pub(crate) fn queue(conn: &Connection, key: i64, waits: Option<Wait>) -> Result<(), StoreError> {
    for kind in JobKind::ALL {
        conn.execute(
            "INSERT INTO job (session_id, kind, state, waits_for) VALUES (?1, ?2, 'waiting', ?3) \
             ON CONFLICT (session_id, kind) DO NOTHING",
            params![key, kind.as_str(), waits.map(Wait::as_str)],
        )?;
    }
    Ok(())
}

fn to_i64(n: u64) -> Result<i64, StoreError> {
    i64::try_from(n).map_err(|_| StoreError::OutOfRange)
}

impl Store {
    /// Marks the session stopped and queues its jobs, in one transaction
    /// committed before returning. After a full disk, `waits` is
    /// [`Wait::Space`], so the jobs wait for space rather than fail. A
    /// session stopped already keeps the jobs it has.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSession`] if it isn't in the library, and
    /// [`StoreError::Sqlite`] for any SQLite failure. Nothing changes then.
    pub fn finish_recording(
        &mut self,
        session: SessionId,
        waits: Option<Wait>,
    ) -> Result<(), StoreError> {
        let key = session_key(session)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE session SET state = ?2 WHERE id = ?1",
            params![key, SessionState::Stopped.as_str()],
        )?;
        if changed == 0 {
            return Err(StoreError::NoSession(session));
        }
        queue(&tx, key, waits)?;
        tx.commit()?;
        Ok(())
    }

    /// Every job, in the order they run.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if a row doesn't parse, and
    /// [`StoreError::Sqlite`] for any SQLite failure.
    pub fn jobs(&self) -> Result<Vec<Job>, StoreError> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {JOB_COLUMNS} FROM job ORDER BY id"))?;
        let raws = stmt
            .query_map([], raw_job)?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter().map(parse_job).collect()
    }

    /// The session's jobs, in the order they run.
    ///
    /// # Errors
    ///
    /// As [`Store::jobs`].
    pub fn session_jobs(&self, session: SessionId) -> Result<Vec<Job>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM job WHERE session_id = ?1 ORDER BY id"
        ))?;
        let raws = stmt
            .query_map([session_key(session)?], raw_job)?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter().map(parse_job).collect()
    }

    /// Takes a waiting job to run: it's [`JobState::Running`] from now,
    /// with the progress it had. Says whether it was waiting; one that
    /// wasn't (another runner took it, or it ended) is left as it is.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] for any SQLite failure.
    pub fn start_job(&mut self, id: JobId) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "UPDATE job SET state = 'running', waits_for = NULL, detail = NULL \
             WHERE id = ?1 AND state = 'waiting'",
            [id.0],
        )?;
        Ok(changed == 1)
    }

    /// Notes a running job's progress.
    ///
    /// # Errors
    ///
    /// [`StoreError::OutOfRange`] if a count doesn't fit SQLite's integer,
    /// and [`StoreError::Sqlite`] for any SQLite failure.
    pub fn job_progress(&mut self, id: JobId, progress: Progress) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE job SET progress = ?2, total = ?3 WHERE id = ?1 AND state = 'running'",
            params![id.0, to_i64(progress.done)?, to_i64(progress.total)?],
        )?;
        Ok(())
    }

    /// Ends a run of the job, as `end` says.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoJob`] if there's no such job, and
    /// [`StoreError::Sqlite`] for any SQLite failure.
    pub fn end_job(&mut self, id: JobId, end: &JobEnd) -> Result<(), StoreError> {
        let (state, waits, detail) = match end {
            JobEnd::Done => ("done", None, None),
            JobEnd::Waiting(waits) => ("waiting", waits.map(Wait::as_str), None),
            JobEnd::Failed(why) => ("failed", None, Some(why.as_str())),
        };
        let changed = self.conn.execute(
            "UPDATE job SET state = ?2, waits_for = ?3, detail = ?4 WHERE id = ?1",
            params![id.0, state, waits, detail],
        )?;
        if changed == 0 {
            return Err(StoreError::NoJob(id.0));
        }
        Ok(())
    }

    /// Takes back the jobs a runner that died left running: each waits to
    /// run again, with one more attempt counted, or fails once `limit`
    /// attempts have died (a job that kills nota every time it runs mustn't
    /// keep doing so). Only the runner that holds the job queue calls this,
    /// before it runs anything. Gives how many were taken back.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] for any SQLite failure. Nothing changes then.
    pub fn recover_jobs(&mut self, limit: u32) -> Result<usize, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let taken = tx.execute(
            "UPDATE job SET state = 'waiting', attempts = attempts + 1 WHERE state = 'running'",
            [],
        )?;
        tx.execute(
            "UPDATE job SET state = 'failed', waits_for = NULL, \
             detail = 'nota stopped while running it ' || attempts || ' times' \
             WHERE state = 'waiting' AND attempts >= ?1",
            [i64::from(limit)],
        )?;
        tx.commit()?;
        Ok(taken)
    }
}

#[cfg(test)]
mod tests;
