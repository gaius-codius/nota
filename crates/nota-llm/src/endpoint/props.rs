//! Property tests for reading a base URL. The contract:
//! - any text is taken or refused, never a panic
//! - an endpoint taken writes a `Host` header and a path with no space,
//!   line break or other character that could end the request's line
//! - a URL nota writes for an endpoint is read back as the same endpoint

use proptest::prelude::*;

use super::*;

/// Whether `text` is safe on a request's first line or in a header.
fn safe_in_a_head(text: &str) -> bool {
    text.bytes().all(|b| b.is_ascii_graphic())
}

proptest! {
    /// Any text is taken or refused, and what's taken can't break the
    /// request's head.
    #[test]
    fn any_url_is_taken_or_refused(rest in "\\PC{0,64}", any in ".{0,64}") {
        for url in [format!("http://{rest}"), any] {
            if let Ok(endpoint) = Endpoint::parse(&url) {
                prop_assert!(safe_in_a_head(&endpoint.host_header()), "{url:?}");
                prop_assert!(safe_in_a_head(&endpoint.chat_path()), "{url:?}");
            }
        }
    }

    /// An endpoint's own URL reads back as the same endpoint.
    #[test]
    fn an_endpoints_url_reads_back(
        host in "[a-z0-9][a-z0-9.-]{0,20}",
        port in 1..=u16::MAX,
        base in "(/[a-z0-9._~-]{1,8}){0,3}",
    ) {
        let endpoint = Endpoint::parse(&format!("http://{host}:{port}{base}")).unwrap();
        prop_assert_eq!(Endpoint::parse(&endpoint.to_string()), Ok(endpoint));
    }
}
