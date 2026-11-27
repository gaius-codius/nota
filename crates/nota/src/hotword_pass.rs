//! The hotword pass: the job between the final pass and term clean-up.
//!
//! A beam search decodes each final-pass chunk again with the session's
//! terms boosted (sherpa-onnx's hotwords), so a term the final pass misheard
//! has a second chance to be heard. Clean-up compares the two texts a word
//! at a time ([`nota_checks::hotword_candidates`]), and a difference that
//! brings in a listed term is one more candidate to ask the decider about.
//! The boosted text is only ever compared: it's never shown, and never
//! merged into a transcript.
//!
//! **One chunk at a time.** Each chunk the final pass heard text in is fed
//! to the engine on its own (`nota engine asr --pass hotword`, bounded by
//! [`TrackAudio::until`]), so the beam decodes exactly that chunk and its
//! text is stored against it ([`nota_store::HotwordText`]). A chunk the
//! final pass heard nothing in isn't fed: it's stored as text-less once
//! the pass has got past it.
//!
//! **Safe to stop.** Texts are committed a whole chunk at a time, with how
//! far the pass has got on the track, never a sample mid-chunk. A run
//! stopped for a recording, or killed, starts again at the first chunk it
//! didn't commit and decodes that chunk whole, so it stores what a run
//! never stopped stores.
//!
//! **The hotwords** are the terms of the session's packs, in the session's
//! order, each once ([`crate::cleanup::CleanUp::hotwords`]); with none,
//! there's nothing to boost and the job is done without an engine. The
//! boost is the setting `[hotword_pass] boost`, read at each run.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::Arc;

use nota_core::messages::{ToEngine, Transcript};
use nota_core::{Clock, SampleIndex, SampleRange, SessionId, TrackId};
use nota_recorder::engine::EngineCommand;
use nota_recorder::engine::pass::{PassConfig, PassSink, TrackAudio, transcribe};
use nota_recorder::fs::StdFs;
use nota_recorder::segment::read_segment;
use nota_store::{
    FinalText, HotwordText, JobEnd, JobKind, JobState, Progress, SegmentRow, StoreError, Wait,
    Writer,
};

use crate::cleanup::CleanUp;
use crate::final_pass::{pass_end, stored};
use crate::jobs::Running;
use crate::library::Library;
use crate::record::RATE;

#[cfg(test)]
mod tests;

/// The file in a session's directory that the real engine writes its
/// vocabulary to, matched to its tokenizer, for its beam search to read.
pub(crate) const VOCAB: &str = "hotword-vocab.txt";

/// How strongly the beam favours a hotword: sherpa-onnx's
/// `hotwords_score`, above 0 and at most [`Boost::MAX`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Boost(f64);

impl Boost {
    /// The boost without a setting: the score the bench measured the pass
    /// at.
    pub(crate) const DEFAULT: Self = Self(3.0);

    /// The highest boost taken: the engine's `--boost` takes no more.
    pub(crate) const MAX: f64 = 10.0;

    /// The boost `score`, or `None` unless it's at least the smallest
    /// normal `f32` (the engine reads it as one, so a smaller score would
    /// reach it as 0, which it refuses) and at most [`Boost::MAX`] (so
    /// never NaN or infinite).
    pub(crate) fn new(score: f64) -> Option<Self> {
        (score >= f64::from(f32::MIN_POSITIVE) && score <= Self::MAX).then_some(Self(score))
    }

    /// The score.
    pub(crate) const fn get(self) -> f64 {
        self.0
    }
}

/// Where a run finds its boost: the user's settings, read at each run, or
/// what a test gives. `Err` is why it can't be used, for the user.
pub(crate) type BoostSource = Box<dyn Fn() -> Result<Boost, String> + Send>;

/// The boost the user's settings give now: on unix, the setting
/// `[hotword_pass] boost` ([`crate::settings::hotword_boost`]);
/// elsewhere, where no settings are read, [`Boost::DEFAULT`].
///
/// # Errors
///
/// Why the setting can't be used, naming it.
pub(crate) fn settings_boost() -> Result<Boost, String> {
    #[cfg(unix)]
    let boost = crate::settings::hotword_boost(&crate::settings::Environment::current())
        .map_err(|why| format!("the hotword pass's boost can't be used: {why}"));
    #[cfg(not(unix))]
    let boost = Ok(Boost::DEFAULT);
    boost
}

/// How the hotword pass's engine is started.
#[derive(Debug, Clone)]
pub(crate) enum HotwordEngine {
    /// nota's stand-in, `nota engine fake --pass hotword`, which takes no
    /// boost or vocabulary file.
    Stand(EngineCommand),
    /// The real engine's command through `--pass hotword`: each run adds
    /// `--boost` and `--vocab`.
    Asr(EngineCommand),
}

impl HotwordEngine {
    /// The command for a run at `boost` whose engine writes its vocabulary
    /// to `vocab`.
    fn command(&self, boost: Boost, vocab: &Path) -> EngineCommand {
        match self {
            Self::Stand(command) => command.clone(),
            Self::Asr(command) => {
                let mut command = command.clone();
                command.args.extend([
                    "--boost".into(),
                    boost.get().to_string().into(),
                    "--vocab".into(),
                    vocab.as_os_str().to_owned(),
                ]);
                command
            }
        }
    }
}

/// What a hotword run needs from the jobs runner.
pub(crate) struct Pass<'a> {
    /// The library.
    pub(crate) library: &'a Library,
    /// The engine, if nota has speech models.
    pub(crate) engine: Option<&'a HotwordEngine>,
    /// Times the engine's requests.
    pub(crate) clock: &'a Arc<dyn Clock>,
    /// Gives the session's hotwords, as clean-up compares them.
    pub(crate) clean_up: &'a CleanUp,
    /// Gives the boost.
    pub(crate) boost: &'a dyn Fn() -> Result<Boost, String>,
}

/// What a hotword job of `session` waits for now, if anything: the
/// session's final pass, then (if it has hotwords) an engine. A failed
/// final pass, or one that can't be read, is reported by the run.
pub(crate) fn lacks(pass: &Pass<'_>, session: SessionId) -> Option<Wait> {
    match job_state(pass.library.db(), session, JobKind::FinalPass) {
        Ok(Some(JobState::Done) | None) => {}
        Ok(Some(JobState::Failed(_))) | Err(_) => return None,
        Ok(Some(_)) => return Some(Wait::FinalText),
    }
    // Without hotwords, or without a chunk left to feed, the run is done
    // at once, engine or not, and clean-up mustn't wait behind it for one.
    if pass.engine.is_some() {
        return None;
    }
    let db = pass.library.db();
    let boosted = pass
        .clean_up
        .hotwords(db, session)
        .is_ok_and(|hotwords| !hotwords.is_empty());
    (boosted && feeds_left(db, session)).then_some(Wait::Engine)
}

/// Whether `session` has a final-pass chunk with text that the hotword
/// pass hasn't done yet. One that can't be read is taken to have one: the
/// run reports the error.
fn feeds_left(db: &Writer, session: SessionId) -> bool {
    let found = db.with(|db| Ok((db.final_texts(session)?, db.hotword_progress(session)?)));
    let Ok((finals, progress)) = found else {
        return true;
    };
    finals.iter().any(|text| {
        let from = progress
            .get(&text.track)
            .copied()
            .unwrap_or(SampleIndex::ZERO);
        text.text.is_some() && text.range.end() > from
    })
}

/// The state of `session`'s job of `kind`, if it has one.
pub(crate) fn job_state(
    db: &Writer,
    session: SessionId,
    kind: JobKind,
) -> Result<Option<JobState>, StoreError> {
    let jobs = db.with(|db| db.session_jobs(session))?;
    Ok(jobs
        .into_iter()
        .find(|job| job.kind == kind)
        .map(|job| job.state))
}

/// Runs the hotword pass over `session` until it's done or `running`
/// says to stop.
pub(crate) fn run(pass: &Pass<'_>, session: SessionId, running: &Running<'_>) -> JobEnd {
    let db = pass.library.db();
    match job_state(db, session, JobKind::FinalPass) {
        Ok(Some(JobState::Done) | None) => {}
        Ok(Some(JobState::Failed(why))) => {
            return JobEnd::Failed(format!("there's no final transcript to boost: {why}"));
        }
        Ok(Some(_)) => return JobEnd::Waiting(Some(Wait::FinalText)),
        Err(e) => return stored(&e),
    }
    let hotwords = match pass.clean_up.hotwords(db, session) {
        Ok(hotwords) if hotwords.is_empty() => return JobEnd::Done,
        Ok(hotwords) => hotwords,
        Err(end) => return end,
    };
    let found = db.with(|db| {
        Ok((
            db.segments(session)?,
            db.final_texts(session)?,
            db.hotword_progress(session)?,
        ))
    });
    let work = match found {
        Ok((rows, finals, progress)) => Work::new(rows, &finals, &progress),
        Err(e) => return stored(&e),
    };
    if work.is_done() {
        return JobEnd::Done;
    }
    // Read only once there's work, so a bad setting fails a job that would
    // use it, and never one with nothing to boost.
    let boost = match (pass.boost)() {
        Ok(boost) => boost,
        Err(why) => return JobEnd::Failed(why),
    };
    let mut sink = Heard::new(db.clone(), session, boost, &work, running);
    (running.progress)(sink.progress());
    if work.feeds.is_empty() {
        return finished(&mut sink);
    }
    let Some(engine) = pass.engine else {
        return JobEnd::Waiting(Some(Wait::Engine));
    };
    let paths = pass.library.session(session);
    let mut config = PassConfig::final_pass(engine.command(boost, &paths.dir.join(VOCAB)));
    // Sent after every hello, so an engine restarted mid-pass is boosted
    // again before it hears anything.
    config.engine.greeting = vec![ToEngine::Hotwords(hotwords)];
    let audio = paths.audio();
    let ended = transcribe(
        &work.feeds,
        |row| read_segment(&StdFs, &audio, row, RATE),
        &config,
        pass.clock,
        &mut sink,
        running.stop,
    );
    match pass_end(ended) {
        JobEnd::Done => finished(&mut sink),
        end => end,
    }
}

/// How a run ends once the engine has done every chunk: done, once the
/// chunks left (those it wasn't fed) are stored; but failed if the engines
/// skipped audio and never once confirmed any themselves. That's an engine
/// that can't be boosted at all (its vocabulary can't be written, or its
/// beam recognizer can't be built), which dies at every chunk until each
/// is skipped: a pass that heard nothing mustn't read as done.
fn finished(sink: &mut Heard<'_>) -> JobEnd {
    if let Err(e) = sink.finish() {
        return stored(&e);
    }
    if sink.lost && !sink.told {
        return JobEnd::Failed(
            "the boosted engine heard none of the audio: it kept failing on every chunk".to_owned(),
        );
    }
    JobEnd::Done
}

/// One final-pass chunk the pass has still to store.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Chunk {
    /// Its samples, exactly its `final_text` row's.
    range: SampleRange,
    /// Whether it's fed to the engine: the final pass heard text in it.
    fed: bool,
}

/// What a run has to do, from what's stored.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Work {
    /// The audio of each chunk to feed, in order.
    feeds: Vec<TrackAudio>,
    /// Each track's chunks still to store, in sample order.
    chunks: BTreeMap<TrackId, Vec<Chunk>>,
    /// Each track's chunks with text, done or not, which the job's progress
    /// counts.
    spoken: BTreeMap<TrackId, Vec<SampleRange>>,
    /// How far earlier runs got on each track.
    from: BTreeMap<TrackId, SampleIndex>,
}

impl Work {
    /// The work left in a session with segment `rows` and final-pass
    /// chunks `finals`, earlier runs having got to `from` on each track.
    fn new(
        rows: Vec<SegmentRow>,
        finals: &[FinalText],
        from: &BTreeMap<TrackId, SampleIndex>,
    ) -> Self {
        let mut segments: BTreeMap<TrackId, Vec<SegmentRow>> = BTreeMap::new();
        for row in rows {
            segments.entry(row.track()).or_default().push(row);
        }
        for rows in segments.values_mut() {
            rows.sort_by_key(|row| row.range().start());
        }
        let mut sorted: Vec<&FinalText> = finals.iter().collect();
        sorted.sort_by_key(|text| (text.track, text.range.start()));
        let mut work = Self {
            feeds: Vec::new(),
            chunks: BTreeMap::new(),
            spoken: BTreeMap::new(),
            from: from.clone(),
        };
        for text in sorted {
            work.add(text, &segments);
        }
        work
    }

    /// Adds the final-pass chunk `text` to the work, fed from `segments`
    /// if it holds text and isn't done.
    fn add(&mut self, text: &FinalText, segments: &BTreeMap<TrackId, Vec<SegmentRow>>) {
        let (track, range) = (text.track, text.range);
        let fed = text.text.is_some();
        if fed {
            self.spoken.entry(track).or_default().push(range);
        }
        let from = self.from.get(&track).copied().unwrap_or(SampleIndex::ZERO);
        if range.end() <= from {
            return;
        }
        if fed {
            self.feeds.push(TrackAudio {
                track,
                segments: segments.get(&track).cloned().unwrap_or_default(),
                from: range.start(),
                until: Some(range.end()),
            });
        }
        self.chunks
            .entry(track)
            .or_default()
            .push(Chunk { range, fed });
    }

    /// Whether every chunk is stored already.
    fn is_done(&self) -> bool {
        self.chunks.values().all(Vec::is_empty)
    }
}

/// A chunk waiting to be stored, with what the engine heard in it so far.
#[derive(Debug)]
struct Pending {
    /// The chunk.
    chunk: Chunk,
    /// The texts heard in it, in order.
    texts: Vec<String>,
    /// Whether the engine skipped any of its audio.
    lost: bool,
}

impl Pending {
    /// The row to store for it: its texts joined by single spaces, or no
    /// text if it heard none or lost some of its audio.
    fn row(&self, track: TrackId, boost: Boost) -> HotwordText {
        let heard: Vec<&str> = self
            .texts
            .iter()
            .map(|text| text.trim())
            .filter(|text| !text.is_empty())
            .collect();
        HotwordText {
            track,
            range: self.chunk.range,
            text: (!self.lost && !heard.is_empty()).then(|| heard.join(" ")),
            boost: boost.get(),
        }
    }
}

/// The hotword pass's results, stored a whole chunk at a time.
struct Heard<'a> {
    db: Writer,
    session: SessionId,
    boost: Boost,
    /// Each track's chunks not yet stored, in order.
    pending: BTreeMap<TrackId, VecDeque<Pending>>,
    /// Each track's chunks with text, and how far the engine has got.
    spoken: BTreeMap<TrackId, (Vec<SampleRange>, SampleIndex)>,
    report: &'a dyn Fn(Progress),
    /// Tells the runner the engine has answered.
    answered: &'a dyn Fn(),
    /// Tells the runner why the engine is down now, or that it's back.
    engine_down: &'a dyn Fn(Option<&str>),
    /// Whether it has been told yet: once is enough. Also whether the
    /// engine itself confirmed any audio this run.
    told: bool,
    /// Whether any audio was skipped, the engines having kept failing on
    /// it.
    lost: bool,
}

impl<'a> Heard<'a> {
    /// The sink for `work` in `session`, storing each row at `boost`.
    fn new(
        db: Writer,
        session: SessionId,
        boost: Boost,
        work: &Work,
        running: &'a Running<'a>,
    ) -> Self {
        let pending = work
            .chunks
            .iter()
            .map(|(&track, chunks)| {
                let chunks = chunks.iter().map(|chunk| Pending {
                    chunk: chunk.clone(),
                    texts: Vec::new(),
                    lost: false,
                });
                (track, chunks.collect())
            })
            .collect();
        let spoken = work
            .spoken
            .iter()
            .map(|(&track, ranges)| {
                let from = work.from.get(&track).copied().unwrap_or(SampleIndex::ZERO);
                (track, (ranges.clone(), from))
            })
            .collect();
        Self {
            db,
            session,
            boost,
            pending,
            spoken,
            report: running.progress,
            answered: running.answered,
            engine_down: running.engine_down,
            told: false,
            lost: false,
        }
    }

    /// The samples of the chunks with text done, of all of them.
    fn progress(&self) -> Progress {
        let mut progress = Progress::default();
        for (ranges, up_to) in self.spoken.values() {
            for range in ranges {
                let len = range.len().get();
                progress.total = progress.total.saturating_add(len);
                let done = up_to.saturating_count_since(range.start()).get().min(len);
                progress.done = progress.done.saturating_add(done);
            }
        }
        progress
    }

    /// Notes `texts` and `skipped` against the chunks of `track` they fall
    /// in. A text belongs to the chunk its first sample is in.
    fn note(&mut self, track: TrackId, texts: Vec<Transcript>, skipped: &[SampleRange]) {
        let Some(pending) = self.pending.get_mut(&track) else {
            return;
        };
        for text in texts {
            let start = text.range().start();
            if let Some(chunk) = pending.iter_mut().find(|p| p.chunk.range.contains(start)) {
                chunk.texts.push(text.into_text());
            }
        }
        for range in skipped {
            let overlaps = |p: &&mut Pending| {
                p.chunk.range.start() < range.end() && range.start() < p.chunk.range.end()
            };
            for chunk in pending.iter_mut().filter(overlaps) {
                chunk.lost = true;
            }
        }
    }

    /// Stores the first `n` chunks still pending on `track`, with the
    /// pass's progress there at the end of the last of them, and takes
    /// them off the list once stored.
    fn commit(&mut self, track: TrackId, n: usize) -> Result<(), StoreError> {
        let Some(pending) = self.pending.get_mut(&track) else {
            return Ok(());
        };
        let rows: Vec<HotwordText> = pending
            .iter()
            .take(n)
            .map(|p| p.row(track, self.boost))
            .collect();
        let Some(up_to) = rows.last().map(|row| row.range.end()) else {
            return Ok(());
        };
        self.db
            .with(|db| db.add_hotword_text(self.session, track, up_to, &rows))?;
        pending.drain(..rows.len());
        Ok(())
    }

    /// Stores every chunk left: those the engine wasn't fed, and any whose
    /// audio it never reached (a chunk with no segments to read).
    fn finish(&mut self) -> Result<(), StoreError> {
        let tracks: Vec<(TrackId, usize)> = self
            .pending
            .iter()
            .map(|(&track, pending)| (track, pending.len()))
            .collect();
        for (track, n) in tracks {
            self.commit(track, n)?;
        }
        Ok(())
    }
}

impl PassSink for Heard<'_> {
    type Error = StoreError;

    fn confirmed(
        &mut self,
        track: TrackId,
        up_to: SampleIndex,
        texts: Vec<Transcript>,
        skipped: Vec<SampleRange>,
    ) -> Result<(), StoreError> {
        // With nothing skipped, the engine itself confirmed this audio:
        // it works, whatever stopped an earlier run.
        let engine_answered = skipped.is_empty();
        self.lost |= !engine_answered;
        self.note(track, texts, &skipped);
        // Only chunks wholly confirmed are stored, so the progress stored
        // is always a chunk's end: a run that stops mid-chunk decodes that
        // chunk again, whole, next time.
        let whole = self.pending.get(&track).map_or(0, |pending| {
            pending
                .iter()
                .take_while(|p| p.chunk.range.end() <= up_to)
                .count()
        });
        self.commit(track, whole)?;
        if let Some((_, done)) = self.spoken.get_mut(&track) {
            *done = (*done).max(up_to);
        }
        (self.report)(self.progress());
        if engine_answered && !self.told {
            (self.answered)();
            self.told = true;
        }
        Ok(())
    }

    fn engine_down(&mut self, why: Option<&str>) {
        (self.engine_down)(why);
    }
}
