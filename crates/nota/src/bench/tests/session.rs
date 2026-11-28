//! A session's candidates, read from a library without changing it.

use nota_core::{SampleRange, SampleRate, SessionId};
use nota_store::{
    FinalText, FinalWord, HeardBy, JobEnd, JobKind, NewSession, Pack, Proposal, RevisionNumber,
    Writer,
};

use super::*;
use crate::bench::from_session;
use crate::cleanup::fake::{FakeSources, Gives};
use crate::cleanup::sources::PACKS;
use crate::cleanup::{CleanUp, Saved};
use crate::jobs::Running;
use crate::library::LIBRARY_DB;
use crate::pack::{GlossaryAbout, glossary};

/// The session the tests bench.
const SESSION: SessionId = SessionId::new(1);

/// Each revision of a session, with its text chunk by chunk.
type Revisions = Vec<(RevisionNumber, Vec<Option<String>>)>;

/// A data directory whose library has [`SESSION`], recorded with the
/// glossary pack of [`TERMS`], which is installed in its packs folder.
struct Library {
    /// The data directory, removed at the end.
    dir: TestDir,
    /// The library.
    db: Writer,
    /// The session's pack.
    pack: Pack,
}

impl Library {
    /// The library, its session with `chunks` of final text (one per
    /// 30 s, words split at spaces) and its final pass done.
    fn new(name: &str, chunks: &[&str]) -> Self {
        let dir = TestDir::new(name);
        let db = Writer::new(&dir.0.join(LIBRARY_DB));
        let pack = glossary(
            &TERMS.join("\n"),
            &GlossaryAbout {
                name: "terms".to_owned(),
                version: "1".to_owned(),
                language: "en".to_owned(),
                author: "a tester".to_owned(),
            },
        )
        .unwrap();
        install(&dir.0, &pack);
        db.with(|db| {
            db.create_session(&NewSession::bare(SESSION))?;
            db.record_session_packs(SESSION, &[pack.pack_ref()])?;
            db.finish_recording(SESSION, None)?;
            for (i, text) in (0_u64..).zip(chunks) {
                let line = final_text(i * CHUNK, text);
                db.add_final_text(SESSION, MIC, line.range.end(), &[line])?;
            }
            Ok(())
        })
        .unwrap();
        let library = Self { dir, db, pack };
        library.end_passes();
        library
    }

    /// Ends the session's final pass and hotword pass as done, the
    /// hotword pass having heard nothing.
    fn end_passes(&self) {
        let jobs = self.db.with(|db| db.session_jobs(SESSION)).unwrap();
        for job in jobs
            .into_iter()
            .filter(|j| matches!(j.kind, JobKind::FinalPass | JobKind::HotwordPass))
        {
            self.db
                .with(|db| db.end_job(job.id, &JobEnd::Done))
                .unwrap();
        }
    }

    /// Runs a clean-up job of the session with `fake` as its decider.
    fn clean_up(&self, fake: &Fake) -> JobEnd {
        let sources = FakeSources {
            gives: Gives::Fake(fake.clone()),
            packs: vec![self.pack.clone()],
            precheck: Precheck::Off,
        };
        let mut clean_up = CleanUp::new(Box::new(sources), SampleRate::SPEECH);
        let running = Running {
            stop: &|| false,
            progress: &|_| {},
            answered: &|| {},
            engine_down: &|_| {},
        };
        clean_up.run(&self.db, SESSION, &running)
    }

    /// The session's proposals, and each of its revisions with its text.
    fn stored(&self) -> (Vec<Proposal>, Revisions) {
        self.db
            .with(|db| {
                let revisions = db
                    .revisions(SESSION)?
                    .into_iter()
                    .map(|n| {
                        let text = db.revision(SESSION, n)?.into_iter().map(|l| l.text);
                        Ok((n, text.collect()))
                    })
                    .collect::<Result<_, nota_store::StoreError>>()?;
                Ok((db.proposals(SESSION)?, revisions))
            })
            .unwrap()
    }
}

/// Installs `pack` in the data directory `data`'s packs folder, as
/// `nota pack` would leave it.
#[expect(
    clippy::disallowed_methods,
    reason = "test scaffolding outside the recorder's write path"
)]
fn install(data: &Path, pack: &Pack) {
    let packs = data.join(PACKS);
    std::fs::create_dir_all(&packs).unwrap();
    pack.write(&packs.join("terms.pack")).unwrap();
}

/// The final text at sample `start`: `text`'s words spread evenly over
/// [`CHUNK`].
fn final_text(start: u64, text: &str) -> FinalText {
    let words: Vec<&str> = text.split(' ').collect();
    let step = CHUNK / u64::try_from(words.len()).unwrap();
    let range =
        |a: u64, b: u64| SampleRange::new(SampleIndex::new(a), SampleIndex::new(b)).unwrap();
    FinalText {
        track: MIC,
        range: range(start, start + CHUNK),
        text: Some(text.to_owned()),
        words: (0_u64..)
            .zip(&words)
            .map(|(i, w)| FinalWord {
                text: (*w).to_owned(),
                range: range(start + i * step, start + (i + 1) * step),
                log_prob: None,
            })
            .collect(),
        heard_by: HeardBy {
            engine: "sherpa-onnx".to_owned(),
            model: "parakeet".to_owned(),
        },
    }
}

/// Acceptance (#164): the session's proposals and revisions are unchanged
/// after a bench run, even with an engine whose answers would change
/// them.
#[test]
fn a_bench_run_leaves_the_sessions_proposals_and_revisions_unchanged() {
    let library = Library::new("unchanged", &CHUNKS);
    let cleaned = library.clean_up(&Fake::saying_yes([("amylo diping", "amlodipine")]));
    assert_eq!(cleaned, JobEnd::Done);
    let before = library.stored();
    // The clean-up run proposed and applied something, so an unchanged
    // session isn't merely an empty one.
    assert_eq!((before.0.len(), before.1.len()), (2, 2), "{before:?}");
    let candidates =
        from_session(&library.dir.0, SESSION, Precheck::Off, Arc::new(clock())).unwrap();
    // This engine says yes where the clean-up run's said no: anything it
    // wrote would show.
    let yes = Fake::saying_yes([("amylo diping", "amlodipine"), ("profen", "ibuprofen")]);
    let runs = run(&candidates, vec![engine("yes", &yes)], &clock(), &|| false);
    assert_eq!(
        runs[0].verdicts.values().copied().collect::<Vec<_>>(),
        [
            verdict(Said::Yes, Ending::Applied),
            verdict(Said::Yes, Ending::Applied)
        ]
    );
    assert_eq!(library.stored(), before);
}

/// A session's candidates are every one the lookup offers, including
/// those its clean-up runs decided, and carry what C3 saved them.
#[test]
fn a_sessions_candidates_include_those_already_decided() {
    let library = Library::new("decided", &CHUNKS);
    library.clean_up(&Fake::saying_yes([]));
    let candidates =
        from_session(&library.dir.0, SESSION, Precheck::On, Arc::new(clock())).unwrap();
    let offered: Vec<Key> = candidates
        .offering
        .iter()
        .flat_map(|(chunk, offers)| offers.iter().map(|o| Key::of(chunk, o)))
        .collect();
    assert_eq!(
        offered,
        [
            key(0, 1, 2, "amylo diping", "amlodipine"),
            key(1, 2, 2, "profen", "ibuprofen"),
        ]
    );
    assert_eq!(
        (candidates.source, candidates.precheck, candidates.saved),
        (
            "session 1".to_owned(),
            Precheck::On,
            Some(Saved {
                candidates: 0,
                questions: 0
            })
        )
    );
}

/// With no library in the data directory the bench says so, and doesn't
/// make one.
#[test]
fn a_missing_library_is_refused_and_not_made() {
    let dir = TestDir::new("no-library");
    let Err(err) = from_session(&dir.0, SESSION, Precheck::Off, Arc::new(clock())) else {
        panic!("a missing library was read");
    };
    let path = dir.0.join(LIBRARY_DB);
    assert_eq!(err, format!("there's no library at {}", path.display()));
    assert!(!path.exists(), "the bench made a library");
}

/// A session with no final text has nothing to bench.
#[test]
fn a_session_without_final_text_is_refused() {
    let library = Library::new("no-text", &[]);
    let Err(err) = from_session(&library.dir.0, SESSION, Precheck::Off, Arc::new(clock())) else {
        panic!("a session without final text was benched");
    };
    assert_eq!(err, "session 1 has no final text to bench");
}
