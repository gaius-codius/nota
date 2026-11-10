use std::collections::BTreeSet;
use std::ffi::OsString;

use nota_core::SessionId;
use nota_store::{NewSession, Store};
use proptest::prelude::*;

use super::*;

/// A fresh directory under the system temp dir, removed when dropped.
struct TestDir(PathBuf);

impl TestDir {
    #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nota-pack-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// Writes `text` to the file `name` in the directory.
    #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
    fn file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for TestDir {
    #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The manifest a glossary by Sam gets.
fn about() -> GlossaryAbout {
    GlossaryAbout {
        name: "cardio".to_owned(),
        version: "1".to_owned(),
        language: "en".to_owned(),
        author: "Sam".to_owned(),
    }
}

/// `words` as command-line arguments.
fn args(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

/// The repository's pharmacy pack directory.
fn pharmacy_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs/pharmacy")
}

/// The file's bytes.
fn bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

/// Acceptance (#154): a plain-text glossary builds into a pack whose terms
/// and aliases read back from its file, as the author's unreviewed rows,
/// and the pack is offered for a session that chooses it.
#[test]
fn a_glossary_builds_into_a_pack_a_session_is_offered() {
    let dir = TestDir::new("glossary");
    let text = "# cardiology terms\n\
                GLP-1\tgee el pee one\tglp one\n\
                \n\
                orthopnoea\n\
                Naranjo\tnaranjo scale\n";
    let path = dir.0.join("cardio.pack");
    glossary(text, &about()).unwrap().write(&path).unwrap();
    let pack = Pack::open(&path).unwrap();
    let terms: Vec<_> = pack.terms().iter().map(|r| r.term.as_str()).collect();
    assert_eq!(terms, ["GLP-1", "Naranjo", "orthopnoea"]);
    let aliases: Vec<_> = pack
        .aliases()
        .iter()
        .map(|r| (r.term.as_str(), r.alias.as_str()))
        .collect();
    assert_eq!(
        aliases,
        [
            ("GLP-1", "gee el pee one"),
            ("GLP-1", "glp one"),
            ("Naranjo", "naranjo scale")
        ]
    );
    // The author's rows: their source, their own licence, not reviewed.
    let m = pack.manifest();
    assert_eq!((m.name.as_str(), m.kind), ("cardio", PackKind::Own));
    assert_eq!(m.sources[0].name, "Sam");
    assert!(
        pack.terms()
            .iter()
            .all(|r| r.source == AUTHOR && r.licence == OWN_LICENCE && r.review == Review::Draft)
    );

    // A session that chooses it is offered its terms, as unreviewed.
    let library = dir.0.join("library.db");
    let mut store = Store::open(&library).unwrap();
    let session = SessionId::new(1);
    store.create_session(&NewSession::bare(session)).unwrap();
    store
        .record_session_packs(session, &[m.pack_ref()])
        .unwrap();
    let offered = store.session_terms(session, &[pack]).unwrap().terms;
    let summary: Vec<_> = offered
        .iter()
        .map(|o| (o.term.as_str(), o.aliases.len(), o.reviewed))
        .collect();
    assert_eq!(
        summary,
        [
            ("GLP-1", 2, false),
            ("Naranjo", 1, false),
            ("orthopnoea", 0, false)
        ]
    );
}

/// A glossary's fields lose the spaces at their ends, empty fields and
/// repeats are dropped, and a comment may be indented.
#[test]
fn a_glossary_is_forgiving_about_spacing_and_repeats() {
    let text = "  # indented comment\n\
                \x20amlodipine \t\t amlo \n\
                amlodipine\tamlo\n\
                \t\t\n";
    let pack = glossary(text, &about()).unwrap();
    let terms: Vec<_> = pack.terms().iter().map(|r| r.term.as_str()).collect();
    assert_eq!(terms, ["amlodipine"]);
    let aliases: Vec<_> = pack.aliases().iter().map(|r| r.alias.as_str()).collect();
    assert_eq!(aliases, ["amlo"]);
}

/// A glossary with nothing in it still builds, as an empty pack; one with
/// a control character in a term is refused.
#[test]
fn a_glossary_term_with_a_control_character_is_refused() {
    assert!(
        glossary("# only a comment\n", &about())
            .unwrap()
            .terms()
            .is_empty()
    );
    let err = glossary("amlo\u{7}dipine\n", &about()).unwrap_err();
    assert!(
        matches!(err, PackError::BadText { field: "term", .. }),
        "{err:?}"
    );
}

/// A shipped pack builds from its directory's `pack.toml` and lists: the
/// rows take their source's id and licence, and an alias row names its
/// term.
#[test]
fn a_directory_of_lists_builds_a_pack() {
    let dir = TestDir::new("build");
    dir.file(
        "pack.toml",
        r#"
name = "test"
version = "1"
language = "en"
kind = "shipped"

[[source]]
id = "open"
name = "An open list"
licence = "CC0-1.0"
attribution = "From an open list."
url = "https://example.org/open"
lists = ["open.tsv"]
"#,
    );
    dir.file(
        "open.tsv",
        "# term, status, alias\namlodipine\treviewed\nGLP-1\treviewed\nGLP-1\treviewed\tgee el pee one\nsema\tdraft\n",
    );
    let pack = build_dir(&dir.0).unwrap();
    let rows: Vec<_> = pack
        .terms()
        .iter()
        .map(|r| {
            (
                r.term.as_str(),
                r.source.as_str(),
                r.licence.as_str(),
                r.review,
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("GLP-1", "open", "CC0-1.0", Review::Reviewed),
            ("amlodipine", "open", "CC0-1.0", Review::Reviewed),
            ("sema", "open", "CC0-1.0", Review::Draft),
        ]
    );
    assert_eq!(pack.aliases()[0].alias, "gee el pee one");
    assert_eq!(
        pack.manifest().sources[0].url.as_deref(),
        Some("https://example.org/open")
    );
}

/// A source with no licence fails the build, so a row can't be built
/// without one.
#[test]
fn a_source_without_a_licence_fails_the_build() {
    let dir = TestDir::new("no-licence");
    dir.file(
        "pack.toml",
        "name = \"t\"\nversion = \"1\"\nlanguage = \"en\"\nkind = \"shipped\"\n\
         [[source]]\nid = \"open\"\nname = \"Open\"\nattribution = \"A.\"\nlists = [\"a.tsv\"]\n",
    );
    dir.file("a.tsv", "amlodipine\treviewed\n");
    let err = build_dir(&dir.0).unwrap_err();
    assert!(err.ends_with("licence is missing"), "{err}");
    dir.file(
        "pack.toml",
        "name = \"t\"\nversion = \"1\"\nlanguage = \"en\"\nkind = \"shipped\"\n\
         [[source]]\nid = \"open\"\nname = \"Open\"\nlicence = \"\"\nattribution = \"A.\"\n\
         lists = [\"a.tsv\"]\n",
    );
    let err = build_dir(&dir.0).unwrap_err();
    assert!(err.contains("the source licence \"\" is empty"), "{err}");
}

/// A list row that isn't a term and a status, with maybe an alias, is
/// refused with its line's number.
#[test]
fn a_bad_list_row_is_refused_with_its_line() {
    let source = Source {
        id: "open".to_owned(),
        name: "Open".to_owned(),
        licence: "CC0-1.0".to_owned(),
        attribution: "A.".to_owned(),
        url: None,
    };
    let cases = [
        (
            "amlodipine\n",
            "1: a row is a term, a status and maybe an alias",
        ),
        (
            "# c\namlodipine\tchecked\n",
            "2: \"checked\" is not reviewed or draft",
        ),
        (
            "a\treviewed\tb\tc\n",
            "1: a row is a term, a status and maybe an alias",
        ),
    ];
    for (text, want) in cases {
        assert_eq!(parse_list(text, &source).unwrap_err(), want, "{text:?}");
    }
}

/// `nota pack`'s arguments parse into its commands, with a glossary's
/// defaults, and missing or unknown options are refused.
#[test]
fn pack_arguments_parse() {
    assert_eq!(
        parse(&args(&["build", "packs/pharmacy", "--out", "p.pack"])).unwrap(),
        PackCommand::Build {
            dir: PathBuf::from("packs/pharmacy"),
            out: PathBuf::from("p.pack"),
        }
    );
    assert_eq!(
        parse(&args(&[
            "glossary",
            "my/cardio.txt",
            "--out",
            "c.pack",
            "--author",
            "Sam"
        ]))
        .unwrap(),
        PackCommand::Glossary {
            file: PathBuf::from("my/cardio.txt"),
            out: PathBuf::from("c.pack"),
            about: about(),
        }
    );
    assert_eq!(
        parse(&args(&["notice", "p.pack"])).unwrap(),
        PackCommand::Notice {
            pack: PathBuf::from("p.pack")
        }
    );
    for (words, want) in [
        (&["build", "d"][..], "--out is required"),
        (
            &["glossary", "g.txt", "--out", "o"][..],
            "--author is required",
        ),
        (
            &["build", "d", "--out", "o", "--fast", "x"][..],
            "unknown option --fast",
        ),
        (&["show"][..], "pack takes build, glossary or notice"),
    ] {
        assert_eq!(parse(&args(words)).unwrap_err(), want, "{words:?}");
    }
}

/// `nota pack notice` writes the pack's credits; a build says what it made.
#[test]
fn notice_writes_the_credits() {
    let dir = TestDir::new("notice");
    let path = dir.0.join("cardio.pack");
    let file = dir.file("cardio.txt", "GLP-1\tgee el pee one\n");
    let mut out = Vec::new();
    let command = PackCommand::Glossary {
        file,
        out: path.clone(),
        about: about(),
    };
    run(&command, &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "cardio 1: 1 term rows, 1 alias rows, 1 offered terms\n  Sam (the author's own)\n"
    );
    let mut out = Vec::new();
    run(&PackCommand::Notice { pack: path }, &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "cardio 1 (en)\n\nSam\nLicence: the author's own\nWritten by Sam.\n"
    );
}

/// Acceptance (#154): the pharmacy pack builds from the committed lists
/// alone, and two builds give the same bytes; every source's credits are
/// generated.
#[test]
fn the_pharmacy_pack_builds_the_same_bytes_twice() {
    let dir = TestDir::new("pharmacy");
    let (a, b) = (dir.0.join("a.pack"), dir.0.join("b.pack"));
    for out in [&a, &b] {
        let command = PackCommand::Build {
            dir: pharmacy_dir(),
            out: out.clone(),
        };
        run(&command, &mut Vec::new()).unwrap();
    }
    assert_eq!(bytes(&a), bytes(&b));
    let pack = Pack::open(&a).unwrap();
    assert_eq!(pack.manifest().name, "pharmacy");
    let notice = pack.manifest().notice();
    for source in &pack.manifest().sources {
        assert!(notice.contains(&source.attribution), "{}", source.id);
        // Every source gives rows.
        assert!(
            pack.terms().iter().any(|r| r.source == source.id),
            "{}",
            source.id
        );
    }
}

proptest! {
    /// Any text gives a glossary pack or a refusal, never a panic; a pack
    /// it gives has exactly the lines' first fields as its terms.
    #[test]
    fn any_glossary_text_builds_or_is_refused(text in "[a-c #\t\n\u{7}]{0,60}") {
        if let Ok(pack) = glossary(&text, &about()) {
            let want: BTreeSet<String> = text
                .lines()
                .filter(|l| !l.trim_start().starts_with('#'))
                .filter_map(|l| l.split('\t').map(str::trim).find(|f| !f.is_empty()))
                .map(str::to_owned)
                .collect();
            let got: BTreeSet<String> = pack.terms().iter().map(|r| r.term.clone()).collect();
            prop_assert_eq!(got, want);
        }
    }
}
