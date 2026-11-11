//! Just enough HTTP/1.1 to talk to llama-server over its UNIX socket, and
//! to an OpenAI-compatible server over TCP.
//!
//! nota sends one request per connection, with `Connection: close`, and
//! reads one response. llama-server frames a response with
//! `Content-Length`; with no length, the body runs to the end of the
//! connection. Other servers may send a whole answer in chunks (Go's,
//! Ollama's among them, do once it's over a couple of KiB), which are
//! joined; any other transfer encoding is refused. Everything read is
//! bounded: the head to [`MAX_HEAD_LEN`], the body to [`MAX_REPLY_LEN`],
//! and each chunk's size line to [`MAX_CHUNK_LINE`].
//!
//! A UNIX socket rather than a TCP port, because the socket lives in a
//! directory only this user can open, so no other user on the machine can
//! send the server work, and it can't clash with another server's port.
//! The common HTTP crates speak only TCP.

use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};

use crate::reply::MAX_REPLY_LEN;

/// The longest chunk size line read, in bytes: a size in hex, with room for
/// an extension after it.
pub const MAX_CHUNK_LINE: usize = 256;

/// How a response's body is framed, from its head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    /// `Content-Length` gives its length.
    Length(usize),
    /// It comes in chunks.
    Chunked,
    /// It runs to the end of the connection.
    ToEnd,
}

/// The longest response head (status line and headers) read, in bytes.
/// llama-server's are about 200.
pub const MAX_HEAD_LEN: usize = 16_384;

/// A response: its status code and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The status code: 200, 503 and so on.
    pub status: u16,
    /// The body, at most [`MAX_REPLY_LEN`] bytes.
    pub body: Vec<u8>,
}

/// Why no response was read.
#[derive(Debug)]
pub enum HttpError {
    /// Reading or writing the connection failed, or it closed early.
    Io(io::Error),
    /// The response's head isn't HTTP/1.x as nota reads it: what's wrong.
    BadHead(&'static str),
    /// The head or the body was longer than its limit: which.
    TooLong(&'static str),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "the connection failed: {e}"),
            Self::BadHead(what) => write!(f, "the response isn't HTTP as expected: {what}"),
            Self::TooLong(what) => write!(f, "the response's {what} is too long"),
        }
    }
}

impl Error for HttpError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for HttpError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// The request to send to `host` (its `Host` header: a name, with a port
/// if it isn't the default): `GET path`, or `POST path` with a JSON `body`.
/// The caller makes sure neither holds a space or a line break.
#[must_use]
pub fn request(host: &str, path: &str, body: Option<&[u8]>) -> Vec<u8> {
    let mut out = match body {
        Some(body) => format!(
            "POST {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes(),
        None => {
            format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").into_bytes()
        }
    };
    out.extend_from_slice(body.unwrap_or_default());
    out
}

/// Sends `request` on `stream` and reads the response.
///
/// # Errors
///
/// As [`read_response`], or if the request can't be written.
pub fn exchange(stream: &mut (impl Read + Write), request: &[u8]) -> Result<Response, HttpError> {
    stream.write_all(request)?;
    stream.flush()?;
    read_response(stream)
}

/// The response read from `input`: its head, then as much body as its
/// `Content-Length` says, or all that's left if it gives none.
///
/// # Errors
///
/// If reading fails or ends early, the head isn't HTTP/1.x, the body's
/// encoding isn't plain or chunked, its chunks are malformed, or the head
/// or body is over its limit.
pub fn read_response(input: &mut impl Read) -> Result<Response, HttpError> {
    let (head, rest) = read_head(input)?;
    let (status, framing) = parse_head(&head)?;
    let body = match framing {
        Framing::Length(length) => body_of(input, rest, length)?,
        Framing::Chunked => body_in_chunks(&mut io::Cursor::new(rest).chain(input))?,
        Framing::ToEnd => body_to_end(input, rest)?,
    };
    Ok(Response { status, body })
}

/// A chunked body read from `input`: each chunk's size line, its bytes and
/// their line end, until the chunk of size 0 and the trailers after it.
fn body_in_chunks(input: &mut impl Read) -> Result<Vec<u8>, HttpError> {
    let mut body = Vec::new();
    loop {
        let line = line_of(input)?;
        let size = line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size, 16)
            .map_err(|_| HttpError::BadHead("a chunk's size isn't a number"))?;
        if size == 0 {
            break;
        }
        if size > MAX_REPLY_LEN.saturating_sub(body.len()) {
            return Err(HttpError::TooLong("body"));
        }
        let start = body.len();
        input.take(size as u64).read_to_end(&mut body)?;
        if body.len() - start < size {
            return Err(HttpError::Io(io::ErrorKind::UnexpectedEof.into()));
        }
        if !line_of(input)?.is_empty() {
            return Err(HttpError::BadHead("a chunk runs past its size"));
        }
    }
    // Trailers, which nota has no use for, up to the blank line that ends
    // them; bounded, since each is a line.
    for _ in 0..MAX_TRAILERS {
        if line_of(input)?.is_empty() {
            return Ok(body);
        }
    }
    Err(HttpError::TooLong("trailers"))
}

/// The most trailer lines read after a chunked body.
const MAX_TRAILERS: usize = 64;

/// One line from `input`, without its `\r\n`, read a byte at a time so
/// nothing past it is taken; at most [`MAX_CHUNK_LINE`] bytes.
fn line_of(input: &mut impl Read) -> Result<String, HttpError> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        if input.read(&mut byte)? == 0 {
            return Err(HttpError::Io(io::ErrorKind::UnexpectedEof.into()));
        }
        if byte == *b"\n" {
            if line.pop() != Some(b'\r') {
                return Err(HttpError::BadHead("a chunk line doesn't end in CRLF"));
            }
            return String::from_utf8(line)
                .map_err(|_| HttpError::BadHead("a chunk line isn't text"));
        }
        if line.len() >= MAX_CHUNK_LINE {
            return Err(HttpError::TooLong("chunk line"));
        }
        line.push(byte[0]);
    }
}

/// The body that `Content-Length` says is `length` bytes: `read`, what came
/// with the head, and the rest from `input`.
fn body_of(input: &mut impl Read, mut read: Vec<u8>, length: usize) -> Result<Vec<u8>, HttpError> {
    if length > MAX_REPLY_LEN {
        return Err(HttpError::TooLong("body"));
    }
    read.truncate(length);
    let missing = length - read.len();
    input.take(missing as u64).read_to_end(&mut read)?;
    if read.len() < length {
        return Err(HttpError::Io(io::ErrorKind::UnexpectedEof.into()));
    }
    Ok(read)
}

/// A body with no length: `read`, what came with the head, and all that's
/// left of `input`.
fn body_to_end(input: &mut impl Read, mut read: Vec<u8>) -> Result<Vec<u8>, HttpError> {
    let room = MAX_REPLY_LEN.saturating_sub(read.len());
    // One byte over the room shows the body didn't fit.
    input.take(room as u64 + 1).read_to_end(&mut read)?;
    if read.len() > MAX_REPLY_LEN {
        return Err(HttpError::TooLong("body"));
    }
    Ok(read)
}

/// Reads up to the blank line that ends the head. Returns the head (without
/// the blank line) and whatever was read past it.
fn read_head(input: &mut impl Read) -> Result<(Vec<u8>, Vec<u8>), HttpError> {
    let mut read = Vec::new();
    let mut buf = [0_u8; 4_096];
    loop {
        if let Some(end) = find(&read, b"\r\n\r\n") {
            // The blank line may have come in a read that took the head
            // past its limit.
            if end > MAX_HEAD_LEN {
                return Err(HttpError::TooLong("head"));
            }
            let rest = read.split_off(end + 4);
            read.truncate(end);
            return Ok((read, rest));
        }
        if read.len() > MAX_HEAD_LEN {
            return Err(HttpError::TooLong("head"));
        }
        let n = match input.read(&mut buf) {
            Ok(0) => return Err(HttpError::Io(io::ErrorKind::UnexpectedEof.into())),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        read.extend_from_slice(buf.get(..n).unwrap_or_default());
    }
}

/// Where `needle` first starts in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The status code and the body's framing, from a response `head`.
fn parse_head(head: &[u8]) -> Result<(u16, Framing), HttpError> {
    let head = std::str::from_utf8(head).map_err(|_| HttpError::BadHead("it isn't text"))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(status_of)
        .ok_or(HttpError::BadHead("no status line"))?;
    let (mut length, mut chunked) = (None, false);
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or(HttpError::BadHead("a header has no colon"))?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            let parsed = value
                .parse::<usize>()
                .map_err(|_| HttpError::BadHead("its length isn't a number"))?;
            if length.is_some_and(|l| l != parsed) {
                return Err(HttpError::BadHead("it gives two lengths"));
            }
            length = Some(parsed);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if !value.eq_ignore_ascii_case("chunked") || chunked {
                return Err(HttpError::BadHead("its transfer encoding isn't chunked"));
            }
            chunked = true;
        }
    }
    match (length, chunked) {
        // Which to believe is a guess, and a wrong guess cuts the body in the wrong place.
        (Some(_), true) => Err(HttpError::BadHead("it gives a length and is chunked")),
        (Some(length), false) => Ok((status, Framing::Length(length))),
        (None, true) => Ok((status, Framing::Chunked)),
        (None, false) => Ok((status, Framing::ToEnd)),
    }
}

/// The status code in a status line such as `HTTP/1.1 200 OK`.
fn status_of(line: &str) -> Option<u16> {
    let rest = line.strip_prefix("HTTP/1.")?;
    let (minor, rest) = rest.split_once(' ')?;
    if minor != "0" && minor != "1" {
        return None;
    }
    let code = rest.get(..3)?;
    // The code ends the line, or a space and its reason follow it.
    if !rest
        .get(3..)
        .is_some_and(|after| after.is_empty() || after.starts_with(' '))
    {
        return None;
    }
    let status: u16 = code.parse().ok()?;
    (100..=599).contains(&status).then_some(status)
}

#[cfg(test)]
mod props;
#[cfg(test)]
mod tests;
