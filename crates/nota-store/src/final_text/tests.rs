use nota_core::{SampleIndex, SampleRange, SessionId, TrackId};

use super::*;
use crate::test_dir::TestDir;
use crate::tests::new_session;

const S1: SessionId = SessionId::new(1);
const S2: SessionId = SessionId::new(2);
const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);

fn store(name: &str) -> (TestDir, Store) {
    let dir = TestDir::new(name);
    let mut store = Store::open(&dir.db()).unwrap();
    store.create_session(&new_session(S1)).unwrap();
    store.create_session(&new_session(S2)).unwrap();
    (dir, store)
}

fn s(n: u64) -> SampleIndex {
    SampleIndex::new(n)
}

fn range(a: u64, b: u64) -> SampleRange {
    SampleRange::new(s(a), s(b)).unwrap()
}

fn text(track: TrackId, a: u64, b: u64, heard: Option<&str>) -> FinalText {
    FinalText {
        track,
        range: range(a, b),
        text: heard.map(str::to_owned),
        words: Vec::new(),
        heard_by: HeardBy {
            engine: "sherpa-onnx".to_owned(),
            model: "parakeet".to_owned(),
        },
    }
}

/// Text and progress go in together and read back by track and sample;
/// another session's are its own.
#[test]
fn final_text_reads_back_by_track_and_sample_with_its_progress() {
    let (_dir, mut store) = store("read-back");
    let mut worded = text(MIC, 0, 400, Some("good morning"));
    worded.words = vec![
        FinalWord {
            text: "good".into(),
            range: range(10, 100),
        },
        FinalWord {
            text: "morning".into(),
            range: range(120, 380),
        },
    ];
    store
        .add_final_text(S1, SYSTEM, s(300), &[text(SYSTEM, 0, 300, None)])
        .unwrap();
    store
        .add_final_text(
            S1,
            MIC,
            s(900),
            &[worded.clone(), text(MIC, 500, 900, Some("today"))],
        )
        .unwrap();
    store.add_final_text(S2, MIC, s(50), &[]).unwrap();
    assert_eq!(
        store.final_texts(S1).unwrap(),
        [
            worded,
            text(MIC, 500, 900, Some("today")),
            text(SYSTEM, 0, 300, None)
        ]
    );
    assert_eq!(
        store
            .final_progress(S1)
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        [(MIC, s(900)), (SYSTEM, s(300))]
    );
    assert!(store.final_texts(S2).unwrap().is_empty());
    assert_eq!(
        store
            .final_progress(S2)
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        [(MIC, s(50))]
    );
}

/// The same text stored twice (an answer lost, the write retried) is
/// stored once; progress never goes back.
#[test]
fn a_retried_write_stores_nothing_twice_and_progress_never_goes_back() {
    let (_dir, mut store) = store("retry");
    let first = [text(MIC, 0, 100, Some("one"))];
    store.add_final_text(S1, MIC, s(100), &first).unwrap();
    store.add_final_text(S1, MIC, s(100), &first).unwrap();
    store.add_final_text(S1, MIC, s(40), &[]).unwrap();
    assert_eq!(store.final_texts(S1).unwrap(), first);
    assert_eq!(store.final_progress(S1).unwrap()[&MIC], s(100));
}

/// Text that would cover a sample twice, sits on another track, or ends
/// past its progress is refused, and nothing of its call is stored.
#[test]
fn text_that_overlaps_or_runs_past_its_progress_is_refused() {
    let (_dir, mut store) = store("overlap");
    store
        .add_final_text(S1, MIC, s(100), &[text(MIC, 0, 100, Some("one"))])
        .unwrap();
    for (up_to, given) in [
        // The same samples, other text.
        (100, vec![text(MIC, 0, 100, Some("won"))]),
        // Overlapping its end.
        (200, vec![text(MIC, 50, 200, Some("two"))]),
        // Inside it.
        (100, vec![text(MIC, 10, 20, None)]),
        // Two of its own that overlap, after one that's fine.
        (
            400,
            vec![
                text(MIC, 100, 300, Some("a")),
                text(MIC, 250, 400, Some("b")),
            ],
        ),
        // Past the progress given with it.
        (150, vec![text(MIC, 100, 200, Some("c"))]),
        // On another track.
        (300, vec![text(SYSTEM, 100, 200, Some("d"))]),
    ] {
        assert!(
            matches!(
                store.add_final_text(S1, MIC, s(up_to), &given),
                Err(StoreError::FinalOverlap { track: MIC, .. })
            ),
            "{given:?}"
        );
    }
    assert_eq!(
        store.final_texts(S1).unwrap(),
        [text(MIC, 0, 100, Some("one"))]
    );
    assert_eq!(store.final_progress(S1).unwrap()[&MIC], s(100));
    assert!(matches!(
        store.add_final_text(SessionId::new(7), MIC, s(1), &[]),
        Err(StoreError::NoSession(_))
    ));
}

/// Text stored on the live path and the final pass's never meet: the
/// heard text and its revisions show none of the final pass's.
#[test]
fn the_final_pass_never_shows_in_the_heard_text() {
    let (_dir, mut store) = store("apart");
    store
        .add_final_text(S1, MIC, s(100), &[text(MIC, 0, 100, Some("final"))])
        .unwrap();
    assert!(store.utterances(S1).unwrap().is_empty());
    assert!(store.revisions(S1).unwrap().is_empty());
}
