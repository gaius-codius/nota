//! Hotwords for Parakeet: which of a session's terms sherpa-onnx can boost,
//! and the vocabulary it splits them with.
//!
//! sherpa-onnx turns each hotword into the model's tokens with its own
//! splitter (ssentencepiece), which picks the split whose pieces' scores
//! sum highest, from a vocabulary file read when the recogniser is built.
//! Parakeet's own tokenizer is BPE: it merges, again and again, the
//! adjacent pair whose merged piece comes first in `tokens.txt`. The two
//! often disagree, and a hotword split unlike the model's output is never
//! boosted (research bench, `make_vocab.py`; sherpa-onnx PR 3980). No one
//! vocabulary makes them agree on every word: over the pharmacy pack's
//! 3,696 words, a vocabulary of every piece scored by its rank splits
//! 2,960 unlike the model.
//!
//! So [`prepare`] builds a vocabulary for one list, as `make_vocab.py`
//! does: only the pieces the model's splits of the list's words use, each
//! scored -1, and every single character at -100 so no word is left
//! without a split; then, round by round, it raises the pieces of each
//! word split wrongly and lowers the ones the wrong split used. A term
//! whose words still split unlike the model is left out of the list.
//!
//! The same list always gives the same vocabulary, byte for byte: scores
//! are whole tenths, words are taken in sorted order, and the rounds stop
//! at [`MAX_ROUNDS`]. A restarted engine therefore boosts exactly as the
//! one it replaced did.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

use nota_core::messages::Hotword;

/// The most rounds the vocabulary is trained for. The bench glossary (174
/// words) agrees after 9; a whole pack may not agree on every word, and
/// its terms that don't are left out.
pub const MAX_ROUNDS: usize = 200;

/// The most lines of `tokens.txt` read. Parakeet TDT 0.6B v3 has 8,193.
const MAX_TOKENS: usize = 100_000;

/// The score of a piece a word of the list uses, in tenths.
const USED: i32 = -10;

/// The score of a single character no word of the list uses, in tenths:
/// low enough that a split falls back on it only where nothing else fits.
const FALLBACK: i32 = -1_000;

/// sentencepiece's mark for the start of a word.
const WORD_START: char = '▁';

/// Parakeet's tokens, from its `tokens.txt`: each piece and its id, which
/// is also its rank among the tokenizer's merges.
#[derive(Debug, Clone)]
pub struct Tokens {
    /// Each piece's id.
    rank: HashMap<String, u32>,
    /// The longest piece, in bytes.
    longest: usize,
}

/// `tokens.txt` didn't parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokensError {
    /// The line numbered `.0` (from 1) isn't a piece, a space and an id.
    Line(usize),
    /// The file has more than 100,000 lines.
    TooLong,
}

impl fmt::Display for TokensError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Line(n) => write!(f, "tokens.txt line {n} isn't a piece and an id"),
            Self::TooLong => write!(f, "tokens.txt has more than {MAX_TOKENS} lines"),
        }
    }
}

impl std::error::Error for TokensError {}

impl Tokens {
    /// The tokens in `text`, the contents of `tokens.txt`: one
    /// `<piece> <id>` per line.
    ///
    /// # Errors
    ///
    /// [`TokensError`] for a line that isn't a piece and an id, or a file
    /// over 100,000 lines.
    pub fn parse(text: &str) -> Result<Self, TokensError> {
        let mut rank = HashMap::new();
        for (n, line) in text.lines().enumerate() {
            if n >= MAX_TOKENS {
                return Err(TokensError::TooLong);
            }
            let (piece, id) = line
                .rsplit_once(' ')
                .filter(|(piece, _)| !piece.is_empty())
                .ok_or(TokensError::Line(n + 1))?;
            let id: u32 = id.parse().map_err(|_| TokensError::Line(n + 1))?;
            rank.insert(piece.to_owned(), id);
        }
        let longest = rank.keys().map(String::len).max().unwrap_or(0);
        Ok(Self { rank, longest })
    }

    /// How Parakeet's tokenizer splits `word`: start from its characters,
    /// after the word-start mark, and merge the adjacent pair whose merged
    /// piece has the lowest id, the leftmost on a tie, until no pair is a
    /// piece. `None` if a character isn't a piece of its own, since the
    /// model can't say the word as written.
    #[must_use]
    pub fn model_split(&self, word: &str) -> Option<Vec<String>> {
        let mut pieces: Vec<String> = std::iter::once(WORD_START)
            .chain(word.chars())
            .map(String::from)
            .collect();
        if pieces.iter().any(|piece| !self.rank.contains_key(piece)) {
            return None;
        }
        while let Some(at) = self.best_merge(&pieces) {
            let next = pieces.remove(at + 1);
            pieces[at].push_str(&next);
        }
        Some(pieces)
    }

    /// Where in `pieces` the next merge is: the pair whose merged piece has
    /// the lowest id, the leftmost on a tie.
    fn best_merge(&self, pieces: &[String]) -> Option<usize> {
        let mut best: Option<(u32, usize)> = None;
        for (at, pair) in pieces.windows(2).enumerate() {
            let merged = format!("{}{}", pair[0], pair[1]);
            if let Some(&id) = self.rank.get(&merged)
                && best.is_none_or(|(lowest, _)| id < lowest)
            {
                best = Some((id, at));
            }
        }
        best.map(|(_, at)| at)
    }

    /// The single-character pieces, and the word-start mark with one
    /// character: what a split can always fall back on.
    fn characters(&self) -> impl Iterator<Item = &str> {
        self.rank.keys().map(String::as_str).filter(|piece| {
            let mut chars = piece.chars();
            matches!(
                (chars.next(), chars.next(), chars.next()),
                (Some(_), None, None) | (Some(WORD_START), Some(_), None)
            )
        })
    }
}

/// What a recogniser boosts for one list: the vocabulary to build it with,
/// and the terms it can boost, in sherpa-onnx's per-stream format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boost {
    /// The vocabulary file's text: `<piece>\t<score>` per line, in piece
    /// order.
    pub vocab: String,
    /// The terms it boosts, lowercased, separated by `/`.
    pub hotwords: String,
    /// The terms it boosts, lowercased, in the list's order.
    pub admitted: Vec<String>,
    /// The terms left out because the vocabulary doesn't split them as the
    /// model does, or the model can't say them, in the list's order.
    pub left_out: Vec<String>,
}

/// What to boost for `hotwords`: each lowercased (Parakeet's lowercase
/// pieces are what the bench boosted), duplicates dropped, and kept if the
/// trained vocabulary splits every word of it as the model does.
#[must_use]
pub fn prepare(tokens: &Tokens, hotwords: &[Hotword]) -> Boost {
    let terms = distinct_lowercase(hotwords);
    let mut model: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut sayable = Vec::new();
    for term in &terms {
        let splits: Option<Vec<(String, Vec<String>)>> = term
            .split(' ')
            .map(|word| Some((word.to_owned(), tokens.model_split(word)?)))
            .collect();
        if let Some(splits) = splits {
            model.extend(splits);
            sayable.push(term.as_str());
        }
    }
    let vocab = train(tokens, &model);
    let agrees = |word: &str| {
        model
            .get(word)
            .is_some_and(|want| vocab.split(word) == *want)
    };
    let (admitted, left_out): (Vec<String>, Vec<String>) = terms
        .iter()
        .cloned()
        .partition(|term| sayable.contains(&term.as_str()) && term.split(' ').all(agrees));
    Boost {
        vocab: vocab.text(),
        hotwords: admitted.join("/"),
        admitted,
        left_out,
    }
}

/// `hotwords` lowercased, each once, in their first order.
fn distinct_lowercase(hotwords: &[Hotword]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    hotwords
        .iter()
        .map(|hotword| hotword.as_str().to_lowercase())
        .filter(|term| seen.insert(term.clone()))
        .collect()
}

/// A vocabulary for ssentencepiece: each piece's score, in whole tenths.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Vocab {
    /// Each piece's score, in tenths.
    tenths: BTreeMap<String, i32>,
    /// The same scores, keyed by the pieces' bytes, for splitting.
    by_bytes: HashMap<Vec<u8>, i32>,
    /// The longest piece, in bytes.
    longest: usize,
}

impl Vocab {
    /// The vocabulary scoring each piece in `tenths`.
    fn new(tenths: BTreeMap<String, i32>, longest: usize) -> Self {
        let by_bytes = tenths
            .iter()
            .map(|(piece, &t)| (piece.as_bytes().to_vec(), t))
            .collect();
        Self {
            tenths,
            by_bytes,
            longest,
        }
    }

    /// Adds `by` tenths to `piece`'s score.
    fn nudge(&mut self, piece: &str, by: i32) {
        if let Some(t) = self.tenths.get_mut(piece) {
            *t += by;
            self.by_bytes.insert(piece.as_bytes().to_vec(), *t);
        }
    }

    /// How ssentencepiece splits `word` with this vocabulary. It's the C++
    /// `CalcDp` and `Cut`, over bytes, with `f32` sums in the same order:
    /// from the end, each position takes the piece starting there whose
    /// score plus the best from its end is highest, the shorter piece on a
    /// tie; a position no piece starts at scores 0 and is cut as `<unk>`.
    fn split(&self, word: &str) -> Vec<String> {
        let text = format!("{WORD_START}{word}");
        let bytes = text.as_bytes();
        let n = bytes.len();
        // (score, end of the piece taken, or None at a dead end)
        let mut route: Vec<(f32, Option<usize>)> = vec![(0.0, None); n + 1];
        for i in (0..n).rev() {
            let mut best: (f32, Option<usize>) = (f32::NEG_INFINITY, None);
            for j in i + 1..=n.min(i + self.longest) {
                if let Some(&t) = self.by_bytes.get(&bytes[i..j]) {
                    let score = score(t) + route[j].0;
                    // Strictly higher: the first, shortest piece wins a tie.
                    if score > best.0 {
                        best = (score, Some(j));
                    }
                }
            }
            route[i] = if best.1.is_some() { best } else { (0.0, None) };
        }
        cut(bytes, &route)
    }

    /// The vocabulary file's text: `<piece>\t<score>` per line, in piece
    /// order, each score in tenths as a decimal ("-1.3").
    fn text(&self) -> String {
        let mut text = String::new();
        for (piece, &t) in &self.tenths {
            text.push_str(piece);
            text.push('\t');
            text.push_str(&decimal(t));
            text.push('\n');
        }
        text
    }
}

/// The pieces the `route` takes through `bytes`, `<unk>` for each byte at a
/// dead end.
fn cut(bytes: &[u8], route: &[(f32, Option<usize>)]) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(j) = route[i].1 {
            pieces.push(String::from_utf8_lossy(&bytes[i..j]).into_owned());
            i = j;
        } else {
            pieces.push("<unk>".to_owned());
            i += 1;
        }
    }
    pieces
}

/// The `f32` C++ reads for `tenths` written as [`decimal`]: both are the
/// nearest `f32` to the same number, since the division is correctly
/// rounded and so is C++'s parse.
#[expect(
    clippy::cast_precision_loss,
    reason = "scores stay far below 2^24 tenths, exact in an f32"
)]
fn score(tenths: i32) -> f32 {
    tenths as f32 / 10.0
}

/// `tenths` as a decimal with one place: -13 is "-1.3", -5 is "-0.5".
fn decimal(tenths: i32) -> String {
    let sign = if tenths < 0 { "-" } else { "" };
    let abs = tenths.unsigned_abs();
    format!("{sign}{}.{}", abs / 10, abs % 10)
}

/// The vocabulary for the words in `model` (each with the model's split),
/// trained as `make_vocab.py` trains it: each round, every word split
/// wrongly raises its model pieces by a tenth and lowers the pieces of its
/// wrong split that the list uses, until every word agrees or
/// [`MAX_ROUNDS`] have run. The best round's vocabulary (fewest words
/// wrong, the earliest on a tie) is kept.
fn train(tokens: &Tokens, model: &BTreeMap<String, Vec<String>>) -> Vocab {
    let used: BTreeSet<&str> = model.values().flatten().map(String::as_str).collect();
    let mut tenths: BTreeMap<String, i32> = used.iter().map(|&p| (p.to_owned(), USED)).collect();
    for piece in tokens.characters() {
        tenths.entry(piece.to_owned()).or_insert(FALLBACK);
    }
    let mut vocab = Vocab::new(tenths, tokens.longest);
    let mut best: Option<(usize, Vocab)> = None;
    for _ in 0..MAX_ROUNDS {
        let wrong: Vec<(&String, &Vec<String>)> = model
            .iter()
            .filter(|(word, want)| vocab.split(word) != **want)
            .collect();
        if best
            .as_ref()
            .is_none_or(|(fewest, _)| wrong.len() < *fewest)
        {
            best = Some((wrong.len(), vocab.clone()));
        }
        if wrong.is_empty() {
            break;
        }
        for (word, want) in wrong {
            let got = vocab.split(word);
            for piece in want {
                vocab.nudge(piece, 1);
            }
            for piece in got.iter().filter(|p| used.contains(p.as_str())) {
                vocab.nudge(piece, -1);
            }
        }
    }
    best.map_or(vocab, |(_, kept)| kept)
}

#[cfg(test)]
mod tests;
