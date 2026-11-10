//! Coverage-guided fuzz target for the decider's reply readers.
//!
//! Reads arbitrary bytes as llama-server's HTTP response, then its body as
//! the reply to a question about one to five candidates, and asserts the
//! readers' contract on whatever they accept. A violated assertion
//! panics, which AFL records as a crash.

use nota_core::decide::Answer;
use nota_llm::http::read_response;
use nota_llm::reply::{MAX_REPLY_LEN, parse};

/// The body read as a reply about `candidates` candidates: if read, one
/// decision per candidate, each probability from 0 to 1.
fn check_reply(body: &[u8], candidates: usize) {
    let Ok(decisions) = parse(body, candidates) else {
        return;
    };
    assert_eq!(decisions.len(), candidates, "not one decision per candidate");
    for decision in decisions {
        assert!(matches!(decision.answer, Answer::Yes | Answer::No));
        if let Some(yes) = decision.yes {
            assert!((0.0..=1.0).contains(&yes.get()), "a probability of {}", yes.get());
        }
    }
}

fn main() {
    afl::fuzz!(|data: &[u8]| {
        if let Ok(response) = read_response(&mut &data[..]) {
            assert!((100..=599).contains(&response.status), "status {}", response.status);
            assert!(response.body.len() <= MAX_REPLY_LEN, "a body over the limit");
            for candidates in 1..=5 {
                check_reply(&response.body, candidates);
            }
        }
        // The same bytes as a body alone, for inputs with no HTTP head.
        for candidates in 1..=5 {
            check_reply(data, candidates);
        }
    });
}
