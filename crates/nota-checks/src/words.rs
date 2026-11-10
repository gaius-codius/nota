//! How common an English word is: wordfreq's English list and its
//! `zipf_frequency`, ported so C4's cut-offs (Zipf 1.5, 3.3, 5.0 and 6.3)
//! mean what they meant in the bench.
//!
//! The list is wordfreq's `large_en.msgpack.gz` (tag `v3.2`), unmodified,
//! built into the binary: `data/wordfreq/` holds it with its CC BY-SA 4.0
//! licence and its attribution. Its format, "cBpack", is gzip of msgpack: a
//! header, then one list of words per centibel, the rarest last. The words in
//! list `i` occur with frequency 10^(−i/100).
//!
//! A plain word → frequency table isn't enough: the bench asked wordfreq,
//! which tokenises first, gives numbers their own frequency and combines the
//! tokens of a hyphenated or apostrophised word. A table alone changes 23 of
//! the bench's 60 chunks (research note 2026-11-10, #148); this port changes
//! none. Its steps, each as wordfreq does it:
//!
//! 1. lower-case, and split into tokens on anything but letters and digits,
//!    keeping an apostrophe between two letters or two digits ("don't");
//!    one or two letters or digits before an apostrophe and a vowel are a token of
//!    their own ("y'all" is "y" and "all");
//! 2. in each token, write each run of two or more digits as zeros, look
//!    that up, and if the token changed, scale by how common such a number
//!    is (Benford's law, or a curve of years for four digits);
//! 3. if any token isn't in the list, the word is as rare as can be;
//! 4. combine tokens as 1/f = 1/f1 + 1/f2 + …, never below 10⁻⁹, round to
//!    three significant figures, and report log10(f) + 9 to two places.
//!
//! The port covers what C4 asks about: words made of ASCII letters, digits,
//! apostrophes and hyphens. Any other character only separates tokens here,
//! where wordfreq would keep a non-ASCII letter inside its word; the lookup
//! never asks about one.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::error::Error;
use std::fmt;
use std::hash::BuildHasherDefault;
use std::io::{self, Read};

use flate2::read::GzDecoder;

/// wordfreq's English list, `large_en.msgpack.gz` at tag `v3.2`, byte for
/// byte (sha256 `dffae806…5c`, pinned in `scripts/check-files.allow`).
const LARGE_EN: &[u8] = include_bytes!("../data/wordfreq/large_en.msgpack.gz");

/// The most bytes the list may unpack to. The pinned file unpacks to about
/// 2.6 MB; the cap stops a damaged file from filling memory.
const MAX_UNPACKED: u64 = 16 << 20;

/// The most centibel lists a cBpack may hold: a word in list 1,000 occurs
/// once in 10¹⁰, below the 10⁻⁹ floor every lookup is held to. The pinned
/// file has 800.
const MAX_BUCKETS: usize = 1_000;

/// The frequency a word missing from the list gets, and the least any word
/// gets: wordfreq's `zipf_to_freq(0)`, once in a billion words.
const MIN_FREQ: f64 = 1.0 / 1e9;

/// How common a word is on the Zipf scale: log10 of its occurrences per
/// billion words, to two places. 0 for a word not in the list; 7 or so for
/// "the".
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Zipf(f64);

impl Zipf {
    /// The Zipf value `value`, for a [`Language`](crate::Language) with its
    /// own word list.
    #[must_use]
    pub fn new(value: f64) -> Self {
        Self(value)
    }

    /// The value, rounded to two places as wordfreq rounds it.
    #[must_use]
    pub fn value(self) -> f64 {
        self.0
    }
}

/// Why the word list couldn't be read. The list is built into the binary,
/// so any of these means the build is damaged; nothing was looked up.
#[derive(Debug)]
pub enum WordListError {
    /// The gzip layer couldn't be unpacked.
    Unpack(io::Error),
    /// It unpacked to more than the cap allows.
    TooLarge,
    /// The msgpack inside isn't a cBpack: `what` was expected at byte `at`
    /// of the unpacked data.
    Malformed {
        /// The offset in the unpacked bytes.
        at: usize,
        /// What should have been there.
        what: &'static str,
    },
}

impl fmt::Display for WordListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unpack(_) => write!(f, "the word list couldn't be unpacked"),
            Self::TooLarge => write!(f, "the word list unpacks to more than {MAX_UNPACKED} bytes"),
            Self::Malformed { at, what } => {
                write!(f, "the word list is damaged: expected {what} at byte {at}")
            }
        }
    }
}

impl Error for WordListError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Unpack(e) => Some(e),
            Self::TooLarge | Self::Malformed { .. } => None,
        }
    }
}

/// Each word and its centibel list's index. Hashed with fixed keys, not
/// the standard library's seeded ones, so the crate reads no randomness.
type Buckets = HashMap<Box<str>, u16, BuildHasherDefault<DefaultHasher>>;

/// A word list: each word and the centibel list it's in.
#[derive(Debug, Clone)]
pub(crate) struct WordList {
    /// Each word and its list's index, `i` for frequency 10^(−i/100).
    buckets: Buckets,
}

impl WordList {
    /// wordfreq's English list, unpacked from the copy built in.
    ///
    /// # Errors
    ///
    /// [`WordListError`] if the built-in copy is damaged.
    pub(crate) fn english() -> Result<Self, WordListError> {
        Self::from_gzip(LARGE_EN)
    }

    /// The list in `gz`, a gzipped cBpack.
    ///
    /// # Errors
    ///
    /// [`WordListError`] if `gz` isn't gzip, unpacks past [`MAX_UNPACKED`],
    /// or isn't a cBpack.
    pub(crate) fn from_gzip(gz: &[u8]) -> Result<Self, WordListError> {
        let mut packed = Vec::new();
        GzDecoder::new(gz)
            .take(MAX_UNPACKED + 1)
            .read_to_end(&mut packed)
            .map_err(WordListError::Unpack)?;
        if u64::try_from(packed.len()).map_or(true, |n| n > MAX_UNPACKED) {
            return Err(WordListError::TooLarge);
        }
        Self::from_cbpack(&packed)
    }

    /// The list in `bytes`, an unpacked cBpack.
    ///
    /// # Errors
    ///
    /// [`WordListError::Malformed`] if `bytes` isn't one.
    pub(crate) fn from_cbpack(bytes: &[u8]) -> Result<Self, WordListError> {
        let mut r = cbpack::Reader::new(bytes);
        let lists = r.array_len("the outer list")?;
        if lists == 0 {
            return Err(r.malformed("a header"));
        }
        r.header()?;
        if lists - 1 > MAX_BUCKETS {
            return Err(r.malformed("at most 1,000 frequency lists"));
        }
        let mut buckets = Buckets::default();
        for index in 0..lists - 1 {
            let index = u16::try_from(index).map_err(|_| r.malformed("a smaller list"))?;
            for _ in 0..r.array_len("a list of words")? {
                // As wordfreq builds its dict: a word listed twice keeps
                // its later, rarer, list.
                buckets.insert(r.str()?.into(), index);
            }
        }
        r.end()?;
        Ok(Self { buckets })
    }

    /// How common `text` is, as wordfreq's `zipf_frequency(text, "en")`
    /// answers for ASCII letters, digits, apostrophes and hyphens.
    pub(crate) fn zipf(&self, text: &str) -> Zipf {
        let freq = self.frequency(text);
        Zipf(round_to(log10(freq) + 9.0, 2))
    }

    /// wordfreq's `word_frequency(text, "en", minimum=1e-9)`: the tokens'
    /// frequencies combined, held to [`MIN_FREQ`] and rounded to three
    /// significant figures.
    fn frequency(&self, text: &str) -> f64 {
        let tokens = tokens(text);
        if tokens.is_empty() {
            return MIN_FREQ;
        }
        let mut one_over = 0.0;
        for token in &tokens {
            let Some(freq) = self.token_frequency(token) else {
                return MIN_FREQ;
            };
            one_over += 1.0 / freq;
        }
        let unrounded = (1.0 / one_over).max(MIN_FREQ);
        // floor(-log10) of a frequency of at most 1 is 0 or more, and at
        // most 9 here, so the cast keeps it.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the value is a whole number from 0 to 9"
        )]
        let leading_zeroes = (-log10(unrounded)).floor() as i32;
        round_to(unrounded, leading_zeroes + 3)
    }

    /// One token's frequency: its digit runs written as zeros and looked up,
    /// then scaled by how common the number is. `None` if it isn't listed.
    fn token_frequency(&self, token: &str) -> Option<f64> {
        let smashed = smash_numbers(token);
        let index = self.buckets.get(smashed.as_str())?;
        let freq = 10f64.powf(-f64::from(*index) / 100.0);
        // Only when a run was rewritten: wordfreq compares the two strings,
        // so "0000" is looked up as itself, with no number scaling.
        Some(if smashed == token {
            freq
        } else {
            freq * digit_freq(token)
        })
    }
}

/// `x`'s logarithm to base 10 as Python's `math.log(x, 10)` computes it,
/// `ln(x) / ln(10)`, which can differ from `log10` in the last bit.
fn log10(x: f64) -> f64 {
    x.ln() / 10f64.ln()
}

/// Python's `round(x, digits)`: the decimal nearest `x` with `digits` places,
/// ties to even on `x`'s exact value, read back as the nearest float. Rust's
/// formatting rounds the exact value in the same way. A negative `digits`
/// can't arise here and is taken as 0.
fn round_to(x: f64, digits: i32) -> f64 {
    let digits = usize::try_from(digits).unwrap_or(0);
    format!("{x:.digits$}").parse().unwrap_or(x)
}

/// The tokens wordfreq's English tokeniser finds in `text`, lower-cased.
///
/// Letters and digits are word characters. An apostrophe joins two letters
/// or two digits; a hyphen, any other character, or an apostrophe anywhere
/// else ends a token. At a token's start, one or two word characters, an
/// apostrophe and a vowel (`aehiouy`) make a token of the one or two
/// characters ("y'all" → "y", "all"), as wordfreq's rule for French
/// elisions does in every language.
pub(crate) fn tokens(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let mut out = Vec::new();
    let mut from = 0;
    // Each token starts at the next word character; anything else between
    // tokens, an apostrophe after an elision included, is passed over.
    while let Some(i) = (from..chars.len()).find(|&k| is_word(chars[k])) {
        if let Some(len) = elision(&chars[i..]) {
            out.push(chars[i..i + len].iter().collect());
            from = i + len;
        } else {
            let end = word_end(&chars, i);
            out.push(chars[i..end].iter().collect());
            from = end;
        }
    }
    out
}

/// A character wordfreq counts as part of a word, in the port's domain.
fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

/// The vowels (and `h`) after which an apostrophe splits an elision off.
fn is_elision_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'h' | 'i' | 'o' | 'u' | 'y')
}

/// The length (1 or 2) of an elision at the start of `rest`: word
/// characters, an apostrophe and a vowel. Two characters are tried first,
/// as the regex's greedy `\w\w?'` does.
fn elision(rest: &[char]) -> Option<usize> {
    [2, 1].into_iter().find(|&len| {
        rest.len() > len + 1
            && rest[..len].iter().all(|&c| is_word(c))
            && rest[len] == '\''
            && is_elision_vowel(rest[len + 1])
    })
}

/// Where the word starting at `start` ends: the first break between
/// characters that Unicode's word rules allow there.
fn word_end(chars: &[char], start: usize) -> usize {
    // A word character, or an apostrophe that joins its neighbours, goes on
    // the word; the first of anything else ends it. The caller starts on a
    // word character, so `start` itself never ends it.
    (start..chars.len())
        .find(|&k| {
            let joined =
                chars[k] == '\'' && k > 0 && joins(chars[k - 1], chars.get(k + 1).copied());
            !is_word(chars[k]) && !joined
        })
        .unwrap_or(chars.len())
}

/// Whether an apostrophe between `before` and `after` stays inside a word:
/// between two letters, or two digits.
fn joins(before: char, after: Option<char>) -> bool {
    after.is_some_and(|after| {
        (before.is_ascii_alphabetic() && after.is_ascii_alphabetic())
            || (before.is_ascii_digit() && after.is_ascii_digit())
    })
}

/// `token` with each run of two or more digits written as zeros
/// (wordfreq's `smash_numbers`): "2019" → "0000", "b12" → "b00", "5" → "5".
fn smash_numbers(token: &str) -> String {
    let mut out = String::with_capacity(token.len());
    for (run, is_digits) in runs(token) {
        if is_digits && run.len() > 1 {
            out.extend(std::iter::repeat_n('0', run.len()));
        } else {
            out.push_str(run);
        }
    }
    out
}

/// `token` cut into its runs of digits and of other characters, each with
/// whether it's digits.
fn runs(token: &str) -> Vec<(&str, bool)> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = token.as_bytes();
    for i in 1..=bytes.len() {
        if i == bytes.len() || bytes[i].is_ascii_digit() != bytes[start].is_ascii_digit() {
            out.push((&token[start..i], bytes[start].is_ascii_digit()));
            start = i;
        }
    }
    out
}

/// wordfreq's `digit_freq`: how common the numbers in `token` are, the
/// product over its runs of two or more digits.
fn digit_freq(token: &str) -> f64 {
    runs(token)
        .into_iter()
        .filter(|&(run, is_digits)| is_digits && run.len() > 1)
        .map(|(run, _)| {
            if run.len() == 4 {
                year_freq(run)
            } else {
                benford_freq(run)
            }
        })
        .product()
}

/// How often each leading digit starts a number, from Benford's law, with
/// an estimate for a leading zero (wordfreq's `DIGIT_FREQS`).
const DIGIT_FREQS: [f64; 10] = [
    0.009, 0.300, 0.175, 0.124, 0.096, 0.078, 0.066, 0.057, 0.050, 0.045,
];

/// wordfreq's `benford_freq`: how common the number `digits` is among
/// numbers of its length. A run too long for a float divides by infinity
/// and gives 0 (Python raises an overflow error there). The lookup never
/// gets that far: the word list's longest run of zeros is 21 digits, so a
/// longer run is missing from it, and the word counts as unknown, first.
fn benford_freq(digits: &str) -> f64 {
    let first = digits
        .bytes()
        .next()
        .map_or(0, |b| usize::from(b.saturating_sub(b'0')));
    let lead = DIGIT_FREQS.get(first).copied().unwrap_or(0.0);
    lead / power_of_ten(digits.len().saturating_sub(1))
}

/// 10ⁿ as the nearest float, as Python converts the integer `10 ** n`.
fn power_of_ten(n: usize) -> f64 {
    format!("1e{n}").parse().unwrap_or(f64::INFINITY)
}

/// wordfreq's `year_freq`: how common the four digits `digits` are, read as
/// a year (90%, peaking from 2019 to 2039) or as any number (10%).
fn year_freq(digits: &str) -> f64 {
    /// log10 of a year's share at the peak.
    const YEAR_LOG_PEAK: f64 = -1.9185;
    /// The share of four-digit numbers that aren't years.
    const NOT_YEAR_PROB: f64 = 0.1;
    /// The year the curve peaks from.
    const REFERENCE_YEAR: i32 = 2019;
    /// How many years the peak lasts.
    const PLATEAU_WIDTH: i32 = 20;

    let year: i32 = digits.parse().unwrap_or(0);
    let year_log_freq = if year <= REFERENCE_YEAR {
        YEAR_LOG_PEAK - 0.0083 * f64::from(REFERENCE_YEAR - year)
    } else if year <= REFERENCE_YEAR + PLATEAU_WIDTH {
        YEAR_LOG_PEAK
    } else {
        YEAR_LOG_PEAK - 0.2 * f64::from(year - (REFERENCE_YEAR + PLATEAU_WIDTH))
    };
    10f64.powf(year_log_freq) + NOT_YEAR_PROB * benford_freq(digits)
}

mod cbpack;

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
mod props;
