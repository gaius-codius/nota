use std::cell::RefCell;
use std::path::PathBuf;

use nota_core::{EpochId, SampleRate, SystemClock};
use nota_store::{FinalWord, HeardBy, NewSession, Pack, Proposal, Sha256Digest};

use super::*;
use crate::cleanup::fake::{Fake, FakeSources, Gives};
use crate::pack::{GlossaryAbout, glossary};

/// A fresh directory under the system temp dir, removed when dropped.
struct TestDir(PathBuf);

impl TestDir {
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding outside the recorder's write path"
    )]
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("nota-hotword-pass-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TestDir {
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding outside the recorder's write path"
    )]
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const SESSION: SessionId = SessionId::new(1);
const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);

fn s(n: u64) -> SampleIndex {
    SampleIndex::new(n)
}

fn range(a: u64, b: u64) -> SampleRange {
    SampleRange::new(s(a), s(b)).unwrap()
}

fn row(track: TrackId, a: u64, b: u64) -> SegmentRow {
    SegmentRow::new(
        track,
        EpochId::new(0),
        range(a, b),
        Sha256Digest::new([0; 32]),
    )
    .unwrap()
}

/// The final pass's chunk `a..b` on `track`, with `text`'s words (split at
/// spaces) spread over it, or none if it heard nothing.
fn final_text(track: TrackId, a: u64, b: u64, text: Option<&str>) -> FinalText {
    let words: Vec<&str> = text.map(|t| t.split(' ').collect()).unwrap_or_default();
    let step = (b - a) / u64::try_from(words.len().max(1)).unwrap();
    FinalText {
        track,
        range: range(a, b),
        text: text.map(str::to_owned),
        words: (0_u64..)
            .zip(&words)
            .map(|(i, w)| FinalWord {
                text: (*w).to_owned(),
                range: range(a + i * step, a + (i + 1) * step),
                log_prob: None,
            })
            .collect(),
        heard_by: HeardBy {
            engine: "sherpa-onnx".to_owned(),
            model: "parakeet".to_owned(),
        },
    }
}

/// A chunk of the microphone's final text: its samples and what the final
/// pass heard there.
type Line = (u64, u64, Option<&'static str>);

/// A library whose session has the microphone's final text `lines` (each
/// on a segment of its own), its final pass done, and the glossary pack of
/// `terms` recorded as its pack (none without terms).
struct Fixture {
    /// Removed at the end.
    _dir: TestDir,
    /// The library.
    library: Library,
    /// The session's pack, installed.
    pack: Option<Pack>,
}

impl Fixture {
    fn new(name: &str, lines: &[Line], terms: &[&str]) -> Self {
        let dir = TestDir::new(name);
        let library = Library::open(&dir.0).unwrap();
        let pack = (!terms.is_empty()).then(|| {
            glossary(
                &terms.join("\n"),
                &GlossaryAbout {
                    name: "terms".to_owned(),
                    version: "1".to_owned(),
                    language: "en".to_owned(),
                    author: "a tester".to_owned(),
                },
            )
            .unwrap()
        });
        library
            .db()
            .with(|db| {
                db.create_session(&NewSession::bare(SESSION))?;
                if let Some(pack) = &pack {
                    db.record_session_packs(SESSION, &[pack.pack_ref()])?;
                }
                db.finish_recording(SESSION, None)?;
                for &(a, b, text) in lines {
                    db.insert_segment(SESSION, &row(MIC, a, b))?;
                    let line = final_text(MIC, a, b, text);
                    db.add_final_text(SESSION, MIC, s(b), &[line])?;
                }
                Ok(())
            })
            .unwrap();
        let fixture = Self {
            _dir: dir,
            library,
            pack,
        };
        fixture.end(JobKind::FinalPass, &JobEnd::Done);
        fixture
    }

    fn db(&self) -> &Writer {
        self.library.db()
    }

    /// Ends the session's job of `kind` as `end`.
    fn end(&self, kind: JobKind, end: &JobEnd) {
        let jobs = self.db().with(|db| db.session_jobs(SESSION)).unwrap();
        let id = jobs.into_iter().find(|j| j.kind == kind).unwrap().id;
        self.db().with(|db| db.end_job(id, end)).unwrap();
    }

    /// Clean-up with `gives` as its decider and the session's pack
    /// installed.
    fn clean_up(&self, gives: Gives) -> CleanUp {
        let sources = FakeSources {
            gives,
            packs: self.pack.iter().cloned().collect(),
        };
        CleanUp::new(Box::new(sources), SampleRate::SPEECH)
    }

    /// The session's hotword texts, as samples and text.
    fn texts(&self) -> Vec<(TrackId, u64, u64, Option<String>)> {
        self.db()
            .with(|db| db.hotword_texts(SESSION))
            .unwrap()
            .into_iter()
            .map(|t| (t.track, t.range.start().get(), t.range.end().get(), t.text))
            .collect()
    }

    /// How far the stored hotword pass has got on each track.
    fn progress(&self) -> BTreeMap<TrackId, SampleIndex> {
        self.db().with(|db| db.hotword_progress(SESSION)).unwrap()
    }

    /// The work a run would do now.
    fn work(&self) -> Work {
        let (rows, finals, from) = self
            .db()
            .with(|db| {
                Ok((
                    db.segments(SESSION)?,
                    db.final_texts(SESSION)?,
                    db.hotword_progress(SESSION)?,
                ))
            })
            .unwrap();
        Work::new(rows, &finals, &from)
    }
}

/// A runner's view that never stops, noting nothing.
fn quiet<'a>() -> Running<'a> {
    Running {
        stop: &|| false,
        progress: &|_| {},
        answered: &|| {},
        engine_down: &|_| {},
    }
}

/// What the engine heard over `a..b` of the microphone.
fn heard(a: u64, b: u64, text: &str) -> Transcript {
    Transcript::new(MIC, range(a, b), text.to_owned()).unwrap()
}

/// An engine command whose program isn't there: a run that started it
/// would end with the engine down.
fn missing_engine() -> HotwordEngine {
    HotwordEngine::Stand(EngineCommand {
        program: PathBuf::from("/nonexistent/nota-hotword-engine"),
        args: Vec::new(),
    })
}

/// Runs the job over the fixture's session with `engine`, `boost` and
/// `stop`.
fn run_job(
    fixture: &Fixture,
    clean_up: &CleanUp,
    engine: Option<&HotwordEngine>,
    boost: &dyn Fn() -> Result<Boost, String>,
    stop: &dyn Fn() -> bool,
) -> JobEnd {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::start().unwrap());
    let pass = Pass {
        library: &fixture.library,
        engine,
        clock: &clock,
        clean_up,
        boost,
    };
    let running = Running {
        stop,
        progress: &|_| {},
        answered: &|| {},
        engine_down: &|_| {},
    };
    run(&pass, SESSION, &running)
}

/// What a job's `lacks` says for the fixture's session with `engine`.
fn lacks_with(
    fixture: &Fixture,
    clean_up: &CleanUp,
    engine: Option<&HotwordEngine>,
) -> Option<Wait> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::start().unwrap());
    let boost = || Ok(Boost::DEFAULT);
    let pass = Pass {
        library: &fixture.library,
        engine,
        clock: &clock,
        clean_up,
        boost: &boost,
    };
    lacks(&pass, SESSION)
}

/// A boost source that always gives the default.
const DEFAULT_BOOST: &dyn Fn() -> Result<Boost, String> = &|| Ok(Boost::DEFAULT);

/// Each chunk with text the final pass heard is fed on its own, bounded by
/// its end, with its track's segments; chunks done already, and chunks
/// with no text, aren't fed, though a text-less one still to store is
/// listed.
#[test]
fn each_text_chunk_left_is_fed_on_its_own() {
    let finals = [
        final_text(MIC, 200, 300, Some("c")),
        final_text(MIC, 0, 100, Some("a")),
        final_text(MIC, 100, 200, None),
        final_text(SYSTEM, 0, 50, Some("d")),
    ];
    let rows = vec![row(MIC, 0, 300), row(SYSTEM, 0, 50)];
    let work = Work::new(rows, &finals, &BTreeMap::from([(MIC, s(100))]));
    assert_eq!(
        work.feeds,
        [
            TrackAudio {
                track: MIC,
                segments: vec![row(MIC, 0, 300)],
                from: s(200),
                until: Some(s(300)),
            },
            TrackAudio {
                track: SYSTEM,
                segments: vec![row(SYSTEM, 0, 50)],
                from: s(0),
                until: Some(s(50)),
            },
        ]
    );
    let chunk = |a, b, fed| Chunk {
        range: range(a, b),
        fed,
    };
    assert_eq!(
        work.chunks,
        BTreeMap::from([
            (MIC, vec![chunk(100, 200, false), chunk(200, 300, true)]),
            (SYSTEM, vec![chunk(0, 50, true)]),
        ])
    );
    // Progress counts every chunk with text, done or not.
    assert_eq!(
        work.spoken,
        BTreeMap::from([
            (MIC, vec![range(0, 100), range(200, 300)]),
            (SYSTEM, vec![range(0, 50)]),
        ])
    );
}

/// What the engine confirms is stored chunk by chunk, a chunk's texts
/// joined by spaces, with the boost and the progress at the chunk's end,
/// and the job's progress counts the samples of the chunks with text.
#[test]
fn confirmed_text_is_stored_per_chunk_with_its_progress() {
    let fixture = Fixture::new(
        "stored",
        &[(0, 100, Some("a b")), (100, 200, Some("c"))],
        &[],
    );
    let work = fixture.work();
    let reported = RefCell::new(Vec::new());
    let report = |p: Progress| reported.borrow_mut().push((p.done, p.total));
    let running = Running {
        stop: &|| false,
        progress: &report,
        answered: &|| {},
        engine_down: &|_| {},
    };
    let boost = Boost::new(2.5).unwrap();
    let mut sink = Heard::new(fixture.db().clone(), SESSION, boost, &work, &running);
    sink.confirmed(
        MIC,
        s(100),
        vec![heard(0, 40, "alpha"), heard(40, 90, "beta")],
        vec![],
    )
    .unwrap();
    assert_eq!(
        fixture.texts(),
        [(MIC, 0, 100, Some("alpha beta".to_owned()))]
    );
    assert_eq!(fixture.progress()[&MIC], s(100));
    let stored = fixture.db().with(|db| db.hotword_texts(SESSION)).unwrap();
    assert_eq!(stored[0].boost.to_bits(), 2.5_f64.to_bits());
    assert_eq!(*reported.borrow(), [(100, 200)]);
}

/// A confirmation partway through a chunk stores nothing past the last
/// whole chunk: the progress stored is that chunk's end, and the text
/// heard so far in the next is kept until it's whole.
#[test]
fn a_confirmation_mid_chunk_commits_nothing_past_the_last_whole_chunk() {
    let fixture = Fixture::new(
        "mid-chunk",
        &[(0, 100, Some("a")), (100, 200, Some("c"))],
        &[],
    );
    let work = fixture.work();
    let running = quiet();
    let mut sink = Heard::new(
        fixture.db().clone(),
        SESSION,
        Boost::DEFAULT,
        &work,
        &running,
    );
    sink.confirmed(
        MIC,
        s(150),
        vec![heard(0, 90, "one"), heard(100, 140, "two")],
        vec![],
    )
    .unwrap();
    assert_eq!(fixture.texts(), [(MIC, 0, 100, Some("one".to_owned()))]);
    assert_eq!(fixture.progress()[&MIC], s(100));
    sink.confirmed(MIC, s(200), vec![heard(150, 190, "three")], vec![])
        .unwrap();
    assert_eq!(
        fixture.texts(),
        [
            (MIC, 0, 100, Some("one".to_owned())),
            (MIC, 100, 200, Some("two three".to_owned())),
        ]
    );
    assert_eq!(fixture.progress()[&MIC], s(200));
}

/// A chunk whose audio the engine skipped, and one it heard nothing in,
/// are stored without text.
#[test]
fn skipped_or_silent_chunks_store_no_text() {
    let fixture = Fixture::new(
        "skipped",
        &[(0, 100, Some("a")), (100, 200, Some("c"))],
        &[],
    );
    let work = fixture.work();
    let running = quiet();
    let mut sink = Heard::new(
        fixture.db().clone(),
        SESSION,
        Boost::DEFAULT,
        &work,
        &running,
    );
    sink.confirmed(MIC, s(50), vec![heard(0, 40, "lost")], vec![])
        .unwrap();
    sink.confirmed(MIC, s(100), vec![], vec![range(50, 100)])
        .unwrap();
    sink.confirmed(MIC, s(200), vec![], vec![]).unwrap();
    assert_eq!(
        fixture.texts(),
        [(MIC, 0, 100, None), (MIC, 100, 200, None)]
    );
}

/// A run whose engines skipped audio and never confirmed any themselves
/// (an engine that can't be boosted dies at every chunk) fails rather than
/// reading as done; one that skipped some but heard the rest is done.
#[test]
fn a_run_that_heard_nothing_but_skips_fails() {
    let fixture = Fixture::new(
        "all-skipped",
        &[(0, 100, Some("a")), (100, 200, Some("c"))],
        &[],
    );
    let work = fixture.work();
    let running = quiet();
    let mut sink = Heard::new(
        fixture.db().clone(),
        SESSION,
        Boost::DEFAULT,
        &work,
        &running,
    );
    sink.confirmed(MIC, s(100), vec![], vec![range(0, 100)])
        .unwrap();
    sink.confirmed(MIC, s(200), vec![], vec![range(100, 200)])
        .unwrap();
    assert!(
        matches!(finished(&mut sink), JobEnd::Failed(ref why) if why.contains("none of the audio")),
    );
    // What it skipped is stored, so a later run doesn't feed it again.
    assert_eq!(
        fixture.texts(),
        [(MIC, 0, 100, None), (MIC, 100, 200, None)]
    );
    let fixture = Fixture::new(
        "some-skipped",
        &[(0, 100, Some("a")), (100, 200, Some("c"))],
        &[],
    );
    let work = fixture.work();
    let mut sink = Heard::new(
        fixture.db().clone(),
        SESSION,
        Boost::DEFAULT,
        &work,
        &running,
    );
    sink.confirmed(MIC, s(100), vec![], vec![range(0, 100)])
        .unwrap();
    sink.confirmed(MIC, s(200), vec![heard(100, 190, "c")], vec![])
        .unwrap();
    assert_eq!(finished(&mut sink), JobEnd::Done);
}

/// Chunks the final pass heard nothing in aren't fed: one before a fed
/// chunk is stored when a confirmation passes it, and one after the last
/// when the run finishes.
#[test]
fn text_less_chunks_are_stored_without_being_fed() {
    let fixture = Fixture::new(
        "text-less",
        &[(0, 100, None), (100, 200, Some("b")), (200, 300, None)],
        &[],
    );
    let work = fixture.work();
    assert_eq!(
        work.feeds
            .iter()
            .map(|f| (f.from, f.until))
            .collect::<Vec<_>>(),
        [(s(100), Some(s(200)))]
    );
    let running = quiet();
    let mut sink = Heard::new(
        fixture.db().clone(),
        SESSION,
        Boost::DEFAULT,
        &work,
        &running,
    );
    sink.confirmed(MIC, s(200), vec![heard(100, 180, "bee")], vec![])
        .unwrap();
    assert_eq!(
        fixture.texts(),
        [(MIC, 0, 100, None), (MIC, 100, 200, Some("bee".to_owned()))]
    );
    sink.finish().unwrap();
    assert_eq!(fixture.texts().last(), Some(&(MIC, 200, 300, None)));
    assert_eq!(fixture.progress()[&MIC], s(300));
}

/// A run with progress stored starts after it: the chunks it got past
/// aren't fed or listed again.
#[test]
fn a_run_starts_after_its_stored_progress() {
    let fixture = Fixture::new(
        "progress",
        &[(0, 100, Some("a")), (100, 200, None), (200, 300, Some("c"))],
        &[],
    );
    let work = fixture.work();
    let running = quiet();
    let mut sink = Heard::new(
        fixture.db().clone(),
        SESSION,
        Boost::DEFAULT,
        &work,
        &running,
    );
    sink.confirmed(MIC, s(100), vec![heard(0, 90, "a")], vec![])
        .unwrap();
    let again = fixture.work();
    assert_eq!(
        again
            .feeds
            .iter()
            .map(|f| (f.from, f.until))
            .collect::<Vec<_>>(),
        [(s(200), Some(s(300)))]
    );
    assert_eq!(
        again.chunks[&MIC]
            .iter()
            .map(|c| c.range)
            .collect::<Vec<_>>(),
        [range(100, 200), range(200, 300)]
    );
}

/// The runner hears once that the engine answered, at its first
/// confirmation of audio it didn't skip, and hears why it's down.
#[test]
fn the_runner_hears_the_engine_answer_once_and_why_its_down() {
    let fixture = Fixture::new(
        "answered",
        &[(0, 100, Some("a")), (100, 200, Some("b"))],
        &[],
    );
    let work = fixture.work();
    let told = RefCell::new(0);
    let answered = || *told.borrow_mut() += 1;
    let downs = RefCell::new(Vec::new());
    let engine_down = |why: Option<&str>| downs.borrow_mut().push(why.map(str::to_owned));
    let running = Running {
        stop: &|| false,
        progress: &|_| {},
        answered: &answered,
        engine_down: &engine_down,
    };
    let mut sink = Heard::new(
        fixture.db().clone(),
        SESSION,
        Boost::DEFAULT,
        &work,
        &running,
    );
    sink.confirmed(MIC, s(100), vec![], vec![range(0, 100)])
        .unwrap();
    assert_eq!(*told.borrow(), 0);
    sink.engine_down(Some("gone"));
    sink.confirmed(MIC, s(150), vec![], vec![]).unwrap();
    sink.confirmed(MIC, s(200), vec![], vec![]).unwrap();
    assert_eq!(*told.borrow(), 1);
    assert_eq!(*downs.borrow(), [Some("gone".to_owned())]);
}

/// With no terms in its packs there's nothing to boost: the job is done
/// without starting an engine (this one's program isn't there), storing
/// nothing, and doesn't wait for an engine first.
#[test]
fn no_terms_is_done_without_an_engine() {
    let fixture = Fixture::new("no-terms", &[(0, 100, Some("a"))], &[]);
    let clean_up = fixture.clean_up(Gives::NotSetUp);
    assert_eq!(lacks_with(&fixture, &clean_up, None), None);
    let engine = missing_engine();
    assert_eq!(
        run_job(&fixture, &clean_up, Some(&engine), DEFAULT_BOOST, &|| {
            false
        }),
        JobEnd::Done
    );
    assert_eq!(fixture.texts(), []);
}

/// The job waits for its session's final pass, then (with terms to boost)
/// for an engine.
#[test]
fn the_job_waits_for_the_final_text_then_an_engine() {
    let fixture = Fixture::new("lacks", &[(0, 100, Some("a"))], &["amlodipine"]);
    let clean_up = fixture.clean_up(Gives::NotSetUp);
    let engine = missing_engine();
    fixture.end(JobKind::FinalPass, &JobEnd::Waiting(Some(Wait::Engine)));
    assert_eq!(
        lacks_with(&fixture, &clean_up, Some(&engine)),
        Some(Wait::FinalText)
    );
    assert_eq!(
        run_job(&fixture, &clean_up, Some(&engine), DEFAULT_BOOST, &|| {
            false
        }),
        JobEnd::Waiting(Some(Wait::FinalText))
    );
    fixture.end(JobKind::FinalPass, &JobEnd::Done);
    assert_eq!(lacks_with(&fixture, &clean_up, None), Some(Wait::Engine));
    assert_eq!(lacks_with(&fixture, &clean_up, Some(&engine)), None);
    assert_eq!(
        run_job(&fixture, &clean_up, None, DEFAULT_BOOST, &|| false),
        JobEnd::Waiting(Some(Wait::Engine))
    );
}

/// A failed final pass fails the job, saying why.
#[test]
fn a_failed_final_pass_fails_the_job() {
    let fixture = Fixture::new("failed", &[(0, 100, Some("a"))], &["amlodipine"]);
    let clean_up = fixture.clean_up(Gives::NotSetUp);
    fixture.end(JobKind::FinalPass, &JobEnd::Failed("it broke".to_owned()));
    assert_eq!(lacks_with(&fixture, &clean_up, None), None);
    assert_eq!(
        run_job(&fixture, &clean_up, None, DEFAULT_BOOST, &|| false),
        JobEnd::Failed("there's no final transcript to boost: it broke".to_owned())
    );
}

/// A boost the settings can't give fails the job with the reason, which
/// names the setting.
#[test]
fn a_bad_boost_fails_the_job() {
    let fixture = Fixture::new("bad-boost", &[(0, 100, Some("a"))], &["amlodipine"]);
    let clean_up = fixture.clean_up(Gives::NotSetUp);
    let engine = missing_engine();
    let why = "hotword_pass.boost isn't a number";
    let boost = || Err(why.to_owned());
    assert_eq!(
        run_job(&fixture, &clean_up, Some(&engine), &boost, &|| false),
        JobEnd::Failed(why.to_owned())
    );
}

/// Told to stop (a recording has started), the job waits to run again,
/// having stored nothing.
#[test]
fn a_stop_leaves_the_job_waiting() {
    let fixture = Fixture::new("stop", &[(0, 100, Some("a"))], &["amlodipine"]);
    let clean_up = fixture.clean_up(Gives::NotSetUp);
    let engine = missing_engine();
    assert_eq!(
        run_job(&fixture, &clean_up, Some(&engine), DEFAULT_BOOST, &|| true),
        JobEnd::Waiting(None)
    );
    assert_eq!(fixture.texts(), []);
}

/// A session whose only chunks heard nothing has nothing to feed: the
/// job waits for no engine, its chunks are stored text-less and it's done
/// without one.
#[test]
fn only_text_less_chunks_are_done_without_an_engine() {
    let fixture = Fixture::new("all-silent", &[(0, 100, None)], &["amlodipine"]);
    let clean_up = fixture.clean_up(Gives::NotSetUp);
    fixture.end(JobKind::FinalPass, &JobEnd::Done);
    assert_eq!(lacks_with(&fixture, &clean_up, None), None);
    assert_eq!(
        run_job(&fixture, &clean_up, None, DEFAULT_BOOST, &|| false),
        JobEnd::Done
    );
    assert_eq!(fixture.texts(), [(MIC, 0, 100, None)]);
}

/// The real engine's command gets the boost and vocabulary file; the
/// stand-in's is left as it is.
#[test]
fn the_real_engine_is_given_its_boost_and_vocabulary() {
    let command = EngineCommand {
        program: PathBuf::from("nota"),
        args: vec!["engine".into(), "asr".into()],
    };
    let vocab = Path::new("/s/1/hotword-vocab.txt");
    let boost = Boost::new(2.5).unwrap();
    let args = |engine: &HotwordEngine| engine.command(boost, vocab).args;
    assert_eq!(
        args(&HotwordEngine::Asr(command.clone())),
        [
            "engine",
            "asr",
            "--boost",
            "2.5",
            "--vocab",
            "/s/1/hotword-vocab.txt"
        ]
    );
    assert_eq!(args(&HotwordEngine::Stand(command)), ["engine", "asr"]);
}

/// A boost is above 0 and at most 10, and never so small the engine would
/// read it as 0.
#[test]
fn a_boost_is_above_zero_and_at_most_ten() {
    let taken: Vec<bool> = [
        0.0,
        -1.0,
        0.1,
        10.0,
        10.5,
        f64::NAN,
        f64::INFINITY,
        1e-100,
        f64::from(f32::MIN_POSITIVE),
    ]
    .into_iter()
    .map(|score| Boost::new(score).is_some())
    .collect();
    assert_eq!(
        taken,
        [false, false, true, true, false, false, false, false, true]
    );
}

/// The session for the resume test: two chunks with text around one with
/// none.
const RESUMED: [Line; 3] = [
    (0, 480_000, Some("start the stat ins")),
    (480_000, 960_000, None),
    (960_000, 1_440_000, Some("then the stat ins again")),
];

/// What the beam hears in the chunk starting at `start`.
fn beam(start: SampleIndex) -> String {
    match start.get() {
        0 => "start the statins".to_owned(),
        _ => "then the statins again".to_owned(),
    }
}

/// Plays an engine over `work`'s feeds as the pass sends them: for each,
/// a confirmation partway with nothing heard yet, then one at its end
/// with the beam's text. Stops partway through the feed after the first
/// `cut` if given (as a recording would); otherwise finishes the run.
fn play(fixture: &Fixture, work: &Work, cut: Option<usize>) {
    let running = quiet();
    let mut sink = Heard::new(
        fixture.db().clone(),
        SESSION,
        Boost::DEFAULT,
        work,
        &running,
    );
    for (i, feed) in work.feeds.iter().enumerate() {
        let until = feed.until.unwrap();
        let mid = s(u64::midpoint(feed.from.get(), until.get()));
        sink.confirmed(feed.track, mid, vec![], vec![]).unwrap();
        if cut == Some(i) {
            return;
        }
        let text = Transcript::new(
            feed.track,
            SampleRange::new(feed.from, until).unwrap(),
            beam(feed.from),
        )
        .unwrap();
        sink.confirmed(feed.track, until, vec![text], vec![])
            .unwrap();
    }
    sink.finish().unwrap();
}

/// Each proposal without its number, to compare two sessions.
fn unnumbered(proposals: &[Proposal]) -> Vec<(nota_store::Proposed, nota_store::Decided)> {
    proposals
        .iter()
        .map(|p| (p.proposed.clone(), p.decided.clone()))
        .collect()
}

/// Clean-up's proposals over the fixture's session, its hotword pass done.
fn clean_up_proposals(fixture: &Fixture) -> Vec<Proposal> {
    fixture.end(JobKind::HotwordPass, &JobEnd::Done);
    let fake = Fake::saying_yes([("stat ins", "statins")]);
    let mut clean_up = fixture.clean_up(Gives::Fake(fake));
    assert_eq!(clean_up.run(fixture.db(), SESSION, &quiet()), JobEnd::Done);
    fixture.db().with(|db| db.proposals(SESSION)).unwrap()
}

/// Acceptance: a run stopped after its first committed chunk, then run
/// again from what it stored, stores exactly what a run never stopped
/// stores, and clean-up over each gives the same proposals.
#[test]
fn a_resumed_run_stores_what_an_unbroken_one_does() {
    let terms = ["statins"];
    let whole = Fixture::new("resume-whole", &RESUMED, &terms);
    play(&whole, &whole.work(), None);
    let want = whole.texts();
    assert_eq!(
        want,
        [
            (MIC, 0, 480_000, Some(beam(s(0)))),
            (MIC, 480_000, 960_000, None),
            (MIC, 960_000, 1_440_000, Some(beam(s(960_000)))),
        ]
    );

    let cut = Fixture::new("resume-cut", &RESUMED, &terms);
    // Stopped partway through the second feed: the first chunk is
    // stored, and the text-less one the confirmation got past.
    play(&cut, &cut.work(), Some(1));
    assert_eq!(cut.texts(), want[..2]);
    assert_eq!(cut.progress()[&MIC], s(960_000));
    // The next run carries on from the stored progress.
    play(&cut, &cut.work(), None);
    assert_eq!(cut.texts(), want);
    assert_eq!(cut.progress(), whole.progress());

    let proposals = clean_up_proposals(&whole);
    // Only the diff finds these, so the proposals rest on the stored text.
    let found: Vec<nota_store::Found> = proposals.iter().map(|p| p.proposed.found).collect();
    assert_eq!(found, [nota_store::Found::Hotword; 2]);
    assert_eq!(
        unnumbered(&clean_up_proposals(&cut)),
        unnumbered(&proposals)
    );
}
