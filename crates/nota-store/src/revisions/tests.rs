use nota_core::{
    Drift, EpochAnchor, EpochId, SampleIndex, SampleRange, SampleRate, SessionId, SessionTime,
    TrackId,
};

use super::*;
use crate::test_dir::TestDir;
use crate::tests::{new_session, raw, refused};
use crate::{HeardBy, Sha256Digest};

const S1: SessionId = SessionId::new(1);
const S2: SessionId = SessionId::new(2);
const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);
/// A track with audio but no epoch anchor, as from an older nota's journals.
const UNTIMED: TrackId = TrackId::new(2);

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

fn secs(n: u64) -> SessionTime {
    SessionTime::from_nanos(n * 1_000_000_000)
}

/// The chunk of `track` from sample `a` to `b`, stored as `text` (or none),
/// with `words` spread evenly over it.
fn chunk(track: TrackId, a: u64, b: u64, text: Option<&str>, words: &[&str]) -> FinalText {
    let n = u64::try_from(words.len()).unwrap().max(1);
    let step = (b - a) / n;
    FinalText {
        track,
        range: range(a, b),
        text: text.map(str::to_owned),
        words: (0..)
            .zip(words)
            .map(|(i, w)| FinalWord {
                text: (*w).to_owned(),
                range: range(a + i * step, a + (i + 1) * step),
            })
            .collect(),
        heard_by: HeardBy {
            engine: "sherpa-onnx".to_owned(),
            model: "parakeet".to_owned(),
        },
    }
}

/// Stores `texts` as `session`'s final text on their track, up to the
/// last one's end.
fn add(store: &mut Store, session: SessionId, texts: &[FinalText]) {
    for text in texts {
        store
            .add_final_text(
                session,
                text.track,
                text.range.end(),
                std::slice::from_ref(text),
            )
            .unwrap();
    }
}

fn span(first: u32, last: u32) -> WordSpan {
    WordSpan::new(first, last).unwrap()
}

fn change(chunk: &FinalText, first: u32, last: u32, text: &str) -> Change {
    Change {
        chunk: chunk.key(),
        words: span(first, last),
        text: text.to_owned(),
    }
}

fn applied(first: u32, last: u32, text: &str, revision: RevisionNumber) -> Applied {
    Applied {
        words: span(first, last),
        text: text.to_owned(),
        revision,
    }
}

/// Chunk A on the microphone and chunk B on system audio. A's stored text
/// ends with a full stop its words don't have, so a changed chunk shows
/// it's read from its words.
fn two_chunks() -> (FinalText, FinalText) {
    (
        chunk(
            MIC,
            0,
            16_000,
            Some("the clam holds it."),
            &["the", "clam", "holds", "it"],
        ),
        chunk(
            SYSTEM,
            16_000,
            32_000,
            Some("hyper profen daily"),
            &["hyper", "profen", "daily"],
        ),
    )
}

/// Each line's text as `number` shows it, by first sample.
fn texts(store: &Store, number: RevisionNumber) -> Vec<Option<String>> {
    let mut lines = store.revision(S1, number).unwrap();
    lines.sort_by_key(|l| l.heard.range.start());
    lines.into_iter().map(|l| l.text).collect()
}

fn some(texts: &[&str]) -> Vec<Option<String>> {
    texts.iter().map(|t| Some((*t).to_owned())).collect()
}

/// Acceptance: revisions round-trip against final-text chunks and word
/// spans. Each is its parent with its own changes, back to the final text,
/// which reads as stored; the nearest revision's change wins where two
/// touch the same words.
#[test]
fn revisions_build_on_their_parents_and_never_change_them() {
    let (_dir, mut store) = store("revisions");
    let (a, b) = two_chunks();
    add(&mut store, S1, &[a.clone(), b.clone()]);
    let one = store
        .add_revision(S1, RevisionNumber::FINAL, &[change(&a, 1, 1, "clamp")])
        .unwrap();
    let two = store
        .add_revision(S1, one, &[change(&b, 0, 1, "ibuprofen")])
        .unwrap();
    // From the final text again: revision 1's change isn't in it.
    let three = store
        .add_revision(
            S1,
            RevisionNumber::FINAL,
            &[change(&b, 2, 2, "twice daily")],
        )
        .unwrap();
    // A change over revision 1's word: revision 1's is left out.
    let four = store
        .add_revision(S1, two, &[change(&a, 0, 1, "a clamp")])
        .unwrap();
    assert_eq!(
        [one, two, three, four].map(RevisionNumber::get),
        [1, 2, 3, 4]
    );

    assert_eq!(
        texts(&store, RevisionNumber::FINAL),
        some(&["the clam holds it.", "hyper profen daily"])
    );
    assert_eq!(
        texts(&store, one),
        some(&["the clamp holds it", "hyper profen daily"])
    );
    assert_eq!(
        texts(&store, two),
        some(&["the clamp holds it", "ibuprofen daily"])
    );
    assert_eq!(
        texts(&store, three),
        some(&["the clam holds it.", "hyper profen twice daily"])
    );
    assert_eq!(
        texts(&store, four),
        some(&["a clamp holds it", "ibuprofen daily"])
    );
    // Each line says which changes it shows, and which revision made them.
    let lines = store.revision(S1, four).unwrap();
    assert_eq!(lines[0].changes, [applied(0, 1, "a clamp", four)]);
    assert_eq!(lines[1].changes, [applied(0, 1, "ibuprofen", two)]);
    // The final text itself is as stored.
    assert_eq!(lines[0].heard, a);
    assert_eq!(store.final_texts(S1).unwrap(), [a, b]);
    assert_eq!(
        store.revisions(S1).unwrap(),
        [RevisionNumber::FINAL, one, two, three, four]
    );
}

/// Undoing a change is a new revision that puts its words back; the
/// revision undone still reads with it.
#[test]
fn undoing_a_change_is_a_new_revision() {
    let (_dir, mut store) = store("undo");
    let (a, b) = two_chunks();
    add(&mut store, S1, &[a.clone(), b]);
    let fixed = store
        .add_revision(S1, RevisionNumber::FINAL, &[change(&a, 0, 1, "a clamp")])
        .unwrap();
    let undone = store
        .add_revision(S1, fixed, &[change(&a, 0, 1, "the clam")])
        .unwrap();
    let lines = store.revision(S1, undone).unwrap();
    assert_eq!(lines[0].text.as_deref(), Some("the clam holds it"));
    assert_eq!(lines[0].changes, [applied(0, 1, "the clam", undone)]);
    assert_eq!(
        store.revision(S1, fixed).unwrap()[0].text.as_deref(),
        Some("a clamp holds it")
    );
}

/// Several changes in one chunk each take their own words, and a change to
/// nothing leaves no gap.
#[test]
fn changes_in_one_chunk_each_replace_their_own_words() {
    let (_dir, mut store) = store("several");
    let (_, b) = two_chunks();
    add(&mut store, S1, std::slice::from_ref(&b));
    let one = store
        .add_revision(
            S1,
            RevisionNumber::FINAL,
            &[change(&b, 2, 2, ""), change(&b, 0, 1, "ibuprofen")],
        )
        .unwrap();
    let lines = store.revision(S1, one).unwrap();
    assert_eq!(lines[0].text.as_deref(), Some("ibuprofen"));
    assert_eq!(
        lines[0].changes,
        [applied(0, 1, "ibuprofen", one), applied(2, 2, "", one)]
    );
}

/// A change must name its parent and words its own session's chunk has;
/// a refused revision adds nothing, so the next number is unchanged.
#[test]
fn a_revision_needs_its_parent_and_its_own_chunk_s_words() {
    let (_dir, mut store) = store("refused");
    // No final text yet, so no revision 0.
    assert!(matches!(
        store.revision(S1, RevisionNumber::FINAL),
        Err(StoreError::NoRevision(S1, 0))
    ));
    let (a, _) = two_chunks();
    let lost = chunk(MIC, 16_000, 32_000, None, &[]);
    let theirs = chunk(MIC, 40_000, 48_000, Some("elsewhere"), &["elsewhere"]);
    add(&mut store, S1, &[a.clone(), lost.clone()]);
    add(&mut store, S2, std::slice::from_ref(&theirs));
    assert!(matches!(
        store.add_revision(S1, RevisionNumber(5), &[change(&a, 0, 0, "x")]),
        Err(StoreError::NoRevision(S1, 5))
    ));
    for bad in [
        // Past its last word, with a first that exists.
        change(&a, 3, 4, "x"),
        // Past its last word altogether.
        change(&a, 9, 9, "x"),
        // Audio the engine couldn't transcribe has no words.
        change(&lost, 0, 0, "x"),
        // Another session's chunk.
        change(&theirs, 0, 0, "x"),
    ] {
        assert!(
            matches!(
                store.add_revision(S1, RevisionNumber::FINAL, std::slice::from_ref(&bad)),
                Err(StoreError::NoWords { session: S1, chunk, words })
                    if chunk == bad.chunk && words == bad.words
            ),
            "{bad:?}"
        );
    }
    // Two changes that share a word roll back together.
    assert!(
        store
            .add_revision(
                S1,
                RevisionNumber::FINAL,
                &[change(&a, 0, 1, "p"), change(&a, 1, 2, "q")]
            )
            .is_err()
    );
    assert!(matches!(
        store.revision(S1, RevisionNumber(1)),
        Err(StoreError::NoRevision(S1, 1))
    ));
    let next = store
        .add_revision(S1, RevisionNumber::FINAL, &[change(&a, 3, 3, "them")])
        .unwrap();
    assert_eq!(next.get(), 1);
}

/// Revision 0 comes with the session's first final-text row: not with
/// progress over silence, and once however many rows follow.
#[test]
fn the_first_final_text_makes_revision_0() {
    let (_dir, mut store) = store("revision-0");
    store.add_final_text(S1, MIC, s(100), &[]).unwrap();
    assert!(store.revisions(S1).unwrap().is_empty());
    let (a, b) = two_chunks();
    add(&mut store, S1, &[a, b]);
    assert_eq!(store.revisions(S1).unwrap(), [RevisionNumber::FINAL]);
    assert!(store.revisions(S2).unwrap().is_empty());
}

/// `track`'s epoch `id`, from `first` sample on, starting `start` seconds
/// into the session at 16 kHz.
fn anchor(id: u32, first: u64, start: u64) -> EpochAnchor {
    EpochAnchor {
        id: EpochId::new(id),
        start: secs(start),
        first_sample: s(first),
        rate: SampleRate::SPEECH,
        drift: Drift::ZERO,
    }
}

fn segment(track: TrackId, epoch: u32, a: u64, b: u64) -> SegmentRow {
    SegmentRow::new(
        track,
        EpochId::new(epoch),
        range(a, b),
        Sha256Digest::new([0; 32]),
    )
    .unwrap()
}

/// Acceptance: final rows read back in session order across two tracks,
/// placed by their epoch's anchor. A chunk ending where the next epoch
/// starts is timed by its own epoch; a track with no anchor comes last.
#[test]
fn final_rows_read_in_session_order_across_tracks() {
    let (_dir, mut store) = store("order");
    // The microphone from 1 s, reopened at 100 s after a gap; system audio
    // from 1 s at its sample 16 000.
    for (track, epoch) in [
        (MIC, anchor(0, 0, 1)),
        (MIC, anchor(1, 160_000, 100)),
        (SYSTEM, anchor(0, 16_000, 1)),
    ] {
        store.insert_epoch(S1, track, &epoch).unwrap();
    }
    for row in [
        segment(MIC, 0, 0, 160_000),
        segment(MIC, 1, 160_000, 320_000),
        segment(SYSTEM, 0, 16_000, 320_000),
        segment(UNTIMED, 0, 0, 16_000),
    ] {
        store.insert_segment(S1, &row).unwrap();
    }
    let stored = [
        chunk(MIC, 0, 16_000, Some("can you hear me"), &[]),
        chunk(MIC, 144_000, 160_000, Some("before the gap"), &[]),
        chunk(MIC, 160_000, 176_000, Some("yes"), &[]),
        chunk(SYSTEM, 24_000, 40_000, Some("welcome back"), &[]),
        chunk(UNTIMED, 0, 16_000, Some("untimed"), &[]),
    ];
    add(&mut store, S1, &stored);
    let lines = store.revision(S1, RevisionNumber::FINAL).unwrap();
    let read: Vec<_> = lines
        .iter()
        .map(|l| (l.heard.track, l.text.as_deref().unwrap(), l.at))
        .collect();
    let at = |start: u64, end: u64| {
        Some(Timed {
            start: SessionTime::from_nanos(start),
            end: SessionTime::from_nanos(end),
        })
    };
    assert_eq!(
        read,
        [
            (MIC, "can you hear me", at(1_000_000_000, 2_000_000_000)),
            (SYSTEM, "welcome back", at(1_500_000_000, 2_500_000_000)),
            (MIC, "before the gap", at(10_000_000_000, 11_000_000_000)),
            (MIC, "yes", at(100_000_000_000, 101_000_000_000)),
            (UNTIMED, "untimed", None),
        ]
    );
}

/// Two chunks timed alike read by track, then first sample.
#[test]
fn chunks_at_the_same_time_read_by_track() {
    let (_dir, mut store) = store("ties");
    for track in [SYSTEM, MIC] {
        store.insert_epoch(S1, track, &anchor(0, 0, 0)).unwrap();
        store
            .insert_segment(S1, &segment(track, 0, 0, 16_000))
            .unwrap();
    }
    add(
        &mut store,
        S1,
        &[
            chunk(SYSTEM, 0, 8_000, Some("b"), &[]),
            chunk(MIC, 0, 8_000, Some("a"), &[]),
        ],
    );
    let read: Vec<_> = store
        .revision(S1, RevisionNumber::FINAL)
        .unwrap()
        .into_iter()
        .map(|l| l.heard.track)
        .collect();
    assert_eq!(read, [MIC, SYSTEM]);
}

/// A track whose stored anchors don't make a timeline (a damaged row:
/// the second epoch starts before the first one's audio ends) reads
/// untimed, not misplaced.
#[test]
fn a_track_whose_anchors_do_not_rebuild_reads_untimed() {
    let (_dir, mut store) = store("bad-anchors");
    store.insert_epoch(S1, MIC, &anchor(0, 0, 10)).unwrap();
    store.insert_epoch(S1, MIC, &anchor(1, 160_000, 5)).unwrap();
    store
        .insert_segment(S1, &segment(MIC, 0, 0, 160_000))
        .unwrap();
    add(&mut store, S1, &[chunk(MIC, 0, 16_000, Some("x"), &[])]);
    let lines = store.revision(S1, RevisionNumber::FINAL).unwrap();
    assert_eq!(lines[0].at, None);
}

/// SQLite itself refuses to change, delete or replace a revision or its
/// changes, from any connection, or to add to one but the newest, or to
/// revision 0.
#[test]
fn sqlite_refuses_to_change_a_revision() {
    let (dir, mut store) = store("append-only");
    let (a, b) = two_chunks();
    add(&mut store, S1, &[a.clone(), b.clone()]);
    let one = store
        .add_revision(S1, RevisionNumber::FINAL, &[change(&a, 1, 1, "clamp")])
        .unwrap();
    store
        .add_revision(S1, one, &[change(&b, 0, 1, "ibuprofen")])
        .unwrap();
    let before = store.revision(S1, one).unwrap();
    drop(store);

    // A plain connection, with none of the store's settings.
    let conn = raw(&dir.db());
    let insert = |revision: u32, first: u32, last: u32| {
        format!(
            "INSERT INTO revision_text \
             (session_id, revision, track, start_sample, first_word, last_word, text) \
             VALUES (1, {revision}, 1, 16000, {first}, {last}, 'added')"
        )
    };
    for sql in [
        "UPDATE revision SET parent = NULL WHERE number = 1".to_owned(),
        "DELETE FROM revision WHERE number = 1".to_owned(),
        "INSERT OR REPLACE INTO revision (session_id, number, parent) VALUES (1, 1, NULL)"
            .to_owned(),
        "UPDATE revision_text SET text = 'changed'".to_owned(),
        "DELETE FROM revision_text".to_owned(),
        format!("INSERT OR REPLACE {}", &insert(2, 0, 1)["INSERT ".len()..]),
        // A word the newest revision changes already.
        insert(2, 1, 2),
        // Revision 0, and a revision that isn't the newest.
        insert(0, 2, 2),
        insert(1, 2, 2),
    ] {
        assert!(refused(&conn, &sql), "{sql} was allowed");
    }
    // Recursive triggers on make no difference.
    conn.execute_batch("PRAGMA recursive_triggers = ON")
        .unwrap();
    assert!(refused(&conn, "DELETE FROM revision_text"));
    drop(conn);

    let store = Store::open(&dir.db()).unwrap();
    assert_eq!(store.revision(S1, one).unwrap(), before);
}

#[test]
fn a_word_span_never_ends_before_it_starts() {
    assert_eq!(WordSpan::new(2, 1), None);
    let one = span(3, 3);
    assert_eq!((one.first(), one.last()), (3, 3));
    assert!(span(0, 3).overlaps(span(3, 4)));
    assert!(span(3, 4).overlaps(span(0, 3)));
    assert!(!span(0, 2).overlaps(span(3, 4)));
    assert!(!span(3, 4).overlaps(span(0, 2)));
}
