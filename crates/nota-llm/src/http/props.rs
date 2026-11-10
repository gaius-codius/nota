//! Property tests for the response reader. The contract:
//! - any bytes give a response or an error, never a panic
//! - a response read has a status from 100 to 599 and a body within
//!   [`MAX_REPLY_LEN`]
//! - a response with a length gives exactly that body, however the bytes
//!   arrive

use std::io::Cursor;

use proptest::prelude::*;

use super::*;

/// Any status line and headers, then any body.
fn any_response_bytes() -> impl Strategy<Value = Vec<u8>> {
    let head = prop::collection::vec("[ -~]{0,40}", 0..6).prop_map(|lines| lines.join("\r\n"));
    (
        prop::option::of(100_u16..600),
        head,
        prop::collection::vec(any::<u8>(), 0..64),
    )
        .prop_map(|(status, head, body)| {
            let mut bytes = status
                .map_or_else(String::new, |s| format!("HTTP/1.1 {s} OK\r\n"))
                .into_bytes();
            bytes.extend_from_slice(head.as_bytes());
            bytes.extend_from_slice(b"\r\n\r\n");
            bytes.extend(body);
            bytes
        })
}

proptest! {
    /// Any bytes are read or refused, never a panic.
    #[test]
    fn any_bytes_give_a_response_or_an_error(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        if let Ok(response) = read_response(&mut Cursor::new(bytes)) {
            prop_assert!((100..=599).contains(&response.status));
            prop_assert!(response.body.len() <= MAX_REPLY_LEN);
        }
    }

    /// Bytes shaped like a response are read or refused, never a panic.
    #[test]
    fn any_response_shape_gives_a_response_or_an_error(bytes in any_response_bytes()) {
        if let Ok(response) = read_response(&mut Cursor::new(bytes)) {
            prop_assert!((100..=599).contains(&response.status));
        }
    }

    /// A response with a length gives exactly its body.
    #[test]
    fn a_length_gives_exactly_its_body(
        status in 100_u16..600,
        body in prop::collection::vec(any::<u8>(), 0..256),
        extra in prop::collection::vec(any::<u8>(), 0..16),
    ) {
        let mut bytes = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        bytes.extend_from_slice(&body);
        bytes.extend_from_slice(&extra);
        let response = read_response(&mut Cursor::new(bytes)).unwrap();
        prop_assert_eq!(response, Response { status, body });
    }
}
