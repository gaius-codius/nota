//! Tests for the hotword vocabulary: the model's split, ssentencepiece's,
//! and the training that makes them agree.

use std::path::PathBuf;

use super::*;

/// A tiny invented `tokens.txt`: a special piece, merges in rank order,
/// then single characters. "▁a" merges first, then "bc" (2) before "▁ab"
/// (3), so "abc" ends as "▁a" "bc".
const TOKENS: &str = "<unk> 0\n▁a 1\nbc 2\n▁ab 3\n▁ 5\na 6\nb 7\nc 8\nd 9\n▁d 10\n";

/// The invented tokens.
fn tokens() -> Tokens {
    Tokens::parse(TOKENS).unwrap()
}

/// `text` as a hotword.
fn hotword(text: &str) -> Hotword {
    Hotword::new(text).unwrap()
}

/// The pieces of `split`, as `&str`s, for comparing.
fn pieces(split: &[String]) -> Vec<&str> {
    split.iter().map(String::as_str).collect()
}

/// BPE merges the lowest-id pair first: "▁a" (1), then "bc" (2) rather
/// than "▁ab" (3); "▁abc" isn't a piece, so it stops at two.
#[test]
fn the_model_merges_the_lowest_id_pair_first() {
    let split = tokens().model_split("abc").unwrap();
    assert_eq!(pieces(&split), ["▁a", "bc"]);
    assert_eq!(pieces(&tokens().model_split("d").unwrap()), ["▁d"]);
}

/// A word with a character the model has no piece for can't be said, so
/// it has no split.
#[test]
fn a_word_the_model_cannot_say_has_no_split() {
    assert_eq!(tokens().model_split("abz"), None);
}

/// ssentencepiece takes the highest total score; a shorter piece wins a
/// tie, and a byte no piece starts at is cut as `<unk>`.
#[test]
fn ssentencepiece_takes_the_best_total_and_the_shorter_piece_on_a_tie() {
    let vocab = |pairs: &[(&str, i32)]| {
        Vocab::new(pairs.iter().map(|&(p, t)| (p.to_owned(), t)).collect(), 8)
    };
    let v = vocab(&[("▁abc", -30), ("▁a", -10), ("bc", -10)]);
    assert_eq!(pieces(&v.split("abc")), ["▁a", "bc"]);
    let v = vocab(&[("▁abc", -10), ("▁a", -10), ("bc", -10)]);
    assert_eq!(pieces(&v.split("abc")), ["▁abc"]);
    // A tie: "▁a" + "bc" and "▁ab" + "c" both sum to -2.0; at the start the
    // shorter "▁a" is taken.
    let v = vocab(&[("▁a", -10), ("bc", -10), ("▁ab", -10), ("c", -10)]);
    assert_eq!(pieces(&v.split("abc")), ["▁a", "bc"]);
    let v = vocab(&[("▁a", -10)]);
    assert_eq!(pieces(&v.split("ab")), ["▁a", "<unk>"]);
}

/// Scores are written in tenths with one place, and read back as the same
/// `f32` C++ would parse.
#[test]
fn scores_are_written_in_tenths() {
    assert_eq!(
        [-13, -5, 0, 7, -1000].map(decimal),
        ["-1.3", "-0.5", "0.0", "0.7", "-100.0"]
    );
    for t in [-13, -5, 7, -1000, 12_345] {
        let parsed: f32 = decimal(t).parse().unwrap();
        assert_eq!(score(t).to_bits(), parsed.to_bits(), "{t}");
    }
}

/// Training makes ssentencepiece split each word as the model does, and
/// the vocabulary holds the words' pieces and every single character.
#[test]
fn training_makes_the_splits_agree() {
    let boost = prepare(&tokens(), &[hotword("abc"), hotword("d")]);
    assert_eq!(boost.admitted, ["abc", "d"]);
    assert_eq!(boost.left_out, Vec::<String>::new());
    assert_eq!(boost.hotwords, "abc/d");
    let pieces: Vec<&str> = boost
        .vocab
        .lines()
        .map(|l| l.split('\t').next().unwrap())
        .collect();
    assert_eq!(pieces, ["a", "b", "bc", "c", "d", "▁", "▁a", "▁d"]);
    // The words' pieces score -1, every other character -100: they already
    // agree, so no round changes a score.
    assert_eq!(
        boost.vocab,
        "a\t-100.0\nb\t-100.0\nbc\t-1.0\nc\t-100.0\nd\t-100.0\n▁\t-100.0\n▁a\t-1.0\n▁d\t-1.0\n"
    );
}

/// Invented tokens where "ccca" and "c" can't both split as the model
/// does: "cc" (1) merges before "▁c" (2), so the model splits "ccca" as
/// "▁" "cc" "c" "a", which beats "▁c" "cc" "a" only if "▁" and "c" score
/// more than "▁c"; but "c" is "▁c" whole, which needs the opposite.
const CLASHING: &str = "cc 1\n▁c 2\n▁ 3\na 4\nc 5\n";

/// Invented tokens where "aca" splits as "▁" "ac" "a" ("ac" is 1, "ca"
/// 2), but with every piece at -1 ssentencepiece ties it with "▁" "a"
/// "ca" and takes the shorter "a": a round of training is needed.
const TIED: &str = "ac 1\nca 2\n▁ 3\na 4\nc 5\n";

/// Two equal pairs merge the leftmost first, and a lower id merges before
/// a pair further left: "ccca" ends "▁" "cc" "c" "a", where the rightmost
/// "cc" or the "▁c" first would both end "▁c" "cc" "a".
#[test]
fn the_model_merges_the_leftmost_of_equal_pairs() {
    let tokens = Tokens::parse(CLASHING).unwrap();
    let split = tokens.model_split("ccca").unwrap();
    assert_eq!(pieces(&split), ["▁", "cc", "c", "a"]);
}

/// A word split wrongly raises its model pieces a tenth and lowers the
/// wrong split's, until it agrees: "ac" up and "ca" down, once.
#[test]
fn training_moves_the_scores_a_tenth_a_round_until_the_splits_agree() {
    let tokens = Tokens::parse(TIED).unwrap();
    // Untrained, ssentencepiece's tie goes against the model.
    let untrained = Vocab::new(
        ["a", "ac", "ca", "▁"]
            .map(|p| (p.to_owned(), USED))
            .into_iter()
            .collect(),
        tokens.longest,
    );
    assert_eq!(pieces(&untrained.split("aca")), ["▁", "a", "ca"]);
    let boost = prepare(&tokens, &[hotword("aca"), hotword("ca")]);
    assert_eq!(boost.admitted, ["aca", "ca"]);
    assert_eq!(boost.left_out, Vec::<String>::new());
    // "▁" and "a" were raised and lowered once each, so stay at -1.
    assert_eq!(
        boost.vocab,
        "a\t-1.0\nac\t-0.9\nc\t-100.0\nca\t-1.1\n▁\t-1.0\n"
    );
}

/// When no round makes every word agree, the earliest round with the
/// fewest wrong is kept (here the first, untrained) and the term still
/// split wrongly is left out, though the model can say it.
#[test]
fn a_term_that_never_agrees_is_left_out_and_the_earliest_best_round_kept() {
    let tokens = Tokens::parse(CLASHING).unwrap();
    let boost = prepare(&tokens, &[hotword("ccca"), hotword("c")]);
    assert_eq!(boost.admitted, ["c"]);
    assert_eq!(boost.left_out, ["ccca"]);
    assert_eq!(boost.hotwords, "c");
    assert_eq!(
        boost.vocab,
        "a\t-1.0\nc\t-1.0\ncc\t-1.0\n▁\t-1.0\n▁c\t-1.0\n"
    );
}

/// A term the model can't say is left out, and the rest are still
/// boosted; duplicates and case go.
#[test]
fn a_term_the_model_cannot_say_is_left_out() {
    let boost = prepare(
        &tokens(),
        &[hotword("ABC"), hotword("abz d"), hotword("abc")],
    );
    assert_eq!(boost.admitted, ["abc"]);
    assert_eq!(boost.left_out, ["abz d"]);
}

/// The same list gives the same vocabulary, byte for byte, so a restarted
/// engine boosts as the one it replaced.
#[test]
fn the_same_list_gives_the_same_vocabulary() {
    let list = [hotword("abc"), hotword("d"), hotword("ab")];
    assert_eq!(prepare(&tokens(), &list), prepare(&tokens(), &list));
}

/// A `tokens.txt` line without an id is refused, naming the line.
#[test]
fn a_bad_tokens_line_is_refused() {
    assert_eq!(
        Tokens::parse("▁a 1\n▁b\n").unwrap_err(),
        TokensError::Line(2)
    );
    assert_eq!(Tokens::parse("▁a x\n").unwrap_err(), TokensError::Line(1));
    assert_eq!(Tokens::parse(" 3\n").unwrap_err(), TokensError::Line(1));
}

/// Each parse error says what's wrong, and where.
#[test]
fn a_tokens_error_says_what_is_wrong() {
    assert_eq!(
        TokensError::Line(2).to_string(),
        "tokens.txt line 2 isn't a piece and an id"
    );
    assert_eq!(
        TokensError::TooLong.to_string(),
        "tokens.txt has more than 100000 lines"
    );
}

/// Parakeet's real `tokens.txt`, if the test models are installed
/// (`NOTA_TEST_MODELS`, as the engine's other model tests find them).
fn parakeet_tokens() -> Option<Tokens> {
    let root = std::env::var_os("NOTA_TEST_MODELS")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)?;
    let text = std::fs::read_to_string(root.join("parakeet-tdt-0.6b-v3-int8/tokens.txt")).ok()?;
    Some(Tokens::parse(&text).unwrap())
}

/// With Parakeet's tokens, an invented list of drug names and phrases is
/// boosted whole: every word splits as the model does.
#[test]
fn parakeet_splits_a_drug_list_as_the_model_does() {
    let Some(tokens) = parakeet_tokens() else {
        return;
    };
    let list = [
        "amlodipine",
        "atorvastatin",
        "bisoprolol",
        "varicose veins",
        "orthopnoea",
        "semaglutide",
        "hydrochlorothiazide",
        "exertional breathlessness",
        "naranjo algorithm",
    ]
    .map(hotword);
    let boost = prepare(&tokens, &list);
    assert_eq!(boost.left_out, Vec::<String>::new());
    assert_eq!(boost.admitted.len(), list.len());
    assert_eq!(
        pieces(&tokens.model_split("amlodipine").unwrap()).concat(),
        "▁amlodipine"
    );
}
