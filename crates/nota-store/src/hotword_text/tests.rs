use std::collections::BTreeMap;

use nota_core::{SampleIndex, SampleRange, SessionId, TrackId};

use super::*;
use crate::test_dir::TestDir;
use crate::tests::{new_session, raw, refused};
use crate::{FinalText, HeardBy};

const S1: SessionId = SessionId::new(1);
const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);

fn s(n: u64) -> SampleIndex {
    SampleIndex::new(n)
}

fn range(a: u64, b: u64) -> SampleRange {
    SampleRange::new(s(a), s(b)).unwrap()
}

/// A store whose session 1 has two final-pass chunks on the mic, 0..400
/// and 400..900, and one on the system track, 0..300.
fn store(name: &str) -> (TestDir, Store) {
    let dir = TestDir::new(name);
    let mut store = Store::open(&dir.db()).unwrap();
    store.create_session(&new_session(S1)).unwrap();
    let chunk = |track, a, b| FinalText {
        track,
        range: range(a, b),
        text: Some("amylo diping".to_owned()),
        words: Vec::new(),
        heard_by: HeardBy {
            engine: "sherpa-onnx".to_owned(),
            model: "parakeet".to_owned(),
        },
    };
    store
        .add_final_text(S1, MIC, s(900), &[chunk(MIC, 0, 400), chunk(MIC, 400, 900)])
        .unwrap();
    store
        .add_final_text(S1, SYSTEM, s(300), &[chunk(SYSTEM, 0, 300)])
        .unwrap();
    (dir, store)
}

/// The boosted text of the chunk at `a..b`.
fn boosted(track: TrackId, a: u64, b: u64, heard: Option<&str>) -> HotwordText {
    HotwordText {
        track,
        range: range(a, b),
        text: heard.map(str::to_owned),
        boost: 3.0,
    }
}

/// Each chunk's boosted text reads back by track and sample, with its
/// boost, and the progress with it.
#[test]
fn boosted_text_reads_back_with_its_progress() {
    let (_dir, mut store) = store("hotword-reads");
    let first = boosted(MIC, 0, 400, Some("amlodipine"));
    let silent = boosted(SYSTEM, 0, 300, None);
    store
        .add_hotword_text(S1, MIC, s(400), std::slice::from_ref(&first))
        .unwrap();
    store
        .add_hotword_text(S1, SYSTEM, s(300), std::slice::from_ref(&silent))
        .unwrap();
    assert_eq!(store.hotword_texts(S1).unwrap(), [first, silent]);
    assert_eq!(
        store.hotword_progress(S1).unwrap(),
        BTreeMap::from([(MIC, s(400)), (SYSTEM, s(300))])
    );
}

/// A write retried after its answer was lost stores nothing twice, and
/// progress never goes back.
#[test]
fn a_retried_write_stores_nothing_twice() {
    let (_dir, mut store) = store("hotword-retry");
    let first = boosted(MIC, 0, 400, Some("amlodipine"));
    store
        .add_hotword_text(S1, MIC, s(400), std::slice::from_ref(&first))
        .unwrap();
    store
        .add_hotword_text(S1, MIC, s(400), std::slice::from_ref(&first))
        .unwrap();
    store.add_hotword_text(S1, MIC, s(100), &[]).unwrap();
    assert_eq!(store.hotword_texts(S1).unwrap(), [first]);
    assert_eq!(
        store.hotword_progress(S1).unwrap(),
        BTreeMap::from([(MIC, s(400))])
    );
}

/// Text that doesn't cover exactly one final-pass chunk, is on another
/// track, ends past its progress, has no boost, or differs from what's
/// stored is refused, and nothing is stored.
#[test]
fn misplaced_text_is_refused() {
    let (_dir, mut store) = store("hotword-misplaced");
    store
        .add_hotword_text(S1, MIC, s(400), &[boosted(MIC, 0, 400, Some("amlodipine"))])
        .unwrap();
    let mut no_boost = boosted(MIC, 400, 900, None);
    no_boost.boost = 0.0;
    let mut nan = boosted(MIC, 400, 900, None);
    nan.boost = f64::NAN;
    let mut endless = boosted(MIC, 400, 900, None);
    endless.boost = f64::INFINITY;
    let cases = [
        (MIC, 900, boosted(MIC, 400, 800, None)),
        (MIC, 900, boosted(MIC, 300, 900, None)),
        (MIC, 900, boosted(SYSTEM, 0, 300, None)),
        (MIC, 800, boosted(MIC, 400, 900, None)),
        (MIC, 900, no_boost),
        (MIC, 900, nan),
        (MIC, 900, endless),
        (MIC, 400, boosted(MIC, 0, 400, Some("other"))),
    ];
    for (track, up_to, text) in cases {
        let start = text.range.start();
        let got = store.add_hotword_text(S1, track, s(up_to), std::slice::from_ref(&text));
        assert!(
            matches!(got, Err(StoreError::HotwordMisplaced { track: t, start: at }) if t == track && at == start),
            "{text:?}: {got:?}"
        );
    }
    assert_eq!(store.hotword_texts(S1).unwrap().len(), 1);
    assert_eq!(
        store.hotword_progress(S1).unwrap(),
        BTreeMap::from([(MIC, s(400))])
    );
}

/// A session not in the library is refused.
#[test]
fn an_unknown_session_is_refused() {
    let (_dir, mut store) = store("hotword-no-session");
    let got = store.add_hotword_text(SessionId::new(9), MIC, s(1), &[]);
    assert!(
        matches!(got, Err(StoreError::NoSession(id)) if id == SessionId::new(9)),
        "{got:?}"
    );
}

/// Stored boosted text is a record: the schema refuses changing,
/// deleting or replacing it, whatever connection tries.
#[test]
fn boosted_text_is_never_changed() {
    let (dir, mut store) = store("hotword-record");
    store
        .add_hotword_text(S1, MIC, s(400), &[boosted(MIC, 0, 400, Some("amlodipine"))])
        .unwrap();
    drop(store);
    let conn = raw(&dir.db());
    for sql in [
        "UPDATE hotword_text SET text = 'other'",
        "DELETE FROM hotword_text",
        "INSERT OR REPLACE INTO hotword_text VALUES (1, 0, 0, 400, 'other', 3.0)",
    ] {
        assert!(refused(&conn, sql), "{sql} was allowed");
    }
}

/// `terms` left out of `given`.
fn left_out(given: usize, terms: &[&str]) -> LeftOut {
    LeftOut::new(given, terms.iter().map(|&t| t.to_owned()).collect()).unwrap()
}

/// Before any engine has said, there's nothing; what one says reads back
/// in order, and a report with none left out reads back as such, not as
/// nothing.
#[test]
fn left_out_terms_read_back_once_told() {
    let (_dir, mut store) = store("left-out");
    assert_eq!(store.hotword_left_out(S1).unwrap(), None);
    let said = left_out(3, &["varicose veins", "Mg2+"]);
    store.set_hotword_left_out(S1, &said).unwrap();
    assert_eq!(store.hotword_left_out(S1).unwrap(), Some(said));
    let none = left_out(3, &[]);
    store.set_hotword_left_out(S1, &none).unwrap();
    assert_eq!(store.hotword_left_out(S1).unwrap(), Some(none));
}

/// A later report replaces the earlier one whole: a term no longer left
/// out goes, and the count is the new one.
#[test]
fn a_later_report_replaces_the_earlier() {
    let (_dir, mut store) = store("left-out-again");
    store
        .set_hotword_left_out(S1, &left_out(5, &["a", "b", "c"]))
        .unwrap();
    let later = left_out(4, &["d"]);
    store.set_hotword_left_out(S1, &later).unwrap();
    assert_eq!(store.hotword_left_out(S1).unwrap(), Some(later));
}

/// A report for a session the library doesn't have is refused.
#[test]
fn left_out_terms_for_an_unknown_session_are_refused() {
    let (_dir, mut store) = store("left-out-unknown");
    let got = store.set_hotword_left_out(SessionId::new(9), &left_out(1, &["a"]));
    assert!(
        matches!(got, Err(StoreError::NoSession(id)) if id == SessionId::new(9)),
        "{got:?}"
    );
}

/// More terms left out than given, or an empty one, isn't a report.
#[test]
fn a_report_never_leaves_out_more_than_it_was_given() {
    assert_eq!(LeftOut::new(1, vec!["a".to_owned(), "b".to_owned()]), None);
    assert_eq!(LeftOut::new(1, vec![String::new()]), None);
    let fits = LeftOut::new(2, vec!["a".to_owned(), "b".to_owned()]).unwrap();
    assert_eq!(
        (fits.given(), fits.terms()),
        (2, &["a".to_owned(), "b".to_owned()][..])
    );
}

/// Rows that leave out more terms than were given read as corrupt, not as
/// a report.
#[test]
fn stored_rows_that_leave_out_too_many_are_corrupt() {
    let (dir, mut store) = store("left-out-corrupt");
    store
        .set_hotword_left_out(S1, &left_out(1, &["a"]))
        .unwrap();
    raw(&dir.db())
        .execute("INSERT INTO hotword_left_out_term VALUES (1, 1, 'b')", [])
        .unwrap();
    let got = store.hotword_left_out(S1);
    assert!(matches!(got, Err(StoreError::Corrupt(_))), "{got:?}");
}
