use std::path::PathBuf;

use proptest::prelude::*;

use super::*;
use crate::test_dir::TestDir;
use crate::tests::raw;

/// A source with id `id` and licence `licence`.
fn source(id: &str, licence: &str) -> Source {
    Source {
        id: id.to_owned(),
        name: format!("The {id} list"),
        licence: licence.to_owned(),
        attribution: format!("Contains names from the {id} list."),
        url: Some(format!("https://example.org/{id}")),
    }
}

/// A shipped pack's manifest with two sources: `open` (CC0-1.0) and
/// `public` (public domain).
fn manifest(kind: PackKind) -> Manifest {
    Manifest {
        name: "pharmacy".to_owned(),
        version: "2026.11".to_owned(),
        language: "en".to_owned(),
        kind,
        sources: vec![source("public", "public domain"), source("open", "CC0-1.0")],
    }
}

/// A term row from `source`, with that source's licence.
fn term(term: &str, source: &str, review: Review) -> TermRow {
    TermRow {
        term: term.to_owned(),
        source: source.to_owned(),
        licence: licence_of(source).to_owned(),
        review,
    }
}

/// An alias row from `source`, with that source's licence.
fn alias(term: &str, alias: &str, source: &str, review: Review) -> AliasRow {
    AliasRow {
        term: term.to_owned(),
        alias: alias.to_owned(),
        source: source.to_owned(),
        licence: licence_of(source).to_owned(),
        review,
    }
}

/// The licence [`manifest`] gives `source`.
fn licence_of(source: &str) -> &'static str {
    match source {
        "open" => "CC0-1.0",
        _ => "public domain",
    }
}

/// A pack with reviewed and draft terms and aliases from both sources.
fn sample(kind: PackKind) -> Pack {
    Pack::new(
        manifest(kind),
        vec![
            term("amlodipine", "public", Review::Reviewed),
            term("amlodipine", "open", Review::Reviewed),
            term("GLP-1", "open", Review::Reviewed),
            term("semaglutide", "open", Review::Draft),
        ],
        vec![
            alias("GLP-1", "gee el pee one", "open", Review::Reviewed),
            alias("GLP-1", "glucagon-like peptide 1", "open", Review::Draft),
            alias("semaglutide", "sema", "open", Review::Draft),
        ],
    )
    .unwrap()
}

/// The path of a pack file named `name` in `dir`.
fn pack_path(dir: &TestDir, name: &str) -> PathBuf {
    dir.0.join(format!("{name}.pack"))
}

/// The file's bytes.
fn bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

/// Acceptance: a pack written to a file reads back as the same pack,
/// manifest and every row included.
#[test]
fn a_pack_round_trips_through_write_and_read() {
    let dir = TestDir::new("pack-round-trip");
    let pack = sample(PackKind::Shipped);
    let path = pack_path(&dir, "pharmacy");
    pack.write(&path).unwrap();
    let read = Pack::open(&path).unwrap();
    assert_eq!(read, pack);
    // The manifest is all there, not only the rows.
    assert_eq!(read.manifest(), &manifest(PackKind::Shipped).sorted());
}

impl Manifest {
    /// The manifest with its sources in id order, as a pack holds them.
    fn sorted(mut self) -> Self {
        self.sources.sort();
        self
    }
}

/// The same rows in another order, with a row given twice, make the same
/// pack and the same file bytes: what a reproducible build rests on.
#[test]
fn the_same_rows_in_any_order_give_the_same_bytes() {
    let dir = TestDir::new("pack-bytes");
    let pack = sample(PackKind::Shipped);
    let mut terms = pack.terms().to_vec();
    terms.reverse();
    terms.push(terms[0].clone());
    let mut aliases = pack.aliases().to_vec();
    aliases.reverse();
    let mut m = manifest(PackKind::Shipped);
    m.sources.reverse();
    let shuffled = Pack::new(m, terms, aliases).unwrap();
    assert_eq!(shuffled, pack);
    // Acceptance (#226): the same rows give the same digest.
    assert_eq!(shuffled.digest(), pack.digest());

    let (a, b) = (pack_path(&dir, "a"), pack_path(&dir, "b"));
    pack.write(&a).unwrap();
    shuffled.write(&b).unwrap();
    assert_eq!(bytes(&a), bytes(&b));
}

/// Acceptance (#226): a pack's digest is kept in its file and read back
/// on open, the same as the pack it was written from.
#[test]
fn a_packs_digest_is_read_back_from_its_file() {
    let dir = TestDir::new("pack-digest-file");
    let pack = sample(PackKind::Shipped);
    let path = pack_path(&dir, "pharmacy");
    pack.write(&path).unwrap();
    let stored: Vec<u8> = raw(&path)
        .query_row("SELECT digest FROM manifest", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, pack.digest().as_bytes());
    assert_eq!(Pack::open(&path).unwrap().digest(), pack.digest());
}

/// The digest of [`sample`]'s shipped pack, as this encoding gives it. A
/// change to the encoding changes every pack's digest, so sessions
/// recorded before it would find none of their packs: it needs a new
/// [`DIGEST_TAG`] and a plan for those sessions, not a new value here.
const SAMPLE_DIGEST: &str = "d7ce59153a5ac14b41b36be5db419cc2ca76b83cb64b3fff26c4e1c48478c666";

/// The digest is a fixed function of the pack: a later build of nota
/// works out the one an earlier build recorded.
#[test]
fn a_packs_digest_does_not_change_between_builds() {
    assert_eq!(
        sample(PackKind::Shipped).digest().to_string(),
        SAMPLE_DIGEST
    );
}

/// A digest shows in debug output as its hex, so an error or a failed
/// assert names which build it was.
#[test]
fn a_digest_debugs_as_its_hex() {
    assert_eq!(
        format!("{:?}", sample(PackKind::Shipped).digest()),
        format!("PackDigest({SAMPLE_DIGEST})")
    );
}

/// A pack as a session records it holds the manifest's name and version
/// and the pack's digest.
#[test]
fn a_pack_ref_holds_the_name_version_and_digest() {
    let pack = sample(PackKind::Shipped);
    let pack_ref = pack.pack_ref();
    assert_eq!(
        (pack_ref.name(), pack_ref.version(), pack_ref.digest()),
        ("pharmacy", "2026.11", pack.digest())
    );
}

/// What [`every_field_of_a_pack_changes_its_digest`] changes in a pack.
type Edit = fn(&mut Manifest, &mut Vec<TermRow>, &mut Vec<AliasRow>);

/// Acceptance (#226): every part of the manifest and every part of a row
/// is in the digest, so a pack differing in any one has another digest.
#[test]
fn every_field_of_a_pack_changes_its_digest() {
    let base = || {
        (
            manifest(PackKind::Shipped),
            vec![term("GLP-1", "open", Review::Reviewed)],
            vec![alias("GLP-1", "gee el pee one", "open", Review::Reviewed)],
        )
    };
    let edits: [(&str, Edit); 17] = [
        ("nothing", |_, _, _| {}),
        ("name", |m, _, _| m.name = "law".to_owned()),
        ("version", |m, _, _| m.version = "1".to_owned()),
        ("language", |m, _, _| m.language = "fr".to_owned()),
        ("kind", |m, _, _| m.kind = PackKind::Own),
        ("source id", |m, _, _| m.sources[0].id = "other".to_owned()),
        ("source name", |m, _, _| m.sources[0].name = "A".to_owned()),
        ("source licence", |m, _, _| {
            m.sources[0].licence = "PD".to_owned();
        }),
        ("attribution", |m, _, _| {
            m.sources[0].attribution = "Other.".to_owned();
        }),
        ("no address", |m, _, _| m.sources[0].url = None),
        ("address", |m, _, _| {
            m.sources[0].url = Some("https://example.org".to_owned());
        }),
        ("term", |_, t, a| {
            t[0].term = "GLP-2".to_owned();
            a.clear();
        }),
        ("term source", |_, t, _| {
            t.push(term("GLP-1", "public", Review::Reviewed));
            t.remove(0);
        }),
        ("term review", |_, t, _| t[0].review = Review::Draft),
        ("alias", |_, _, a| a[0].alias = "glp one".to_owned()),
        ("alias source", |_, t, a| {
            t.push(term("GLP-1", "public", Review::Reviewed));
            a[0] = alias("GLP-1", "gee el pee one", "public", Review::Reviewed);
        }),
        ("alias review", |_, _, a| a[0].review = Review::Draft),
    ];
    let mut seen = BTreeMap::new();
    for (what, edit) in edits {
        let (mut m, mut t, mut a) = base();
        edit(&mut m, &mut t, &mut a);
        let digest = Pack::new(m, t, a).unwrap().digest();
        if let Some(other) = seen.insert(digest, what) {
            panic!("{what} and {other} have one digest");
        }
    }
}

/// Where one text ends and the next starts is part of the digest: a name
/// and version that run together alike still differ.
#[test]
fn texts_that_run_together_alike_have_other_digests() {
    let with = |name: &str, version: &str| {
        let mut m = manifest(PackKind::Shipped);
        m.name = name.to_owned();
        m.version = version.to_owned();
        Pack::new(m, vec![], vec![]).unwrap().digest()
    };
    assert_ne!(with("ab", "c"), with("a", "bc"));
}

/// Acceptance: a row with no source, or no licence, fails the build, and
/// the error names the row's term.
#[test]
fn a_row_without_a_source_or_licence_fails() {
    let mut no_source = term("amlodipine", "public", Review::Reviewed);
    no_source.source = String::new();
    let err = Pack::new(manifest(PackKind::Shipped), vec![no_source], vec![]).unwrap_err();
    assert!(
        matches!(&err, PackError::MissingSource { term } if term == "amlodipine"),
        "{err:?}"
    );

    let mut no_licence = term("amlodipine", "public", Review::Reviewed);
    no_licence.licence = String::new();
    let err = Pack::new(manifest(PackKind::Shipped), vec![no_licence], vec![]).unwrap_err();
    assert!(
        matches!(&err, PackError::MissingLicence { term } if term == "amlodipine"),
        "{err:?}"
    );

    // An alias row is a row too.
    let mut no_source = alias("GLP-1", "gee el pee one", "open", Review::Reviewed);
    no_source.source = String::new();
    let err = Pack::new(
        manifest(PackKind::Shipped),
        vec![term("GLP-1", "open", Review::Reviewed)],
        vec![no_source],
    )
    .unwrap_err();
    assert!(
        matches!(&err, PackError::MissingSource { term } if term == "GLP-1"),
        "{err:?}"
    );
}

/// A row must name one of the manifest's sources, with that source's
/// licence, so the credits generated from the manifest cover it.
#[test]
fn a_row_names_a_source_of_the_manifest_with_its_licence() {
    let err = Pack::new(
        manifest(PackKind::Shipped),
        vec![term("amlodipine", "elsewhere", Review::Reviewed)],
        vec![],
    )
    .unwrap_err();
    assert!(
        matches!(&err, PackError::UnknownSource { term, source }
            if term == "amlodipine" && source == "elsewhere"),
        "{err:?}"
    );

    let mut other = term("amlodipine", "open", Review::Reviewed);
    other.licence = "CC-BY-4.0".to_owned();
    let err = Pack::new(manifest(PackKind::Shipped), vec![other], vec![]).unwrap_err();
    assert!(
        matches!(&err, PackError::LicenceMismatch { term, source }
            if term == "amlodipine" && source == "open"),
        "{err:?}"
    );
}

/// An alias of a term the pack lacks is refused: it would be offered for
/// nothing.
#[test]
fn an_alias_needs_its_term() {
    let err = Pack::new(
        manifest(PackKind::Shipped),
        vec![term("amlodipine", "open", Review::Reviewed)],
        vec![alias("GLP-1", "gee el pee one", "open", Review::Reviewed)],
    )
    .unwrap_err();
    assert!(
        matches!(&err, PackError::AliasWithoutTerm { term, alias }
            if term == "GLP-1" && alias == "gee el pee one"),
        "{err:?}"
    );
}

/// One row given as reviewed and as draft can't be both: the build fails
/// rather than pick one.
#[test]
fn a_row_given_twice_with_two_statuses_fails() {
    let err = Pack::new(
        manifest(PackKind::Shipped),
        vec![
            term("amlodipine", "open", Review::Reviewed),
            term("amlodipine", "open", Review::Draft),
        ],
        vec![],
    )
    .unwrap_err();
    assert!(
        matches!(&err, PackError::Conflict { term, source }
            if term == "amlodipine" && source == "open"),
        "{err:?}"
    );

    let err = Pack::new(
        manifest(PackKind::Shipped),
        vec![term("GLP-1", "open", Review::Reviewed)],
        vec![
            alias("GLP-1", "gee el pee one", "open", Review::Reviewed),
            alias("GLP-1", "gee el pee one", "open", Review::Draft),
        ],
    )
    .unwrap_err();
    assert!(
        matches!(&err, PackError::Conflict { term, source } if term == "GLP-1" && source == "open"),
        "{err:?}"
    );
}

/// Text that would make two spellings of one term, or break a line of
/// the credits, is refused: empty, padded, or with a tab or line break.
#[test]
fn bad_text_is_refused() {
    for bad in [
        "",
        " amlodipine",
        "amlodipine ",
        "amlo\tdipine",
        "amlo\ndipine",
    ] {
        let err = Pack::new(
            manifest(PackKind::Shipped),
            vec![term(bad, "open", Review::Reviewed)],
            vec![],
        )
        .unwrap_err();
        assert!(
            matches!(&err, PackError::BadText { field: "term", text } if text == bad),
            "{bad:?}: {err:?}"
        );
    }
    let long = "a".repeat(MAX_TEXT + 1);
    let err = Pack::new(
        manifest(PackKind::Shipped),
        vec![term(&long, "open", Review::Reviewed)],
        vec![],
    )
    .unwrap_err();
    assert!(
        matches!(err, PackError::BadText { field: "term", .. }),
        "{err:?}"
    );
    // An alias is checked as a term is.
    let err = Pack::new(
        manifest(PackKind::Shipped),
        vec![term("GLP-1", "open", Review::Reviewed)],
        vec![alias("GLP-1", "gee\tel", "open", Review::Reviewed)],
    )
    .unwrap_err();
    assert!(
        matches!(&err, PackError::BadText { field: "alias", text } if text == "gee\tel"),
        "{err:?}"
    );
    // A term at the limit is fine.
    let at_limit = "a".repeat(MAX_TEXT);
    Pack::new(
        manifest(PackKind::Shipped),
        vec![term(&at_limit, "open", Review::Reviewed)],
        vec![],
    )
    .unwrap();

    let mut m = manifest(PackKind::Shipped);
    m.version = String::new();
    let err = Pack::new(m, vec![], vec![]).unwrap_err();
    assert!(
        matches!(
            err,
            PackError::BadText {
                field: "pack version",
                ..
            }
        ),
        "{err:?}"
    );

    // An attribution may run over lines, but not be blank.
    let mut m = manifest(PackKind::Shipped);
    m.sources[0].attribution = "Line one.\nLine two.".to_owned();
    Pack::new(m, vec![], vec![]).unwrap();
    let mut m = manifest(PackKind::Shipped);
    m.sources[0].attribution = " \n".to_owned();
    let err = Pack::new(m, vec![], vec![]).unwrap_err();
    assert!(
        matches!(
            err,
            PackError::BadText {
                field: "source attribution",
                ..
            }
        ),
        "{err:?}"
    );
}

/// A manifest needs sources, each with its own id.
#[test]
fn the_sources_are_there_and_distinct() {
    let mut m = manifest(PackKind::Shipped);
    m.sources.clear();
    assert!(matches!(
        Pack::new(m, vec![], vec![]),
        Err(PackError::NoSources)
    ));

    let mut m = manifest(PackKind::Shipped);
    m.sources.push(source("open", "CC0-1.0"));
    let err = Pack::new(m, vec![], vec![]).unwrap_err();
    assert!(
        matches!(&err, PackError::DuplicateSource(id) if id == "open"),
        "{err:?}"
    );
}

/// Acceptance: from a shipped pack, a term with only draft rows isn't
/// offered, and neither is a draft alias of a reviewed term.
#[test]
fn a_draft_row_is_not_offered_from_a_shipped_pack() {
    let sample = sample(PackKind::Shipped);
    let offered = sample.offered();
    let pack = PackRef {
        name: "pharmacy".to_owned(),
        version: "2026.11".to_owned(),
        digest: sample.digest(),
    };
    assert_eq!(
        offered,
        [
            Offered {
                term: "GLP-1".to_owned(),
                aliases: vec!["gee el pee one".to_owned()],
                language: "en".to_owned(),
                pack: pack.clone(),
                sources: vec!["open".to_owned()],
                reviewed: true,
            },
            Offered {
                term: "amlodipine".to_owned(),
                aliases: vec![],
                language: "en".to_owned(),
                pack,
                sources: vec!["open".to_owned(), "public".to_owned()],
                reviewed: true,
            },
        ]
    );
}

/// From the user's own pack, every row is offered, and a term with a
/// draft row shows as unreviewed.
#[test]
fn every_row_of_an_own_pack_is_offered_marked_unreviewed() {
    let offered = sample(PackKind::Own).offered();
    let summary: Vec<_> = offered
        .iter()
        .map(|o| (o.term.as_str(), o.aliases.clone(), o.reviewed))
        .collect();
    assert_eq!(
        summary,
        [
            (
                "GLP-1",
                vec![
                    "gee el pee one".to_owned(),
                    "glucagon-like peptide 1".to_owned()
                ],
                false
            ),
            ("amlodipine", vec![], true),
            ("semaglutide", vec!["sema".to_owned()], false),
        ]
    );
}

/// One alias from two sources is offered once.
#[test]
fn an_alias_from_two_sources_is_offered_once() {
    let pack = Pack::new(
        manifest(PackKind::Shipped),
        vec![term("GLP-1", "open", Review::Reviewed)],
        vec![
            alias("GLP-1", "gee el pee one", "open", Review::Reviewed),
            alias("GLP-1", "gee el pee one", "public", Review::Reviewed),
        ],
    )
    .unwrap();
    assert_eq!(pack.offered()[0].aliases, ["gee el pee one"]);
    // Both sources are credited on the term, the alias's included.
    assert_eq!(pack.offered()[0].sources, ["open", "public"]);
}

/// The credits name every source, in id order, with its licence, address
/// and attribution text.
#[test]
fn the_notice_credits_every_source() {
    let notice = sample(PackKind::Shipped).manifest().notice();
    assert_eq!(
        notice,
        "pharmacy 2026.11 (en)\n\
         \n\
         The open list\n\
         Licence: CC0-1.0\n\
         From: https://example.org/open\n\
         Contains names from the open list.\n\
         \n\
         The public list\n\
         Licence: public domain\n\
         From: https://example.org/public\n\
         Contains names from the public list.\n"
    );
}

/// The library database, or any SQLite file that isn't a pack, is refused
/// as not a pack.
#[test]
fn a_file_that_is_not_a_pack_is_refused() {
    let dir = TestDir::new("not-a-pack");
    drop(crate::Store::open(&dir.db()).unwrap());
    assert!(matches!(Pack::open(&dir.db()), Err(PackError::NotAPack)));
}

/// A pack in a format this code doesn't know is refused, not misread:
/// a later one, or format 1, which had no digest.
#[test]
fn an_unknown_format_is_refused() {
    let dir = TestDir::new("pack-format");
    for format in [FORMAT + 1, 1] {
        let path = pack_path(&dir, &format!("format-{format}"));
        sample(PackKind::Shipped).write(&path).unwrap();
        raw(&path)
            .pragma_update(None, "user_version", format)
            .unwrap();
        let err = Pack::open(&path).unwrap_err();
        assert!(
            matches!(err, PackError::UnknownFormat(f) if f == format),
            "{err:?}"
        );
    }
}

/// What a refusal of one edit to a pack file must be.
type Expected = fn(&PackError) -> bool;

/// A file edited outside nota is checked as a built pack is: a row whose
/// licence was changed, a second manifest row, or an unknown status is
/// refused; so is a valid edit that leaves the stored digest behind, or a
/// stored digest changed alone.
#[test]
fn a_pack_edited_outside_nota_is_refused() {
    let dir = TestDir::new("pack-edited");
    let licence: Expected = |e| matches!(e, PackError::LicenceMismatch { .. });
    let corrupt: Expected = |e| matches!(e, PackError::Corrupt(_));
    let digest: Expected = |e| matches!(e, PackError::Corrupt(why) if why.contains("digest"));
    let edits = [
        // A pack Pack::new would accept, but not the one the digest is of.
        (
            "UPDATE term SET review = 'draft' WHERE term = 'GLP-1'",
            digest,
        ),
        ("UPDATE manifest SET kind = 'own'", digest),
        ("UPDATE manifest SET digest = zeroblob(32)", digest),
        (
            "UPDATE term SET licence = 'proprietary' WHERE term = 'GLP-1'",
            licence,
        ),
        (
            "INSERT INTO manifest VALUES ('other', '1', 'en', 'own', zeroblob(32))",
            corrupt,
        ),
        (
            "PRAGMA ignore_check_constraints = ON; UPDATE term SET review = 'checked'",
            corrupt,
        ),
    ];
    for (n, (edit, expected)) in edits.into_iter().enumerate() {
        let path = pack_path(&dir, &format!("edited-{n}"));
        sample(PackKind::Shipped).write(&path).unwrap();
        raw(&path).execute_batch(edit).unwrap();
        let err = Pack::open(&path).unwrap_err();
        assert!(expected(&err), "{edit}: {err:?}");
    }
}

/// A file whose rows come from a view, or from a table that isn't the
/// format's own, is refused before anything is read from it: a view could
/// run any SQL, for as long as it liked.
#[test]
fn a_pack_whose_tables_are_not_the_formats_is_refused() {
    let dir = TestDir::new("pack-view");
    let edits = [
        "ALTER TABLE term RENAME TO term_rows;
         CREATE VIEW term AS SELECT * FROM term_rows;",
        "ALTER TABLE alias ADD COLUMN extra TEXT",
        "DROP TABLE alias",
    ];
    for (n, edit) in edits.into_iter().enumerate() {
        let path = pack_path(&dir, &format!("view-{n}"));
        sample(PackKind::Shipped).write(&path).unwrap();
        raw(&path).execute_batch(edit).unwrap();
        let err = Pack::open(&path).unwrap_err();
        assert!(
            matches!(&err, PackError::Corrupt(why) if why.contains("tables")),
            "{edit}: {err:?}"
        );
    }
}

/// Rows past the limit are refused, not read; rows at it are read.
#[test]
fn rows_past_the_limit_are_refused() {
    assert!(!too_many(2, 1, 3));
    assert!(too_many(2, 2, 3));
    assert!(too_many(usize::MAX, 1, 3));
    let dir = TestDir::new("pack-limit");
    let path = pack_path(&dir, "limit");
    sample(PackKind::Shipped).write(&path).unwrap();
    let conn = raw(&path);
    // The sample has four term rows.
    let read = |max| {
        read_at_most(&conn, "SELECT term FROM term", max, |r| {
            r.get::<_, String>(0)
        })
    };
    assert_eq!(read(4).unwrap().len(), 4);
    assert!(matches!(read(3), Err(PackError::TooManyRows)));
}

/// A table a reader doesn't know, such as facts added in a later pack, is
/// ignored: adding one isn't a breaking change.
#[test]
fn a_table_the_reader_does_not_know_is_ignored() {
    let dir = TestDir::new("pack-facts");
    let path = pack_path(&dir, "with-facts");
    let pack = sample(PackKind::Shipped);
    pack.write(&path).unwrap();
    raw(&path)
        .execute_batch(
            "CREATE TABLE fact (term TEXT, fact TEXT);
             INSERT INTO fact VALUES ('amlodipine', 'calcium channel blocker');",
        )
        .unwrap();
    assert_eq!(Pack::open(&path).unwrap(), pack);
}

/// Writing over a file that holds something is refused: a build never
/// overwrites a pack.
#[test]
fn writing_over_a_pack_is_refused() {
    let dir = TestDir::new("pack-over");
    let path = pack_path(&dir, "pharmacy");
    sample(PackKind::Shipped).write(&path).unwrap();
    let before = bytes(&path);
    assert!(matches!(
        sample(PackKind::Own).write(&path),
        Err(PackError::Sqlite(_))
    ));
    assert_eq!(bytes(&path), before);
}

/// A missing file is an error, and nothing is created in its place.
#[test]
fn a_missing_pack_is_an_error() {
    let dir = TestDir::new("pack-missing");
    let path = pack_path(&dir, "none");
    assert!(matches!(Pack::open(&path), Err(PackError::Sqlite(_))));
    assert!(!path.exists());
}

/// Any review status.
fn any_review() -> impl Strategy<Value = Review> {
    prop_oneof![Just(Review::Reviewed), Just(Review::Draft)]
}

/// A term from a small pool, so terms and aliases often repeat, and one
/// term often comes from both sources.
fn any_word() -> impl Strategy<Value = String> {
    proptest::sample::select(vec!["a", "ab", "b c", "ca", "GLP-1", "c"]).prop_map(str::to_owned)
}

/// Term rows and alias rows of those terms, from either source, with no
/// row given twice under two statuses.
fn any_rows() -> impl Strategy<Value = (Vec<TermRow>, Vec<AliasRow>)> {
    let source = prop_oneof![Just("open"), Just("public")];
    let terms = proptest::collection::btree_map((any_word(), source.clone()), any_review(), 1..12);
    let aliases = proptest::collection::vec(
        (
            any::<prop::sample::Index>(),
            any_word(),
            source,
            any_review(),
        ),
        0..12,
    );
    (terms, aliases).prop_map(|(terms, aliases)| {
        let terms: Vec<TermRow> = terms
            .into_iter()
            .map(|((t, s), r)| term(&t, s, r))
            .collect();
        // One status per row: a row given twice keeps its first.
        let mut seen = BTreeSet::new();
        let aliases = aliases
            .into_iter()
            .filter_map(|(i, a, s, r)| {
                let t = &i.get(&terms).term;
                seen.insert((t.clone(), a.clone(), s))
                    .then(|| alias(t, &a, s, r))
            })
            .collect();
        (terms, aliases)
    })
}

/// Writes `bytes` to `path`, as a file damaged outside nota would be.
#[expect(
    clippy::disallowed_methods,
    reason = "test scaffolding: a damaged pack file, outside the recorder's write path"
)]
fn damage(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// A pack file with any bytes changed is read as a valid pack or
    /// refused with an error, never a panic: a pack read back is one
    /// [`Pack::new`] accepts.
    #[test]
    fn a_damaged_pack_is_read_whole_or_refused(
        changes in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..8),
    ) {
        let dir = TestDir::new("pack-damaged");
        let path = pack_path(&dir, "p");
        sample(PackKind::Shipped).write(&path).unwrap();
        let mut file = bytes(&path);
        for (at, byte) in &changes {
            let i = at.index(file.len());
            file[i] = *byte;
        }
        let damaged = pack_path(&dir, "damaged");
        damage(&damaged, &file);
        if let Ok(pack) = Pack::open(&damaged) {
            let again = Pack::new(
                pack.manifest().clone(),
                pack.terms().to_vec(),
                pack.aliases().to_vec(),
            );
            prop_assert_eq!(again.ok(), Some(pack));
        }
    }

    /// Any valid pack round-trips through a file unchanged.
    #[test]
    fn any_pack_round_trips((terms, aliases) in any_rows(), own in any::<bool>()) {
        let kind = if own { PackKind::Own } else { PackKind::Shipped };
        let pack = Pack::new(manifest(kind), terms, aliases).unwrap();
        let dir = TestDir::new("pack-prop");
        let path = pack_path(&dir, "p");
        pack.write(&path).unwrap();
        prop_assert_eq!(Pack::open(&path).unwrap(), pack);
    }

    /// From a shipped pack, a term is offered exactly when one of its rows
    /// is reviewed, and an alias exactly when it's reviewed and its term is
    /// offered; the order rows are given in changes nothing.
    #[test]
    fn a_shipped_pack_offers_only_reviewed_rows((terms, aliases) in any_rows()) {
        let pack = Pack::new(manifest(PackKind::Shipped), terms.clone(), aliases.clone()).unwrap();
        let offered = pack.offered();
        let reviewed_terms: BTreeSet<&str> = terms
            .iter()
            .filter(|r| r.review == Review::Reviewed)
            .map(|r| r.term.as_str())
            .collect();
        let offered_terms: BTreeSet<&str> = offered.iter().map(|o| o.term.as_str()).collect();
        prop_assert_eq!(&offered_terms, &reviewed_terms);
        for o in &offered {
            let want: BTreeSet<&str> = aliases
                .iter()
                .filter(|a| a.term == o.term && a.review == Review::Reviewed)
                .map(|a| a.alias.as_str())
                .collect();
            let got: BTreeSet<&str> = o.aliases.iter().map(String::as_str).collect();
            prop_assert_eq!(got, want);
            prop_assert!(o.reviewed);
        }
        let mut reversed_terms = terms;
        reversed_terms.reverse();
        let mut reversed_aliases = aliases;
        reversed_aliases.reverse();
        let reversed =
            Pack::new(manifest(PackKind::Shipped), reversed_terms, reversed_aliases).unwrap();
        prop_assert_eq!(reversed.offered(), offered);
    }
}
