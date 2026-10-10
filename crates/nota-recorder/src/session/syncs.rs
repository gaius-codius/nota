//! Where a session's journal fsyncs run: on the writer's own thread, or on
//! a thread per track.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::sync::mpsc;
use std::thread;

use nota_core::TrackId;

use crate::fs::FileSyncer;
use crate::journal::{PendingSync, SyncDone};

/// Where a [`SessionWriter`](super::SessionWriter)'s journal fsyncs run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Syncing {
    /// On the thread calling the writer, as each is started: the call
    /// waits for it. One track's fsync then holds up every other track's
    /// audio on that thread. Deterministic, for tests and tools.
    #[default]
    Inline,
    /// On a thread for each track, while the writer goes on appending: a
    /// track's fsync holds up only its own durable position.
    Threads,
    /// Inline while the writer records one track; once a second track
    /// starts, on a thread for each track, the first one's too. For
    /// recording: a recording of one track takes the path the crash tests
    /// sweep, and one track's fsync never holds up another's audio.
    Auto,
    /// Held until the test runs them, so a test can complete them at any
    /// point.
    #[cfg(test)]
    Manual,
}

/// One track's fsyncs, completed in the order they were started.
pub(super) struct TrackSyncs<Y> {
    runner: Runner<Y>,
    /// For each sync started and not yet completed, oldest first, what to
    /// complete it with if it's lost.
    outstanding: VecDeque<SyncDone>,
}

enum Runner<Y> {
    /// Each already run, its result waiting to be taken.
    Inline(VecDeque<SyncDone>),
    Thread {
        jobs: mpsc::Sender<PendingSync<Y>>,
        done: mpsc::Receiver<SyncDone>,
    },
    #[cfg(test)]
    Manual {
        queued: VecDeque<PendingSync<Y>>,
        done: VecDeque<SyncDone>,
    },
}

impl<Y: FileSyncer> Runner<Y> {
    /// A runner on a new thread for `track`'s syncs, and a sender into
    /// the results it hands back, ahead of the thread's own.
    fn thread(track: TrackId) -> io::Result<(Self, mpsc::Sender<SyncDone>)> {
        let (jobs, queued) = mpsc::channel::<PendingSync<Y>>();
        let (finished, done) = mpsc::channel();
        let ahead = finished.clone();
        thread::Builder::new()
            .name(format!("nota-sync-{}", track.get()))
            .spawn(move || {
                for job in queued {
                    if finished.send(job.run()).is_err() {
                        break;
                    }
                }
            })?;
        Ok((Self::Thread { jobs, done }, ahead))
    }
}

impl<Y> fmt::Debug for TrackSyncs<Y> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match &self.runner {
            Runner::Inline(_) => "inline",
            Runner::Thread { .. } => "thread",
            #[cfg(test)]
            Runner::Manual { .. } => "manual",
        };
        f.debug_struct("TrackSyncs")
            .field("runner", &kind)
            .field("outstanding", &self.outstanding.len())
            .finish()
    }
}

impl<Y: FileSyncer> TrackSyncs<Y> {
    /// `track`'s fsyncs, run as `syncing` says. With
    /// [`Syncing::Threads`], it starts the track's sync thread, which ends
    /// once this is dropped and the syncs already handed to it have run.
    /// [`Syncing::Auto`] starts inline: the writer moves the track to a
    /// thread ([`Self::move_to_thread`]) when a second one starts.
    ///
    /// # Errors
    ///
    /// If the thread can't be started.
    pub(super) fn new(syncing: Syncing, track: TrackId) -> io::Result<Self> {
        let runner = match syncing {
            Syncing::Inline | Syncing::Auto => Runner::Inline(VecDeque::new()),
            Syncing::Threads => {
                let (runner, _) = Runner::thread(track)?;
                runner
            }
            #[cfg(test)]
            Syncing::Manual => Runner::Manual {
                queued: VecDeque::new(),
                done: VecDeque::new(),
            },
        };
        Ok(Self {
            runner,
            outstanding: VecDeque::new(),
        })
    }

    /// From now on, runs the fsyncs on a thread of the track's own, as
    /// [`Syncing::Threads`] does. Syncs already run inline and not yet
    /// taken come back first, in order. Nothing changes unless they ran
    /// inline.
    ///
    /// # Errors
    ///
    /// If the thread can't be started; the syncs then go on running
    /// inline, with nothing lost.
    pub(super) fn move_to_thread(&mut self, track: TrackId) -> io::Result<()> {
        let Runner::Inline(ran) = &mut self.runner else {
            return Ok(());
        };
        let (runner, finished) = Runner::thread(track)?;
        // Ahead of anything the thread runs: it has been handed nothing yet.
        for done in ran.drain(..) {
            // The receiver is in `runner`, here, so this can't fail.
            let _ = finished.send(done);
        }
        self.runner = runner;
        Ok(())
    }

    /// Which way the fsyncs run now: [`Syncing::Inline`] or
    /// [`Syncing::Threads`] (or [`Syncing::Manual`]), never
    /// [`Syncing::Auto`].
    pub(super) const fn syncing(&self) -> Syncing {
        match self.runner {
            Runner::Inline(_) => Syncing::Inline,
            Runner::Thread { .. } => Syncing::Threads,
            #[cfg(test)]
            Runner::Manual { .. } => Syncing::Manual,
        }
    }

    /// Hands `job` over to run: at once, inline.
    pub(super) fn start(&mut self, job: PendingSync<Y>) {
        self.outstanding.push_back(job.lost());
        match &mut self.runner {
            Runner::Inline(done) => done.push_back(job.run()),
            // A thread that has gone takes nothing: the job is completed as
            // lost when its result is looked for.
            Runner::Thread { jobs, .. } => drop(jobs.send(job)),
            #[cfg(test)]
            Runner::Manual { queued, .. } => queued.push_back(job),
        }
    }

    /// Whether a sync has been started and not yet taken by
    /// [`Self::try_next`] or [`Self::next`].
    #[cfg(test)]
    pub(super) fn is_pending(&self) -> bool {
        !self.outstanding.is_empty()
    }

    /// The oldest completed sync not yet taken, without waiting.
    pub(super) fn try_next(&mut self) -> Option<SyncDone> {
        let done = match &mut self.runner {
            Runner::Inline(done) => done.pop_front(),
            Runner::Thread { done, .. } => match done.try_recv() {
                Ok(done) => Some(done),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => return self.outstanding.pop_front(),
            },
            #[cfg(test)]
            Runner::Manual { done, .. } => done.pop_front(),
        };
        if done.is_some() {
            self.outstanding.pop_front();
        }
        done
    }

    /// The oldest sync not yet taken, waiting for it to complete; `None`
    /// if none is pending.
    pub(super) fn next(&mut self) -> Option<SyncDone> {
        if self.outstanding.is_empty() {
            return None;
        }
        let done = match &mut self.runner {
            Runner::Inline(done) => done.pop_front(),
            Runner::Thread { done, .. } => done.recv().ok(),
            #[cfg(test)]
            Runner::Manual { queued, done } => done
                .pop_front()
                .or_else(|| queued.pop_front().map(PendingSync::run)),
        };
        let lost = self.outstanding.pop_front();
        done.or(lost)
    }

    /// Runs the oldest held sync, with [`Syncing::Manual`]; returns whether
    /// there was one.
    #[cfg(test)]
    pub(super) fn run_one(&mut self) -> bool {
        match &mut self.runner {
            Runner::Manual { queued, done } => match queued.pop_front() {
                Some(job) => {
                    done.push_back(job.run());
                    true
                }
                None => false,
            },
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use nota_core::{Clock, EpochId, FakeClock, SampleIndex, SampleRate, SessionTime};

    use super::*;
    use crate::fs::FsFile;
    use crate::fs::fake::{FakeFile, FakeFs};
    use crate::journal::{JournalHeader, JournalId, JournalWriter};

    const TRACK: TrackId = TrackId::new(0);

    fn journal(fs: &FakeFs) -> JournalWriter<FakeFile> {
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
        let header =
            JournalHeader::new(JournalId::FIRST, TRACK, EpochId::new(0), SampleRate::SPEECH);
        JournalWriter::create(fs, Path::new("/s"), header, SampleIndex::ZERO, clock).unwrap()
    }

    /// Starts two syncs of `j`, after 10 and 20 samples.
    fn two_syncs(
        syncs: &mut TrackSyncs<<FakeFile as FsFile>::Syncer>,
        j: &mut JournalWriter<FakeFile>,
    ) {
        j.append_within(&[1; 10]).unwrap();
        syncs.start(j.begin_sync().unwrap());
        j.append_within(&[2; 10]).unwrap();
        syncs.start(j.begin_sync().unwrap());
    }

    #[test]
    fn inline_syncs_run_at_once_and_come_back_in_order() {
        let fs = FakeFs::with_dirs(["/s"]);
        let mut j = journal(&fs);
        let mut syncs = TrackSyncs::new(Syncing::Inline, TRACK).unwrap();
        assert!(!syncs.is_pending());
        assert!(syncs.next().is_none());
        two_syncs(&mut syncs, &mut j);
        assert!(syncs.is_pending());
        j.complete_sync(syncs.try_next().unwrap()).unwrap();
        assert_eq!(j.durable().end().get(), 10);
        j.complete_sync(syncs.next().unwrap()).unwrap();
        assert_eq!(j.durable().end().get(), 20);
        assert!(!syncs.is_pending());
        assert!(syncs.try_next().is_none());
    }

    #[test]
    fn syncs_on_a_thread_come_back_in_order() {
        let fs = FakeFs::with_dirs(["/s"]);
        let mut j = journal(&fs);
        let mut syncs = TrackSyncs::new(Syncing::Threads, TRACK).unwrap();
        assert!(format!("{syncs:?}").contains("thread"));
        let stall = fs.stall_syncs(&PathBuf::from("/s").join(JournalId::FIRST.file_name()));
        two_syncs(&mut syncs, &mut j);
        assert!(stall.wait_for_held(1, std::time::Duration::from_secs(10)));
        assert!(syncs.try_next().is_none(), "held on the thread");
        assert!(syncs.is_pending());
        stall.release();
        j.complete_sync(syncs.next().unwrap()).unwrap();
        assert_eq!(j.durable().end().get(), 10);
        j.complete_sync(syncs.next().unwrap()).unwrap();
        assert_eq!(j.durable().end().get(), 20);
        assert!(!syncs.is_pending());
        assert!(syncs.next().is_none());
    }

    /// Syncs run inline before the move come back first, in order, then
    /// the thread's.
    #[test]
    fn syncs_moved_to_a_thread_come_back_after_those_run_inline() {
        let fs = FakeFs::with_dirs(["/s"]);
        let mut j = journal(&fs);
        let mut syncs = TrackSyncs::new(Syncing::Auto, TRACK).unwrap();
        assert_eq!(syncs.syncing(), Syncing::Inline);
        two_syncs(&mut syncs, &mut j);
        syncs.move_to_thread(TRACK).unwrap();
        assert_eq!(syncs.syncing(), Syncing::Threads);
        j.append_within(&[3; 10]).unwrap();
        syncs.start(j.begin_sync().unwrap());
        for end in [10, 20, 30] {
            j.complete_sync(syncs.next().unwrap()).unwrap();
            assert_eq!(j.durable().end().get(), end);
        }
        assert!(!syncs.is_pending());
        // Moving again changes nothing.
        syncs.move_to_thread(TRACK).unwrap();
        assert_eq!(syncs.syncing(), Syncing::Threads);
        assert!(syncs.next().is_none());
    }

    #[test]
    fn syncs_a_gone_thread_never_ran_come_back_as_lost() {
        let fs = FakeFs::with_dirs(["/s"]);
        let mut j = journal(&fs);
        let (jobs, _) = mpsc::channel();
        let (_, done) = mpsc::channel();
        let mut syncs = TrackSyncs {
            runner: Runner::Thread { jobs, done },
            outstanding: VecDeque::new(),
        };
        two_syncs(&mut syncs, &mut j);
        assert!(j.complete_sync(syncs.try_next().unwrap()).is_err());
        assert!(j.is_broken());
        assert!(syncs.is_pending());
        assert!(syncs.next().is_some());
        assert!(!syncs.is_pending());
        assert!(syncs.try_next().is_none());
    }

    #[test]
    fn manual_syncs_wait_for_the_test_or_a_wait() {
        let fs = FakeFs::with_dirs(["/s"]);
        let mut j = journal(&fs);
        let mut syncs = TrackSyncs::new(Syncing::Manual, TRACK).unwrap();
        two_syncs(&mut syncs, &mut j);
        assert!(syncs.try_next().is_none());
        assert!(syncs.run_one());
        j.complete_sync(syncs.try_next().unwrap()).unwrap();
        assert_eq!(j.durable().end().get(), 10);
        assert!(syncs.try_next().is_none());
        // Waiting runs it.
        j.complete_sync(syncs.next().unwrap()).unwrap();
        assert_eq!(j.durable().end().get(), 20);
        assert!(!syncs.run_one());
        let mut inline =
            TrackSyncs::<<FakeFile as FsFile>::Syncer>::new(Syncing::Inline, TRACK).unwrap();
        assert!(!inline.run_one());
    }
}
