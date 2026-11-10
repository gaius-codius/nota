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

    let (a, b) = (pack_path(&dir, "a"), pack_path(&dir, "b"));
    pack.write(&a).unwrap();
    shuffled.write(&b).unwrap();
    assert_eq!(bytes(&a), bytes(&b));
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
    let offered = sample(PackKind::Shipped).offered();
    let pack = PackRef {
        name: "pharmacy".to_owned(),
        version: "2026.11".to_owned(),
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

/// A pack in a format this code doesn't know is refused, not misread.
#[test]
fn an_unknown_format_is_refused() {
    let dir = TestDir::new("pack-format");
    let path = pack_path(&dir, "future");
    sample(PackKind::Shipped).write(&path).unwrap();
    raw(&path).pragma_update(None, "user_version", 2).unwrap();
    assert!(matches!(
        Pack::open(&path),
        Err(PackError::UnknownFormat(2))
    ));
}

/// A file edited outside nota is checked as a built pack is: a row whose
/// licence was changed, a second manifest row, or an unknown status is
/// refused.
#[test]
fn a_pack_edited_outside_nota_is_refused() {
    let dir = TestDir::new("pack-edited");
    let edits = [
        "UPDATE term SET licence = 'proprietary' WHERE term = 'GLP-1'",
        "INSERT INTO manifest VALUES ('other', '1', 'en', 'own')",
        "PRAGMA ignore_check_constraints = ON; UPDATE term SET review = 'checked'",
    ];
    for (n, edit) in edits.into_iter().enumerate() {
        let path = pack_path(&dir, &format!("edited-{n}"));
        sample(PackKind::Shipped).write(&path).unwrap();
        raw(&path).execute_batch(edit).unwrap();
        let err = Pack::open(&path).unwrap_err();
        assert!(
            matches!(
                err,
                PackError::LicenceMismatch { .. } | PackError::Corrupt(_)
            ),
            "{edit}: {err:?}"
        );
    }
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

/// A short term from a small alphabet, so terms and aliases repeat.
fn any_word() -> impl Strategy<Value = String> {
    "[a-c]{1,3}( [a-c]{1,2})?"
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

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
