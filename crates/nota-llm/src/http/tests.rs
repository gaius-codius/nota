use std::io::Cursor;

use super::*;

/// A real health response from llama-server (build d2e5458).
const HEALTH: &[u8] = b"HTTP/1.1 200 OK\r\nServer: llama.cpp\r\nAccess-Control-Allow-Origin: \r\n\
    Content-Type: application/json; charset=utf-8\r\nContent-Length: 15\r\n\
    Connection: close\r\n\r\n{\"status\":\"ok\"}";

/// `bytes` read as a response.
fn read(bytes: &[u8]) -> Result<Response, HttpError> {
    read_response(&mut Cursor::new(bytes.to_vec()))
}

/// A reader that gives its bytes a few at a time, as a socket may.
struct Trickle(Vec<u8>, usize);

impl Read for Trickle {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = (self.0.len() - self.1).min(buf.len()).min(3);
        buf[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
        self.1 += n;
        Ok(n)
    }
}

/// A real health response is read: its status, and exactly its body.
#[test]
fn a_real_response_is_read() {
    let response = read(HEALTH).unwrap();
    assert_eq!(
        response,
        Response {
            status: 200,
            body: b"{\"status\":\"ok\"}".to_vec()
        }
    );
}

/// A response that arrives a few bytes at a time reads the same.
#[test]
fn a_response_in_pieces_reads_the_same() {
    let response = read_response(&mut Trickle(HEALTH.to_vec(), 0)).unwrap();
    assert_eq!(response, read(HEALTH).unwrap());
}

/// Bytes past the length are left unread, not taken as body.
#[test]
fn the_length_bounds_the_body() {
    let response = read(b"HTTP/1.1 503 Busy\r\nContent-Length: 2\r\n\r\nokEXTRA").unwrap();
    assert_eq!(
        response,
        Response {
            status: 503,
            body: b"ok".to_vec()
        }
    );
}

/// With no length, the body runs to the end of the connection.
#[test]
fn with_no_length_the_body_runs_to_the_end() {
    let response = read(b"HTTP/1.0 200 OK\r\nServer: x\r\n\r\nall of it").unwrap();
    assert_eq!(
        response,
        Response {
            status: 200,
            body: b"all of it".to_vec()
        }
    );
}

/// A connection that closes before the head ends, or before the body's
/// length is in, is an error, not a short response.
#[test]
fn a_connection_closed_early_is_an_error() {
    for bytes in [
        &b""[..],
        b"HTTP/1.1 200 OK\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort",
    ] {
        let err = read(bytes).unwrap_err();
        assert!(
            matches!(&err, HttpError::Io(e) if e.kind() == io::ErrorKind::UnexpectedEof),
            "{bytes:?}: {err:?}"
        );
    }
}

/// Heads that aren't HTTP/1.x as nota reads them are refused, saying why.
#[test]
fn a_bad_head_is_refused() {
    let cases: [(&[u8], &str); 9] = [
        (b"HTTP/2 200 OK\r\n\r\n", "no status line"),
        (b"HTTP/1.1 20 OK\r\n\r\n", "no status line"),
        (b"HTTP/1.1 2000 OK\r\n\r\n", "no status line"),
        (b"HTTP/1.1 099\r\n\r\n", "no status line"),
        (b"SSH-2.0-OpenSSH\r\n\r\n", "no status line"),
        (
            b"HTTP/1.1 200 OK\r\nno colon here\r\n\r\n",
            "a header has no colon",
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: ten\r\n\r\n",
            "its length isn't a number",
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\ncontent-length: 2\r\n\r\nab",
            "it gives two lengths",
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n\r\n",
            "it's chunked",
        ),
    ];
    for (bytes, why) in cases {
        let err = read(bytes).unwrap_err();
        assert!(
            matches!(err, HttpError::BadHead(w) if w == why),
            "{bytes:?}: {err:?}"
        );
    }
    assert!(matches!(
        read(b"HTTP/1.1 \xff\r\n\r\n"),
        Err(HttpError::BadHead("it isn't text"))
    ));
}

/// A status line may end at its code, with no reason after it.
#[test]
fn a_status_without_a_reason_is_read() {
    assert_eq!(
        read(b"HTTP/1.1 404\r\nContent-Length: 0\r\n\r\n")
            .unwrap()
            .status,
        404
    );
}

/// A head or body over its limit is refused, without reading on.
#[test]
fn a_head_or_body_over_its_limit_is_refused() {
    let mut long_head = b"HTTP/1.1 200 OK\r\n".to_vec();
    long_head.extend(std::iter::repeat_n(b'x', MAX_HEAD_LEN + 10));
    assert!(matches!(read(&long_head), Err(HttpError::TooLong("head"))));

    let too_long = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
        MAX_REPLY_LEN + 1
    );
    assert!(matches!(
        read(too_long.as_bytes()),
        Err(HttpError::TooLong("body"))
    ));

    let mut endless = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
    endless.extend(std::iter::repeat_n(b'x', MAX_REPLY_LEN + 1));
    assert!(matches!(read(&endless), Err(HttpError::TooLong("body"))));
    endless.truncate(endless.len() - 1);
    assert_eq!(read(&endless).unwrap().body.len(), MAX_REPLY_LEN);
}

/// A head over its limit is refused even when its blank line comes in the
/// same read, and one exactly at its limit is read.
#[test]
fn a_whole_head_over_its_limit_is_refused() {
    let head = |len: usize| {
        let mut bytes = b"HTTP/1.1 200 OK\r\nX: ".to_vec();
        bytes.resize(len, b'x');
        bytes.extend(b"\r\n\r\n");
        bytes
    };
    assert!(matches!(
        read(&head(MAX_HEAD_LEN + 1)),
        Err(HttpError::TooLong("head"))
    ));
    assert_eq!(read(&head(MAX_HEAD_LEN)).unwrap().status, 200);
}

/// A request is a closed-connection GET, or a POST with its JSON and its
/// length.
#[test]
fn requests_are_written_as_http() {
    assert_eq!(
        request("/health", None),
        b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    );
    assert_eq!(
        request("/v1/chat/completions", Some(b"{}")),
        b"POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
          Content-Type: application/json\r\nContent-Length: 2\r\n\r\n{}"
    );
}

/// An exchange writes the request and reads the response that follows.
#[test]
fn an_exchange_writes_then_reads() {
    /// A connection: what was written, and what there is to read.
    struct Socket(Vec<u8>, Cursor<Vec<u8>>);
    impl Read for Socket {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.1.read(buf)
        }
    }
    impl Write for Socket {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut socket = Socket(Vec::new(), Cursor::new(HEALTH.to_vec()));
    let response = exchange(&mut socket, &request("/health", None)).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(socket.0, request("/health", None));
}
