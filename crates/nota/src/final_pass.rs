//! The final pass: the first job after a stop.
//!
//! The session's published segments are read back (each checked against
//! its row's hash), cut by the engine's voice activity detector at pauses
//! with a 25 s cap (`nota engine asr --pass final`), and transcribed by the
//! engine child under the supervisor, so each request has its timeout and
//! a hung engine is restarted, as on the live path (see
//! [`nota_recorder::engine::pass`]). The text is stored as the final
//! pass's, beside the heard text and never in it
//! ([`nota_store::final_text`]), with how far each track has got, so a
//! pass stopped for a recording, or by a crash, carries on from there and
//! covers each published sample once. Its progress counts samples.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use nota_core::messages::Transcript;
use nota_core::{Clock, SampleIndex, SampleRange, SessionId, TrackId};
use nota_recorder::engine::EngineCommand;
use nota_recorder::engine::pass::{PassConfig, PassEnd, PassSink, TrackAudio, transcribe};
use nota_recorder::fs::StdFs;
use nota_recorder::segment::read_segment;
use nota_store::{
    FinalText, HeardBy, Job, JobEnd, JobKind, Progress, SegmentRow, StoreError, Wait, Writer,
};

use crate::jobs::{Running, Worker};
use crate::library::Library;
use crate::record::RATE;

/// The engine the final pass runs, and what to store as having heard its
/// text.
#[derive(Debug, Clone)]
pub(crate) struct Engine {
    /// Starts the engine child cutting at the final pass's cap.
    pub(crate) command: EngineCommand,
    /// Its engine and model.
    pub(crate) heard_by: HeardBy,
}

/// Runs the jobs queued after a stop: for now, the final pass.
pub(crate) struct Jobs {
    library: Library,
    /// `None` without a speech engine: its jobs wait for one.
    engine: Option<Engine>,
    clock: Arc<dyn Clock>,
}

impl Jobs {
    pub(crate) const fn new(
        library: Library,
        engine: Option<Engine>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            library,
            engine,
            clock,
        }
    }
}

impl Worker for Jobs {
    fn run(&mut self, job: &Job, running: &Running<'_>) -> JobEnd {
        match job.kind {
            JobKind::FinalPass => match &self.engine {
                Some(engine) => {
                    final_pass(&self.library, engine, &self.clock, job.session, running)
                }
                None => JobEnd::Waiting(Some(Wait::Engine)),
            },
        }
    }

    fn lacks(&self, job: &Job) -> Option<Wait> {
        match job.kind {
            JobKind::FinalPass => self.engine.is_none().then_some(Wait::Engine),
        }
    }
}

/// Runs the final pass over `session` until it's done or `running` says to
/// stop.
fn final_pass(
    library: &Library,
    engine: &Engine,
    clock: &Arc<dyn Clock>,
    session: SessionId,
    running: &Running<'_>,
) -> JobEnd {
    let db = library.db();
    let found = db.with(|db| Ok((db.segments(session)?, db.final_progress(session)?)));
    let (rows, from) = match found {
        Ok(found) => found,
        Err(e) => return stored(&e),
    };
    let tracks = tracks(rows, &from);
    let mut sink = Stored::new(
        db.clone(),
        session,
        engine.heard_by.clone(),
        &tracks,
        running,
    );
    (running.progress)(sink.progress());
    let audio: PathBuf = library.session(session).audio();
    let ended = transcribe(
        &tracks,
        |row| read_segment(&StdFs, &audio, row, RATE),
        &PassConfig::final_pass(engine.command.clone()),
        clock,
        &mut sink,
        running.stop,
    );
    match ended {
        PassEnd::Done => JobEnd::Done,
        PassEnd::Stopped => JobEnd::Waiting(None),
        PassEnd::Sink(e) => stored(&e),
        end @ (PassEnd::Stalled(_) | PassEnd::Segment(..) | PassEnd::Engine(_)) => {
            JobEnd::Failed(end.to_string())
        }
    }
}

/// How a job ends on a store error: waiting for space if the disk is
/// full, failed otherwise.
fn stored(e: &StoreError) -> JobEnd {
    if e.is_disk_full() {
        JobEnd::Waiting(Some(Wait::Space))
    } else {
        JobEnd::Failed(format!("the library database failed: {e}"))
    }
}

/// Each track's segments, from where the pass got to on it.
fn tracks(rows: Vec<SegmentRow>, from: &BTreeMap<TrackId, SampleIndex>) -> Vec<TrackAudio> {
    let mut by_track: BTreeMap<TrackId, Vec<SegmentRow>> = BTreeMap::new();
    for row in rows {
        by_track.entry(row.track()).or_default().push(row);
    }
    by_track
        .into_iter()
        .map(|(track, mut segments)| {
            segments.sort_by_key(|row| row.range().start());
            TrackAudio {
                track,
                segments,
                from: from.get(&track).copied().unwrap_or(SampleIndex::ZERO),
            }
        })
        .collect()
}

/// The final pass's results, stored as they're confirmed.
struct Stored<'a> {
    db: Writer,
    session: SessionId,
    heard_by: HeardBy,
    /// Each track's published samples, and how far the pass has got.
    tracks: BTreeMap<TrackId, (Vec<SampleRange>, SampleIndex)>,
    report: &'a dyn Fn(Progress),
}

impl<'a> Stored<'a> {
    fn new(
        db: Writer,
        session: SessionId,
        heard_by: HeardBy,
        tracks: &[TrackAudio],
        running: &'a Running<'a>,
    ) -> Self {
        Self {
            db,
            session,
            heard_by,
            tracks: tracks
                .iter()
                .map(|t| {
                    let ranges = t.segments.iter().map(SegmentRow::range).collect();
                    (t.track, (ranges, t.from))
                })
                .collect(),
            report: running.progress,
        }
    }

    /// The published samples done, of all of them.
    fn progress(&self) -> Progress {
        let mut progress = Progress::default();
        for (ranges, up_to) in self.tracks.values() {
            for range in ranges {
                let len = range.len().get();
                progress.total = progress.total.saturating_add(len);
                let done = up_to.saturating_count_since(range.start()).get().min(len);
                progress.done = progress.done.saturating_add(done);
            }
        }
        progress
    }
}

impl PassSink for Stored<'_> {
    type Error = StoreError;

    fn confirmed(
        &mut self,
        track: TrackId,
        up_to: SampleIndex,
        texts: Vec<Transcript>,
        skipped: Vec<SampleRange>,
    ) -> Result<(), StoreError> {
        let heard = texts.into_iter().map(|text| FinalText {
            track,
            range: text.range(),
            text: Some(text.into_text()),
            // The engine gives no word times yet.
            words: Vec::new(),
            heard_by: self.heard_by.clone(),
        });
        let lost = skipped.into_iter().map(|range| FinalText {
            track,
            range,
            text: None,
            words: Vec::new(),
            heard_by: self.heard_by.clone(),
        });
        let mut finals: Vec<FinalText> = heard.chain(lost).collect();
        finals.sort_by_key(|text| text.range.start());
        self.db
            .with(|db| db.add_final_text(self.session, track, up_to, &finals))?;
        if let Some((_, done)) = self.tracks.get_mut(&track) {
            *done = (*done).max(up_to);
        }
        (self.report)(self.progress());
        Ok(())
    }
}

#[cfg(test)]
mod tests;
