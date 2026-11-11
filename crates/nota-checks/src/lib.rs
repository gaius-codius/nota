//! nota's code checks: the deterministic rules around the models. A model
//! may propose a fix or vote on one; only these checks decide what is
//! applied. The same input always gives the same answer: the crate reads no
//! files, no clock and no randomness, and starts no threads. (C4's word list
//! is built into the binary, not read from a file.)
//!
//! The rules are ported from the research bench's scripts, which are their
//! reference implementation; fixtures exported from the bench pin each port
//! to the bench's decisions (the code checks spec, "Principles").
//!
//! - C1, letter similarity: [`letter_similarity`].
//! - C2, the candidate gate: [`gate()`], whose [`AppliedFix`] is the only way
//!   a fix is applied.
//! - C4, the term lookup: [`Lookup`], with a [`Language`]'s rules
//!   ([`English`]).
//!
//! ```
//! use nota_checks::letter_similarity;
//!
//! let score = letter_similarity("amylo diping", "amlodipine")?;
//! assert!((score.value() - 6.0 / 7.0).abs() < 1e-12);
//! # Ok::<(), nota_checks::TooLong>(())
//! ```
//!
//! ```
//! use nota_checks::{English, Lookup, Term};
//!
//! let english = English::new()?;
//! let terms = [Term {
//!     term: "amlodipine".to_owned(),
//!     aliases: Vec::new(),
//!     language: "en".to_owned(),
//! }];
//! let lookup = Lookup::new(&english, &terms);
//! let found = lookup.find("start amylo diping daily", &|_, _| true);
//! assert_eq!(found[0].window(), "amylo diping");
//! assert_eq!(found[0].term(), "amlodipine");
//! # Ok::<(), nota_checks::WordListError>(())
//! ```

mod gate;
mod language;
mod lookup;
mod phonetic;
mod similarity;
mod words;

pub use gate::{
    AppliedFix, Decision, Gated, Offered, Proposal, Rejection, SIMILARITY_MIN, Verdict, gate,
};
pub use language::{English, Language};
pub use lookup::{
    Bar, COMMON_ZIPF, Candidate, EVERYDAY_ZIPF, FUNCTION_ZIPF, KEEP, KEEP_RARE, LENGTH_SLACK,
    Lookup, MAX_CANDIDATES, MAX_EXACT_WORDS, MAX_WORDS, MIN_LETTERS, Matched, RARE_ZIPF,
    SHORT_WORD_LEN, Score, Term,
};
pub use phonetic::Sounds;
pub use similarity::{MAX_LETTERS, Similarity, TooLong, letter_similarity};
pub use words::{WordListError, Zipf};
