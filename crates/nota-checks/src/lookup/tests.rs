//! C4's rules one by one, and its scores and candidates against the bench's
//! Python on the committed fixture of invented inputs.

use super::*;
use crate::Zipf;
use crate::language::english;
use crate::phonetic::Metaphone;
use crate::words::tests::fixture;

/// How far a score part may be from Python's: the same operations in the
/// same order give the same float, and JSON round-trips it.
const TOLERANCE: f64 = 1e-12;

/// `t` as an English term with `aliases`.
fn term(t: &str, aliases: &[&str]) -> Term {
    Term {
        term: t.to_owned(),
        aliases: aliases.iter().map(|&a| a.to_owned()).collect(),
        language: "en".to_owned(),
    }
}

/// Every score part of every fixture pair agrees with the bench's: how alike
/// they sound (Double Metaphone), trigram Jaccard, Jaro-Winkler and the
/// total.
#[test]
fn every_fixture_score_has_the_bench_parts() {
    let fixture = fixture();
    let pairs = fixture["scores"].as_array().expect("a score list");
    let mut wrong = Vec::new();
    for p in pairs {
        let window = p["window"].as_str().expect("a window");
        let t = p["term"].as_str().expect("a term");
        let entry = Entry::new(english(), t);
        let letters = letters(window);
        let got = score(
            &english().sounds(window),
            &Trigrams::of(&letters),
            &letters,
            &entry,
        );
        let want = |k: &str| p[k].as_f64().expect("a number");
        for (name, got, want) in [
            ("sounds", got.sounds, want("phonetic")),
            ("trigrams", got.trigrams, want("trigram")),
            ("jaro_winkler", got.jaro_winkler, want("jaro_winkler")),
            ("total", got.total, want("score")),
        ] {
            if (got - want).abs() > TOLERANCE {
                wrong.push(format!(
                    "{window:?} / {t:?} {name}: got {got}, bench {want}"
                ));
            }
        }
    }
    assert!(pairs.len() >= 700, "the fixture has {} pairs", pairs.len());
    assert_eq!(wrong, Vec::<String>::new());
}

/// A language with set Zipf values (2.0, neither everyday nor rare, for any
/// word not given) and English's sounds, so each rule's bar can be met
/// exactly.
#[derive(Debug)]
struct Fake {
    /// Each word's Zipf value, lower-case.
    zipf: Vec<(&'static str, f64)>,
    /// English Double Metaphone.
    metaphone: Metaphone,
}

impl Fake {
    /// The language with `zipf` for the words it names.
    fn new(zipf: &[(&'static str, f64)]) -> Self {
        Self {
            zipf: zipf.to_vec(),
            metaphone: Metaphone::new(),
        }
    }
}

impl Language for Fake {
    fn code(&self) -> &'static str {
        "en"
    }

    fn zipf(&self, word: &str) -> Zipf {
        let word = word.to_lowercase();
        let value = self
            .zipf
            .iter()
            .find(|(w, _)| *w == word)
            .map_or(2.0, |&(_, z)| z);
        Zipf::new(value)
    }

    fn sounds(&self, text: &str) -> Sounds {
        self.metaphone.sounds(text)
    }

    fn may_open_on(&self, word: &str) -> bool {
        word == "a"
    }
}

/// Every window and term `terms` offers in `text` in `language`, in order.
fn offered(language: &dyn Language, terms: &[Term], text: &str) -> Vec<(String, String)> {
    Lookup::new(language, terms)
        .find(text, &|_, _| true)
        .iter()
        .map(|c| (c.window().to_owned(), c.term().to_owned()))
        .collect()
}

/// `(window, term)` pairs, owned, to compare with [`offered`].
fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|&(w, t)| (w.to_owned(), t.to_owned()))
        .collect()
}

/// A window of four letters is never offered, even as an exact alias; one
/// of five is.
#[test]
fn a_window_under_five_letters_is_never_offered() {
    let terms = [term("amlodipine", &["a dlp", "a dlp o"])];
    let fake = Fake::new(&[]);
    assert_eq!(offered(&fake, &terms, "a dlp"), pairs(&[]));
    assert_eq!(
        offered(&fake, &terms, "a dlp o"),
        pairs(&[("a dlp o", "amlodipine")])
    );
}

/// One word at Zipf 3.3 or more is an everyday word, and skipped.
#[test]
fn one_everyday_word_is_skipped() {
    let terms = [term("amlodipine", &[])];
    let everyday = Fake::new(&[("amlodipin", EVERYDAY_ZIPF)]);
    assert_eq!(offered(&everyday, &terms, "amlodipin"), pairs(&[]));
    let rarer = Fake::new(&[("amlodipin", 3.29)]);
    assert_eq!(
        offered(&rarer, &terms, "amlodipin"),
        pairs(&[("amlodipin", "amlodipine")])
    );
}

/// Several words each at Zipf 5.0 or more, none longer than 4 characters,
/// are skipped; one rarer or longer word lets the window through.
#[test]
fn common_short_words_together_are_skipped() {
    let terms = [term("beta blocker", &[])];
    let common = Fake::new(&[
        ("beta", COMMON_ZIPF),
        ("bluk", COMMON_ZIPF),
        ("blukk", COMMON_ZIPF),
    ]);
    assert_eq!(offered(&common, &terms, "beta bluk"), pairs(&[]));
    let rarer = Fake::new(&[("beta", COMMON_ZIPF), ("bluk", 4.99)]);
    assert_eq!(
        offered(&rarer, &terms, "beta bluk"),
        pairs(&[("beta bluk", "beta blocker")])
    );
    assert_eq!(
        offered(&common, &terms, "beta blukk"),
        pairs(&[("beta blukk", "beta blocker")])
    );
}

/// A window of several words may not start on a function word (Zipf 6.3 or
/// more) other than "a", nor end on any.
#[test]
fn a_window_may_not_start_or_end_on_a_function_word() {
    let terms = [term("amlodipine", &[])];
    // The single word is an everyday word, so only the pair can be offered.
    let at = |the: f64, a: f64| Fake::new(&[("amlodipin", EVERYDAY_ZIPF), ("the", the), ("a", a)]);
    assert_eq!(
        offered(&at(FUNCTION_ZIPF, 7.0), &terms, "the amlodipin"),
        pairs(&[])
    );
    assert_eq!(
        offered(&at(6.29, 7.0), &terms, "the amlodipin"),
        pairs(&[("the amlodipin", "amlodipine")])
    );
    assert_eq!(
        offered(&at(6.29, 7.0), &terms, "amlodipin the"),
        pairs(&[("amlodipin the", "amlodipine")])
    );
    assert_eq!(
        offered(&at(FUNCTION_ZIPF, 7.0), &terms, "amlodipin the"),
        pairs(&[])
    );
    // "a" may start a window ("a bloody pine"), but not end one.
    assert_eq!(
        offered(&at(7.0, 7.0), &terms, "a amlodipin"),
        pairs(&[("a amlodipin", "amlodipine")])
    );
    assert_eq!(offered(&at(7.0, 7.0), &terms, "amlodipin a"), pairs(&[]));
}

/// A window scoring between 0.42 and 0.58 is kept only when it has a
/// nearly unknown word (Zipf under 1.5), and says so.
#[test]
fn a_nearly_unknown_word_lowers_the_bar() {
    let terms = [term("amlodipine", &[])];
    let lookup_at = |zipf: f64| Fake::new(&[("bloody", zipf), ("pine", zipf)]);
    let rare = lookup_at(1.49);
    let found = Lookup::new(&rare, &terms).find("bloody pine", &|_, _| true);
    let [found] = &found[..] else {
        panic!("{found:?}")
    };
    assert_eq!(
        (found.window(), found.term()),
        ("bloody pine", "amlodipine")
    );
    let Matched::Scored(score) = found.matched() else {
        panic!("{found:?}")
    };
    assert!(score.total > KEEP_RARE && score.total < KEEP, "{score:?}");
    assert_eq!(score.bar, Bar::Rare);
    assert_eq!(
        offered(&lookup_at(RARE_ZIPF), &terms, "bloody pine"),
        pairs(&[])
    );
}

/// A window over the usual bar says it cleared that one.
#[test]
fn a_usual_window_clears_the_usual_bar() {
    let terms = [term("amlodipine", &[])];
    let found = Lookup::new(&Fake::new(&[]), &terms).find("amlodipin", &|_, _| true);
    let [found] = &found[..] else {
        panic!("{found:?}")
    };
    let Matched::Scored(score) = found.matched() else {
        panic!("{found:?}")
    };
    assert_eq!(score.bar, Bar::Usual);
    assert!(score.total >= KEEP, "{score:?}");
}

/// A window with a word that is a term, or that is a term, is already
/// right: nothing is offered for it.
#[test]
fn a_window_with_a_known_term_is_not_offered() {
    let terms = [
        term("amlodipine", &[]),
        term("ibuprofen", &[]),
        term("beta blocker", &[]),
    ];
    let fake = Fake::new(&[("blocker", EVERYDAY_ZIPF)]);
    assert_eq!(
        offered(&fake, &terms, "ibuprofen amlodipin"),
        pairs(&[("amlodipin", "amlodipine")])
    );
    assert_eq!(offered(&fake, &terms, "beta blocker"), pairs(&[]));
}

/// A term is scored only against windows within reach of its length: 4
/// letters either way, or half its own, whichever is more.
#[test]
fn a_term_is_scored_only_within_reach_of_its_length() {
    let ten = "abcdefghij";
    assert!(within_reach(&"x".repeat(15), ten));
    assert!(!within_reach(&"x".repeat(16), ten));
    assert!(within_reach(&"x".repeat(5), ten));
    assert!(!within_reach(&"x".repeat(4), ten));
    assert!(within_reach(&"x".repeat(9), "abcde"));
    assert!(!within_reach(&"x".repeat(10), "abcde"));
}

/// Windows that share a word aren't both offered; the better stays.
/// Windows side by side are.
#[test]
fn windows_sharing_a_word_are_not_both_offered() {
    let terms = [term("amlodipine", &[]), term("ibuprofen", &[])];
    let fake = Fake::new(&[]);
    assert_eq!(
        offered(&fake, &terms, "amlo dipin"),
        pairs(&[("amlo dipin", "amlodipine")])
    );
    assert_eq!(
        offered(&fake, &terms, "ibuprofin amlodipin"),
        pairs(&[("amlodipin", "amlodipine"), ("ibuprofin", "ibuprofen")])
    );
}

/// No more than six candidates are offered for a chunk, the best first.
#[test]
fn at_most_six_candidates_are_offered() {
    let names = [
        "amlodipine",
        "ibuprofen",
        "nifedipine",
        "bisoprolol",
        "atorvastatin",
        "metformin",
        "ramipril",
        "warfarin",
    ];
    let terms: Vec<Term> = names.iter().map(|n| term(n, &[])).collect();
    // Each term with its last letter swapped: eight good windows.
    let text = names
        .iter()
        .map(|n| format!("{}x", &n[..n.len() - 1]))
        .collect::<Vec<_>>()
        .join(" ");
    let found = Lookup::new(&Fake::new(&[]), &terms).find(&text, &|_, _| true);
    assert_eq!(found.len(), MAX_CANDIDATES, "{found:?}");
    let totals: Vec<f64> = found
        .iter()
        .map(|c| match c.matched() {
            Matched::Scored(s) => s.total,
            Matched::Alias => panic!("{c:?}"),
        })
        .collect();
    assert!(totals.is_sorted_by(|a, b| a >= b), "{totals:?}");
}

/// Two terms that score the same for a window: the one that sorts first is
/// offered, in whatever order the terms come.
#[test]
fn a_tie_goes_to_the_term_that_sorts_first() {
    // Double Metaphone drops digits, and the window has neither, so the two
    // tie on every part of the score.
    let one = term("type 1 diabetes", &[]);
    let two = term("type 2 diabetes", &[]);
    let fake = Fake::new(&[("diabetes", EVERYDAY_ZIPF)]);
    let want = pairs(&[("taip diabetes", "type 1 diabetes")]);
    assert_eq!(
        offered(&fake, &[one.clone(), two.clone()], "taip diabetes"),
        want
    );
    assert_eq!(offered(&fake, &[two, one], "taip diabetes"), want);
}

/// The term that scores best but that the user rejected for the window is
/// passed over for the next best.
#[test]
fn a_rejected_term_is_passed_over_for_the_next_best() {
    let terms = [term("amlodipine", &[]), term("felodipine", &[])];
    let fake = Fake::new(&[]);
    let lookup = Lookup::new(&fake, &terms);
    let found = |allows: &dyn Fn(&str, &str) -> bool| -> Vec<String> {
        lookup
            .find("felodipin", allows)
            .iter()
            .map(|c| c.term().to_owned())
            .collect()
    };
    // 0.93 against felodipine, 0.63 against amlodipine.
    assert_eq!(found(&|_, _| true), ["felodipine"]);
    assert_eq!(
        found(&|w, t| !(w == "felodipin" && t == "felodipine")),
        ["amlodipine"]
    );
    assert_eq!(found(&|_, _| false), Vec::<String>::new());
}

/// Invented text in the shape of a workshop line, with "a dlp one" where
/// "amlodipine" was said.
const DLP_ONE: &str = "We know for example with a dlp one that nocturnal doses help.";

/// Invented text with "endside" where "NSAID" was said.
const ENDSIDE: &str = "Then we have endside fluid retention most days.";

/// The bench's known miss "a dlp one" isn't found by sound alone, and is
/// once amlodipine has it as an alias: an exact match isn't held to the
/// window rules (it ends on the function word "one").
#[test]
fn a_dlp_one_is_found_once_its_alias_exists() {
    let terms_for = |aliases: &[&str]| [term("amlodipine", aliases), term("ibuprofen", &[])];
    let without = offered(english(), &terms_for(&[]), DLP_ONE);
    assert!(
        without.iter().all(|(_, t)| t != "amlodipine"),
        "{without:?}"
    );
    let found = Lookup::new(english(), &terms_for(&["a dlp one"])).find(DLP_ONE, &|_, _| true);
    let [found] = &found[..] else {
        panic!("{found:?}")
    };
    assert_eq!(
        (found.window(), found.term(), found.matched()),
        ("a dlp one", "amlodipine", Matched::Alias)
    );
    assert_eq!(&DLP_ONE[found.span()], "a dlp one");
}

/// "endside", a known miss of the bench's, is found as an exact match once
/// the user has accepted NSAID for it: a learned pair is the term's alias.
/// (With only these terms it scores past the nearly-unknown bar too; the
/// exact match doesn't depend on that.)
#[test]
fn endside_is_found_once_its_learned_pair_exists() {
    let terms = [term("NSAID", &["endside"]), term("ibuprofen", &[])];
    let found = Lookup::new(english(), &terms).find(ENDSIDE, &|_, _| true);
    let [found] = &found[..] else {
        panic!("{found:?}")
    };
    assert_eq!(
        (found.window(), found.term(), found.matched()),
        ("endside", "NSAID", Matched::Alias)
    );
}

/// An alias matches by letters and digits alone, whatever the spacing,
/// case and punctuation of the heard words.
#[test]
fn an_alias_matches_by_letters_and_digits() {
    let terms = [term("NSAID", &["en said"])];
    let fake = Fake::new(&[]);
    assert_eq!(
        offered(&fake, &terms, "the ENSAID, again"),
        pairs(&[("ENSAID", "NSAID")])
    );
    assert_eq!(
        offered(&fake, &terms, "an en-said"),
        pairs(&[("en-said", "NSAID")])
    );
}

/// An exact match is offered ahead of a better-scored window it overlaps.
#[test]
fn an_exact_match_comes_before_any_score() {
    let terms = [
        term("amlodipine", &[]),
        term("nifedipine", &["amlodipin pills"]),
    ];
    let fake = Fake::new(&[]);
    assert_eq!(
        offered(&fake, &terms, "amlodipin"),
        pairs(&[("amlodipin", "amlodipine")])
    );
    assert_eq!(
        offered(&fake, &terms, "amlodipin pills"),
        pairs(&[("amlodipin pills", "nifedipine")])
    );
}

/// An alias of up to eight words is matched, past the three a scored window
/// may have; one of nine never is.
#[test]
fn an_alias_of_up_to_eight_words_is_matched() {
    let eight = "one two three four five six seven eight";
    let nine = "one two three four five six seven eight nine";
    let fake = Fake::new(&[]);
    assert_eq!(
        offered(&fake, &[term("octet", &[eight])], eight),
        pairs(&[(eight, "octet")])
    );
    // Nine words are never matched, even when heard as eight.
    let heard_as_eight = "one two three four five six seven eightnine";
    assert_eq!(offered(&fake, &[term("nonet", &[nine])], nine), pairs(&[]));
    assert_eq!(
        offered(&fake, &[term("nonet", &[nine])], heard_as_eight),
        pairs(&[])
    );
}

/// An alias spelled like a term is never matched: it would offer one term
/// for another that's already right.
#[test]
fn an_alias_spelled_like_a_term_is_ignored() {
    let terms = [term("amlodipine", &[]), term("nifedipine", &["Amlodipine"])];
    assert_eq!(offered(&Fake::new(&[]), &terms, "amlodipine"), pairs(&[]));
}

/// An alias of two terms offers the one that sorts first that the user
/// hasn't rejected for it.
#[test]
fn an_alias_of_two_terms_offers_the_first_allowed() {
    let terms = [
        term("nifedipine", &["dipin pill"]),
        term("felodipine", &["dipin pill"]),
    ];
    let fake = Fake::new(&[]);
    let lookup = Lookup::new(&fake, &terms);
    let term_for = |allows: &dyn Fn(&str, &str) -> bool| -> Vec<String> {
        lookup
            .find("dipin pill", allows)
            .iter()
            .map(|c| c.term().to_owned())
            .collect()
    };
    assert_eq!(term_for(&|_, _| true), ["felodipine"]);
    assert_eq!(term_for(&|_, t| t != "felodipine"), ["nifedipine"]);
}

/// A term listed twice (from two packs, say) is one term with the aliases
/// of both.
#[test]
fn a_term_listed_twice_has_both_lists_of_aliases() {
    let terms = [
        term("NSAID", &["endside"]),
        term("ibuprofen", &[]),
        term("NSAID", &["en said"]),
    ];
    let fake = Fake::new(&[]);
    assert_eq!(
        offered(&fake, &terms, "endside or en said"),
        pairs(&[("endside", "NSAID"), ("en said", "NSAID")])
    );
}

/// Terms in another language are left out; a regional English one isn't.
#[test]
fn terms_in_another_language_are_left_out() {
    let mut french = term("amlodipine", &[]);
    "fr".clone_into(&mut french.language);
    let mut british = term("ibuprofen", &[]);
    "EN-gb".clone_into(&mut british.language);
    let terms = [french, british];
    assert_eq!(
        offered(&Fake::new(&[]), &terms, "amlodipin ibuprofin"),
        pairs(&[("ibuprofin", "ibuprofen")])
    );
    assert!(same_language("en_GB", "en"));
    assert!(!same_language("", "en"));
    assert!(!same_language("eng", "en"));
}

/// A term with no letter or digit is never offered, and doesn't make a
/// lone hyphen a known word.
#[test]
fn a_term_with_no_letters_is_ignored() {
    let terms = [term("+++", &[]), term("amlodipine", &[])];
    assert_eq!(
        offered(&Fake::new(&[]), &terms, "amlo - dipin"),
        pairs(&[("amlo - dipin", "amlodipine")])
    );
}

/// A candidate's window is its words joined by one space, and its span runs
/// from its first word's start to its last word's end in the chunk.
#[test]
fn a_candidates_window_and_span_are_its_words() {
    let terms = [term("amlodipine", &[])];
    let text = "Start  amlo,  dipin daily";
    let found = Lookup::new(&Fake::new(&[]), &terms).find(text, &|_, _| true);
    let [found] = &found[..] else {
        panic!("{found:?}")
    };
    assert_eq!(found.window(), "amlo dipin");
    assert_eq!(found.span(), 7..19);
    assert_eq!(&text[found.span()], "amlo,  dipin");
}

/// Words are runs of ASCII letters, digits, apostrophes and hyphens, as the
/// bench's pattern finds them.
#[test]
fn window_words_are_the_bench_tokens() {
    let words: Vec<&str> = window_words("Don't—stop: café x-ray 10mg ’tis")
        .into_iter()
        .map(|(w, _)| w)
        .collect();
    assert_eq!(words, ["Don't", "stop", "caf", "x-ray", "10mg", "tis"]);
    assert_eq!(letters("Crohn’s GLP-1 É"), "crohnsglp1");
}

/// Text with no words, or no terms to look for, offers nothing.
#[test]
fn nothing_to_look_in_or_for_offers_nothing() {
    assert_eq!(
        offered(&Fake::new(&[]), &[term("amlodipine", &[])], " .,; "),
        pairs(&[])
    );
    assert_eq!(offered(&Fake::new(&[]), &[], "amlodipin"), pairs(&[]));
}

/// Two sets of trigrams share the part of their union both have; two empty
/// sets share nothing.
#[test]
fn trigram_jaccard_is_shared_over_union() {
    let empty = Trigrams(Vec::new());
    assert!(empty.jaccard(&empty).abs() < 1e-12);
    // ^ab ab$ against ^ab abc bc$: one shared of four.
    assert!((Trigrams::of("ab").jaccard(&Trigrams::of("abc")) - 0.25).abs() < 1e-12);
}

/// A hyphenated alias heard as separate words is matched, past the three
/// words a scored window may have.
#[test]
fn a_hyphenated_alias_heard_as_words_is_matched() {
    let terms = [term("GLP-1", &["gee-el-pee-one"])];
    assert_eq!(
        offered(&Fake::new(&[]), &terms, "a gee el pee one agonist"),
        pairs(&[("gee el pee one", "GLP-1")])
    );
}

/// A lookup can be shared with another thread.
#[test]
fn a_lookup_can_be_shared_between_threads() {
    fn shareable<T: Send + Sync>(_: &T) {}
    let terms = [term("amlodipine", &[])];
    shareable(&Lookup::new(english(), &terms));
}
