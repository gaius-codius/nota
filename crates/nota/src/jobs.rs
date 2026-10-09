//! The job runner: runs the jobs queued after each stop, one at a time,
//! in the order they were queued (see [`nota_store::jobs`]).
//!
//! **Capture first.** No job starts while a recording is going, in this
//! nota or another one, and a job running when one starts is stopped: at
//! once for this nota's recording, within a second or two for another's
//! (seen once it has written its first journal). It waits, keeping what
//! it committed, and carries on once the recording has stopped. Only the
//! live pass runs while recording.
//!
//! **One runner for the library.** The runner holds a lock in the data
//! directory (`jobs/`) for as long as it runs, so a second nota open on
//! the same library runs no jobs of its own: its runner waits for the
//! lock, and takes the queue over once the first nota closes or dies.
//! Holding it, the runner first
//! takes back the jobs a runner that died left running: they wait to run
//! again, and run on from what they committed. A job that nota has died
//! running [`MAX_ATTEMPTS`] times fails rather than taking nota down again.
//!
//! **Nothing holds the queue up.** A job runs only once what it waits for
//! is there ([`Wait`]): space, after a full disk; a speech engine; its
//! session's audio, all published. The
//! queue moves on past one that's waiting, and past one that fails (it
//! keeps its reason), to the next session's.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nota_core::{Clock, SessionTime, SystemClock};
use nota_recorder::fs::{Fs, StdFs};
use nota_store::{Job, JobEnd, JobState, Progress, StoreError, Wait, Writer};

/// A job that nota has died while running this many times fails.
pub(crate) const MAX_ATTEMPTS: u32 = 3;

/// How often an idle runner looks at the queue again without being woken:
/// space may have come back, or a recording elsewhere stopped.
const IDLE: Duration = Duration::from_secs(5);

/// How often a runner held off by a recording checks whether it has
/// stopped, and one waiting for another nota's runner tries its lock.
const HELD: Duration = Duration::from_secs(1);

/// What a job is given while it runs.
pub(crate) struct Running<'a> {
    /// Whether to stop now: a recording has started, or nota is closing.
    /// The job stops, keeping what it committed, and runs again later.
    pub(crate) stop: &'a dyn Fn() -> bool,
    /// Notes how far it has got, for the Processing screen.
    pub(crate) progress: &'a dyn Fn(Progress),
}

/// What runs the jobs.
pub(crate) trait Worker: Send + 'static {
    /// Runs `job` until it ends or `running.stop` says to stop (then it
    /// returns [`JobEnd::Waiting`] with nothing to wait for). Whatever it
    /// committed stays: run again, it carries on from there.
    fn run(&mut self, job: &Job, running: &Running<'_>) -> JobEnd;

    /// What `job` would wait for if it were run now, if anything: the
    /// final pass waits for a speech engine, and for its session's
    /// journals to be published.
    fn lacks(&self, job: &Job) -> Option<Wait>;
}

/// Whether a recording is going: nothing runs while one is.
pub(crate) trait Capture: Send + Sync + 'static {
    /// Whether one is going now.
    fn recording(&self) -> bool;
}

/// Whether there's space for jobs to write.
pub(crate) trait Room: Send + Sync + 'static {
    /// Whether there's room now.
    fn room(&self) -> bool;
}

/// Free space under the data directory: at least [`JOB_ROOM`].
#[derive(Debug, Clone)]
pub(crate) struct FreeSpace(pub(crate) std::path::PathBuf);

/// The space a job waiting for it needs: as much as the ballast a
/// recording keeps (256 MB), so jobs don't fill the room a recording needs
/// to finish.
pub(crate) const JOB_ROOM: u64 = nota_recorder::disk::BALLAST_LEN;

impl Room for FreeSpace {
    fn room(&self) -> bool {
        // A disk that can't say how full it is might have room: try.
        StdFs
            .free_space(&self.0)
            .map_or(true, |free| free >= JOB_ROOM)
    }
}

/// What the runner thread shares with its handle.
#[derive(Debug, Default)]
struct Shared {
    stopping: AtomicBool,
    /// Set when the runner should look at the queue now.
    woken: Mutex<bool>,
    wake: Condvar,
}

impl Shared {
    /// Waits up to `limit`, or until woken or stopping.
    fn sleep(&self, limit: Duration) {
        let woken = self.woken.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut woken, _) = self
            .wake
            .wait_timeout_while(woken, limit, |woken| {
                !*woken && !self.stopping.load(Ordering::SeqCst)
            })
            .unwrap_or_else(PoisonError::into_inner);
        *woken = false;
    }

    fn wake(&self) {
        *self.woken.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.wake.notify_all();
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }
}

/// The running runner. Dropping it stops it, pausing the job it runs.
#[derive(Debug)]
pub(crate) struct Runner {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Runner {
    /// Starts the runner for the library whose data directory is `data`
    /// and database `db`. While another nota runs the library's jobs, it
    /// waits, and takes over when that one stops.
    ///
    /// # Errors
    ///
    /// If the lock's directory can't be made, or the thread can't be
    /// started.
    pub(crate) fn spawn(
        data: &Path,
        db: Writer,
        worker: impl Worker,
        capture: impl Capture,
        room: impl Room,
    ) -> io::Result<Self> {
        let clock: Arc<dyn Clock> = Arc::new(
            SystemClock::start().map_err(|_| io::Error::other("the system clock can't be read"))?,
        );
        let dir = data.join("jobs");
        match StdFs.create_dir(&dir) {
            Ok(()) => StdFs.sync_dir(data)?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let shared = Arc::new(Shared::default());
        let looping = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("nota-jobs".into())
            .spawn(move || {
                // Another nota's runner, or a lock that can't be taken now
                // (the directory replaced, say): try again in a moment.
                let _lock = loop {
                    if looping.stopping() {
                        return;
                    }
                    match StdFs.lock_dir(&dir) {
                        Ok(lock) => break lock,
                        Err(_) => looping.sleep(HELD),
                    }
                };
                Loop {
                    db,
                    worker,
                    capture,
                    room,
                    shared: looping,
                    clock,
                    retry_after: BTreeMap::new(),
                }
                .run();
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Asks the runner to look at the queue now: a job was queued, or a
    /// recording stopped.
    pub(crate) fn wake(&self) {
        self.shared.wake();
    }

    fn stop(&mut self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        self.shared.wake();
        if let Some(thread) = self.thread.take() {
            // A panic in the runner has nothing left to clean up: its job
            // is taken back at the next start.
            let _ = thread.join();
        }
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The runner thread's state.
struct Loop<W, C, R> {
    db: Writer,
    worker: W,
    capture: C,
    room: R,
    shared: Arc<Shared>,
    clock: Arc<dyn Clock>,
    retry_after: BTreeMap<nota_store::JobId, SessionTime>,
}

impl<W: Worker, C: Capture, R: Room> Loop<W, C, R> {
    fn run(mut self) {
        // The jobs a runner that died left running, before anything else.
        while !self.shared.stopping() {
            match self.db.with(|db| db.recover_jobs(MAX_ATTEMPTS)) {
                Ok(_) => break,
                Err(_) => self.shared.sleep(IDLE),
            }
        }
        while !self.shared.stopping() {
            if self.capture.recording() {
                self.shared.sleep(HELD);
                continue;
            }
            match self.next() {
                Ok(Some(job)) => self.run_one(&job),
                // Nothing to run, or the database can't be read now.
                Ok(None) | Err(_) => self.shared.sleep(IDLE),
            }
        }
    }

    /// The first job that can run now, in queue order. A waiting job
    /// that lacks what it needs is marked as waiting for it, so the
    /// Processing screen can say why.
    fn next(&self) -> Result<Option<Job>, StoreError> {
        let jobs = self.db.with(|db| db.jobs())?;
        for job in jobs {
            let JobState::Waiting(waits) = job.state else {
                continue;
            };
            if self.retry_after.get(&job.id).is_some_and(|at| {
                self.clock
                    .now()
                    .checked_duration_since(*at)
                    .unwrap_or_default()
                    < IDLE
            }) {
                continue;
            }
            // A job waiting for space keeps waiting for it until there's
            // room, whatever else it lacks meanwhile.
            if waits == Some(Wait::Space) && !self.room.room() {
                continue;
            }
            if let Some(lacks) = self.worker.lacks(&job) {
                if waits != Some(lacks) && waits != Some(Wait::Space) {
                    self.db
                        .with(|db| db.end_job(job.id, &JobEnd::Waiting(Some(lacks))))?;
                }
                continue;
            }
            return Ok(Some(job));
        }
        Ok(None)
    }

    fn run_one(&mut self, job: &Job) {
        // Taken, or ended, meanwhile; or the database can't be written (a
        // full disk): look again in a while, not at once.
        if !matches!(self.db.with(|db| db.start_job(job.id)), Ok(true)) {
            self.shared.sleep(IDLE);
            return;
        }
        let shared = Arc::clone(&self.shared);
        let capture = &self.capture;
        let stop = move || shared.stopping() || capture.recording();
        let db = self.db.clone();
        let id = job.id;
        let progress = move |progress: Progress| {
            // Only shown; a progress that can't be noted is shown late.
            let _ = db.with(|db| db.job_progress(id, progress));
        };
        let end = self.worker.run(
            job,
            &Running {
                stop: &stop,
                progress: &progress,
            },
        );
        // Until it's noted, the job stays running in the database, and
        // nothing else runs: a job that ended must not be run again.
        while self.db.with(|db| db.end_job(id, &end)).is_err() {
            if self.shared.stopping() {
                // Taken back at the next start, and run again from what
                // it committed.
                return;
            }
            self.shared.sleep(IDLE);
        }
        if end == JobEnd::Waiting(Some(Wait::Engine)) {
            self.retry_after.insert(id, self.clock.now());
        } else {
            self.retry_after.remove(&id);
        }
        // Out of space by SQLite's count though the disk check finds room
        // (a quota, a full temp store): don't start it again at once.
        if end == JobEnd::Waiting(Some(Wait::Space)) {
            self.shared.sleep(IDLE);
        }
    }
}

#[cfg(test)]
mod tests;
