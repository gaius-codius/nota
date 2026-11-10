//! nota's code checks: the deterministic rules around the models. A model
//! may propose a fix or vote on one; only these checks decide what is
//! applied. The same input always gives the same answer: the crate reads no
//! files, no clock and no randomness, and starts no threads.
//!
//! The rules are ported from the research bench's scripts, which are their
//! reference implementation; fixtures exported from the bench pin each port
//! to the bench's decisions (the code checks spec, "Principles").
//!
//! - C1, letter similarity: [`letter_similarity`].
//!
//! ```
//! use nota_checks::letter_similarity;
//!
//! let score = letter_similarity("amylo diping", "amlodipine")?;
//! assert!((score.value() - 6.0 / 7.0).abs() < 1e-12);
//! # Ok::<(), nota_checks::TooLong>(())
//! ```

mod similarity;

pub use similarity::{MAX_LETTERS, Similarity, TooLong, letter_similarity};
