//! The engine protocol: how [`ToEngine`] and [`FromEngine`] travel over the
//! engine child's stdin and stdout.
//!
//! Every message is one frame: a little-endian `u32` body length, then the
//! body, a tag byte and the tag's fields. The length is at most
//! [`MAX_BODY_LEN`], so a garbage length can't make the reader allocate
//! gigabytes.
//!
//! | Tag | Direction | Message | Fields after the tag |
//! |---|---|---|---|
//! | `0x00` | both | [`Frame::Hello`] | `b"nota"`, version `u16` |
//! | `0x01` | to engine | [`ToEngine::Audio`] | track `u32`, first sample `u64`, rate `u32`, samples `i16`… |
//! | `0x02` | to engine | [`ToEngine::Flush`] | track `u32` |
//! | `0x81` | from engine | [`FromEngine::Transcript`] | track `u32`, start `u64`, end `u64`, UTF-8 text… |
//! | `0x82` | from engine | [`FromEngine::Confirmed`] | track `u32`, up to `u64` |
//!
//! All integers are little-endian. Each side sends `Hello` first and refuses
//! a peer whose version isn't [`ProtocolVersion::CURRENT`]; the magic bytes
//! make anything that isn't a nota engine fail the handshake rather than
//! pass it by chance. The two directions use separate tags, so a frame sent
//! the wrong way is refused.
//!
//! Bytes from the other process are untrusted: a native crash can leave half
//! a frame, and a library can print to the wrong stream. The decoder checks
//! every field and builds the typed messages ([`AudioChunk`],
//! [`SampleRange`]) through their checking constructors, so nothing past this
//! module sees raw bytes. Every frame it accepts has exactly one encoding,
//! and the property tests check that.

use std::fmt;
use std::io::{self, Read, Write};

use crate::ids::TrackId;
use crate::messages::{AudioChunk, FromEngine, ProtocolVersion, ToEngine, Transcript};
use crate::time::{SampleIndex, SampleRange, SampleRate};

/// The largest frame body, in bytes: 1 MiB.
pub const MAX_BODY_LEN: usize = 1 << 20;

/// The most samples one [`ToEngine::Audio`] frame can carry, about 32 s at
/// 16 kHz. Split longer audio into several chunks.
pub const MAX_AUDIO_SAMPLES: usize = (MAX_BODY_LEN - AUDIO_FIXED_LEN) / 2;

/// The longest transcript text one frame can carry, in bytes.
pub const MAX_TEXT_LEN: usize = MAX_BODY_LEN - TRANSCRIPT_FIXED_LEN;

const MAGIC: [u8; 4] = *b"nota";

const TAG_HELLO: u8 = 0x00;
const TAG_AUDIO: u8 = 0x01;
const TAG_FLUSH: u8 = 0x02;
const TAG_TRANSCRIPT: u8 = 0x81;
const TAG_CONFIRMED: u8 = 0x82;

/// Tag, track, first sample, rate; the samples follow.
const AUDIO_FIXED_LEN: usize = 1 + 4 + 8 + 4;
/// Tag, track, start, end; the text follows.
const TRANSCRIPT_FIXED_LEN: usize = 1 + 4 + 8 + 8;

/// One frame on the wire: the handshake, or a message of type `M`
/// ([`ToEngine`] towards the engine, [`FromEngine`] back).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame<M> {
    /// The handshake, sent first by each side.
    Hello(ProtocolVersion),
    /// A message.
    Message(M),
}

/// A message type the protocol carries: [`ToEngine`] or [`FromEngine`].
/// Sealed; the frame layout is this module's business.
pub trait WireMessage: Sized + sealed::Sealed {
    /// Appends the message's tag and fields to `out`.
    #[doc(hidden)]
    fn encode_body(&self, out: &mut Vec<u8>) -> Result<(), EncodeError>;

    /// Parses a body whose first byte, `tag`, isn't the handshake's.
    #[doc(hidden)]
    fn decode_body(tag: u8, fields: &mut Fields<'_>) -> Result<Self, DecodeError>;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for crate::messages::ToEngine {}
    impl Sealed for crate::messages::FromEngine {}
}

impl WireMessage for ToEngine {
    fn encode_body(&self, out: &mut Vec<u8>) -> Result<(), EncodeError> {
        match self {
            Self::Audio(chunk) => {
                if chunk.samples().len() > MAX_AUDIO_SAMPLES {
                    return Err(EncodeError::TooLarge);
                }
                out.push(TAG_AUDIO);
                out.extend_from_slice(&chunk.track().get().to_le_bytes());
                out.extend_from_slice(&chunk.range().start().get().to_le_bytes());
                out.extend_from_slice(&chunk.rate().hz().to_le_bytes());
                for sample in chunk.samples() {
                    out.extend_from_slice(&sample.to_le_bytes());
                }
            }
            Self::Flush { track } => {
                out.push(TAG_FLUSH);
                out.extend_from_slice(&track.get().to_le_bytes());
            }
        }
        Ok(())
    }

    fn decode_body(tag: u8, fields: &mut Fields<'_>) -> Result<Self, DecodeError> {
        match tag {
            TAG_AUDIO => {
                let track = TrackId::new(fields.u32()?);
                let first = SampleIndex::new(fields.u64()?);
                let hz = fields.u32()?;
                let rate = SampleRate::new(hz).ok_or(DecodeError::BadRate(hz))?;
                let bytes = fields.rest();
                let (pairs, odd) = bytes.as_chunks::<2>();
                if !odd.is_empty() {
                    return Err(DecodeError::OddAudioLength);
                }
                let samples = pairs.iter().map(|&pair| i16::from_le_bytes(pair)).collect();
                let chunk = AudioChunk::new(track, first, rate, samples)
                    .ok_or(DecodeError::RangeOverflow)?;
                Ok(Self::Audio(chunk))
            }
            TAG_FLUSH => {
                let track = TrackId::new(fields.u32()?);
                fields.end()?;
                Ok(Self::Flush { track })
            }
            other => Err(DecodeError::UnknownTag(other)),
        }
    }
}

impl WireMessage for FromEngine {
    fn encode_body(&self, out: &mut Vec<u8>) -> Result<(), EncodeError> {
        match self {
            Self::Transcript(transcript) => {
                if transcript.text.len() > MAX_TEXT_LEN {
                    return Err(EncodeError::TooLarge);
                }
                out.push(TAG_TRANSCRIPT);
                out.extend_from_slice(&transcript.track.get().to_le_bytes());
                out.extend_from_slice(&transcript.range.start().get().to_le_bytes());
                out.extend_from_slice(&transcript.range.end().get().to_le_bytes());
                out.extend_from_slice(transcript.text.as_bytes());
            }
            Self::Confirmed { track, up_to } => {
                out.push(TAG_CONFIRMED);
                out.extend_from_slice(&track.get().to_le_bytes());
                out.extend_from_slice(&up_to.get().to_le_bytes());
            }
        }
        Ok(())
    }

    fn decode_body(tag: u8, fields: &mut Fields<'_>) -> Result<Self, DecodeError> {
        match tag {
            TAG_TRANSCRIPT => {
                let track = TrackId::new(fields.u32()?);
                let start = SampleIndex::new(fields.u64()?);
                let end = SampleIndex::new(fields.u64()?);
                let range = SampleRange::new(start, end).ok_or(DecodeError::InvertedRange)?;
                let text = std::str::from_utf8(fields.rest())
                    .map_err(|_| DecodeError::NotUtf8)?
                    .to_owned();
                Ok(Self::Transcript(Transcript { track, range, text }))
            }
            TAG_CONFIRMED => {
                let track = TrackId::new(fields.u32()?);
                let up_to = SampleIndex::new(fields.u64()?);
                fields.end()?;
                Ok(Self::Confirmed { track, up_to })
            }
            other => Err(DecodeError::UnknownTag(other)),
        }
    }
}

/// The fields of a frame body after its tag, read front to back.
#[derive(Debug)]
pub struct Fields<'a> {
    bytes: &'a [u8],
}

impl<'a> Fields<'a> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let (head, tail) = self
            .bytes
            .split_first_chunk::<N>()
            .ok_or(DecodeError::Short)?;
        self.bytes = tail;
        Ok(*head)
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        self.take().map(u16::from_le_bytes)
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        self.take().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        self.take().map(u64::from_le_bytes)
    }

    /// Everything left.
    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.bytes)
    }

    /// Fails if anything is left.
    fn end(&self) -> Result<(), DecodeError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes)
        }
    }
}

/// Encodes `frame`, length prefix included.
///
/// # Errors
///
/// [`EncodeError::TooLarge`] if the body would pass [`MAX_BODY_LEN`]: audio
/// over [`MAX_AUDIO_SAMPLES`] or text over [`MAX_TEXT_LEN`].
pub fn encode<M: WireMessage>(frame: &Frame<M>) -> Result<Vec<u8>, EncodeError> {
    let mut out = vec![0; 4];
    match frame {
        Frame::Hello(version) => {
            out.push(TAG_HELLO);
            out.extend_from_slice(&MAGIC);
            out.extend_from_slice(&version.get().to_le_bytes());
        }
        Frame::Message(message) => message.encode_body(&mut out)?,
    }
    let body_len = out.len() - 4;
    // The message checks keep the body within the limit; this is the
    // backstop, so the length below always fits.
    let prefix = u32::try_from(body_len)
        .ok()
        .filter(|_| body_len <= MAX_BODY_LEN)
        .ok_or(EncodeError::TooLarge)?;
    out[..4].copy_from_slice(&prefix.to_le_bytes());
    Ok(out)
}

/// Parses one frame body (the bytes after the length prefix).
///
/// # Errors
///
/// A [`DecodeError`] naming the first thing wrong with it.
pub fn decode_body<M: WireMessage>(body: &[u8]) -> Result<Frame<M>, DecodeError> {
    let (&tag, rest) = body.split_first().ok_or(DecodeError::Empty)?;
    let mut fields = Fields { bytes: rest };
    if tag == TAG_HELLO {
        if fields.take::<4>()? != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        let version = ProtocolVersion::new(fields.u16()?);
        fields.end()?;
        return Ok(Frame::Hello(version));
    }
    M::decode_body(tag, &mut fields).map(Frame::Message)
}

/// Writes `frame` to `out` in one `write_all` and flushes, so a frame is
/// never left half in a buffer.
///
/// # Errors
///
/// An encoding error (as [`io::ErrorKind::InvalidInput`]) or the writer's.
pub fn write_frame<M: WireMessage>(out: &mut impl Write, frame: &Frame<M>) -> io::Result<()> {
    let bytes = encode(frame).map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
    out.write_all(&bytes)?;
    out.flush()
}

/// Reads frames from a stream until it ends or a frame is bad.
#[derive(Debug)]
pub struct FrameReader<R> {
    input: R,
    /// Set once a read has failed: the stream's position inside a frame is
    /// then unknown, so nothing more is read from it.
    failed: bool,
}

impl<R: Read> FrameReader<R> {
    /// A reader at the start of `input`, which must be at a frame boundary.
    pub const fn new(input: R) -> Self {
        Self {
            input,
            failed: false,
        }
    }

    /// The next frame, or `Ok(None)` if the stream ended cleanly between
    /// frames.
    ///
    /// # Errors
    ///
    /// A [`ReadError`] if the stream fails, ends inside a frame, or holds a
    /// frame that doesn't parse. After an error every later call fails
    /// with [`ReadError::Failed`]: the reader can't find the next frame
    /// boundary, and a guess could misread audio as text.
    pub fn read_frame<M: WireMessage>(&mut self) -> Result<Option<Frame<M>>, ReadError> {
        if self.failed {
            return Err(ReadError::Failed);
        }
        let result = self.read_inner();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn read_inner<M: WireMessage>(&mut self) -> Result<Option<Frame<M>>, ReadError> {
        let mut prefix = [0_u8; 4];
        let got = read_full(&mut self.input, &mut prefix)?;
        if got == 0 {
            return Ok(None);
        }
        if got < prefix.len() {
            return Err(ReadError::Truncated);
        }
        let len = u32::from_le_bytes(prefix);
        let body_len = usize::try_from(len)
            .ok()
            .filter(|&len| len <= MAX_BODY_LEN)
            .ok_or(ReadError::TooLarge(len))?;
        let mut body = vec![0; body_len];
        if read_full(&mut self.input, &mut body)? < body_len {
            return Err(ReadError::Truncated);
        }
        decode_body(&body).map(Some).map_err(ReadError::Decode)
    }
}

/// Reads until `buf` is full or the input ends, returning how much was read.
fn read_full(input: &mut impl Read, buf: &mut [u8]) -> Result<usize, ReadError> {
    let mut filled = 0;
    while filled < buf.len() {
        match input.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(ReadError::Io(err)),
        }
    }
    Ok(filled)
}

/// A frame couldn't be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// The body would pass [`MAX_BODY_LEN`].
    TooLarge,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => write!(f, "frame body over {MAX_BODY_LEN} bytes"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// What's wrong with a frame body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// The body has no tag.
    Empty,
    /// The tag isn't one this direction carries.
    UnknownTag(u8),
    /// A field runs past the end of the body.
    Short,
    /// Bytes follow the last field of a fixed-size message.
    TrailingBytes,
    /// A handshake without nota's magic bytes.
    BadMagic,
    /// Audio whose sample bytes don't divide into 16-bit samples.
    OddAudioLength,
    /// A sampling rate of zero or above [`SampleRate::MAX_HZ`].
    BadRate(u32),
    /// Audio whose last sample's index would overflow.
    RangeOverflow,
    /// A transcript range that ends before it starts.
    InvertedRange,
    /// Transcript text that isn't UTF-8.
    NotUtf8,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty frame body"),
            Self::UnknownTag(tag) => write!(f, "unknown tag {tag:#04x}"),
            Self::Short => write!(f, "frame body too short for its fields"),
            Self::TrailingBytes => write!(f, "bytes after the last field"),
            Self::BadMagic => write!(f, "handshake without nota's magic bytes"),
            Self::OddAudioLength => write!(f, "audio bytes not a whole number of samples"),
            Self::BadRate(hz) => write!(f, "sampling rate {hz} Hz out of range"),
            Self::RangeOverflow => write!(f, "audio sample index overflows"),
            Self::InvertedRange => write!(f, "transcript range ends before it starts"),
            Self::NotUtf8 => write!(f, "transcript text isn't UTF-8"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Why reading a frame failed.
#[derive(Debug)]
pub enum ReadError {
    /// The stream failed.
    Io(io::Error),
    /// The stream ended inside a frame.
    Truncated,
    /// A length prefix over [`MAX_BODY_LEN`].
    TooLarge(u32),
    /// A frame that doesn't parse.
    Decode(DecodeError),
    /// An earlier read failed, so the stream can't be read any further.
    Failed,
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "engine stream failed: {err}"),
            Self::Truncated => write!(f, "engine stream ended inside a frame"),
            Self::TooLarge(len) => write!(f, "frame length {len} over {MAX_BODY_LEN}"),
            Self::Decode(err) => write!(f, "bad frame: {err}"),
            Self::Failed => write!(f, "engine stream already failed"),
        }
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Decode(err) => Some(err),
            Self::Truncated | Self::TooLarge(_) | Self::Failed => None,
        }
    }
}

#[cfg(test)]
mod props;

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(first: u64, samples: Vec<i16>) -> AudioChunk {
        AudioChunk::new(
            TrackId::new(7),
            SampleIndex::new(first),
            SampleRate::SPEECH,
            samples,
        )
        .unwrap()
    }

    #[test]
    fn hello_layout() {
        let bytes = encode::<ToEngine>(&Frame::Hello(ProtocolVersion::CURRENT)).unwrap();
        assert_eq!(bytes, [7, 0, 0, 0, 0x00, b'n', b'o', b't', b'a', 0, 0]);
    }

    #[test]
    fn audio_layout() {
        let frame = Frame::Message(ToEngine::Audio(chunk(0x0102, vec![1, -2])));
        let bytes = encode(&frame).unwrap();
        let mut want = vec![21, 0, 0, 0, 0x01, 7, 0, 0, 0];
        want.extend_from_slice(&0x0102_u64.to_le_bytes());
        want.extend_from_slice(&16_000_u32.to_le_bytes());
        want.extend_from_slice(&[1, 0, 0xfe, 0xff]);
        assert_eq!(bytes, want);
        assert_eq!(decode_body::<ToEngine>(&bytes[4..]), Ok(frame));
    }

    #[test]
    fn confirmed_layout() {
        let frame = Frame::Message(FromEngine::Confirmed {
            track: TrackId::new(1),
            up_to: SampleIndex::new(300),
        });
        let bytes = encode(&frame).unwrap();
        assert_eq!(
            bytes,
            [13, 0, 0, 0, 0x82, 1, 0, 0, 0, 0x2c, 1, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn a_frame_sent_the_wrong_way_is_refused() {
        let flush = encode(&Frame::Message(ToEngine::Flush {
            track: TrackId::new(0),
        }))
        .unwrap();
        assert_eq!(
            decode_body::<FromEngine>(&flush[4..]),
            Err(DecodeError::UnknownTag(TAG_FLUSH))
        );
    }

    #[test]
    fn bad_handshakes() {
        assert_eq!(
            decode_body::<ToEngine>(&[0x00, b'n', b'o', b't', b'e', 0, 0]),
            Err(DecodeError::BadMagic)
        );
        assert_eq!(
            decode_body::<ToEngine>(&[0x00, b'n', b'o', b't', b'a', 0]),
            Err(DecodeError::Short)
        );
        assert_eq!(
            decode_body::<ToEngine>(&[0x00, b'n', b'o', b't', b'a', 0, 0, 9]),
            Err(DecodeError::TrailingBytes)
        );
        assert_eq!(
            decode_body::<FromEngine>(&[0x00, b'n', b'o', b't', b'a', 3, 1]),
            Ok(Frame::Hello(ProtocolVersion::new(0x0103)))
        );
    }

    #[test]
    fn bad_fields() {
        assert_eq!(decode_body::<ToEngine>(&[]), Err(DecodeError::Empty));
        // Audio at a zero rate, and with an odd byte.
        let mut audio = vec![0x01, 0, 0, 0, 0];
        audio.extend_from_slice(&0_u64.to_le_bytes());
        let mut zero_rate = audio.clone();
        zero_rate.extend_from_slice(&0_u32.to_le_bytes());
        assert_eq!(
            decode_body::<ToEngine>(&zero_rate),
            Err(DecodeError::BadRate(0))
        );
        audio.extend_from_slice(&16_000_u32.to_le_bytes());
        audio.push(1);
        assert_eq!(
            decode_body::<ToEngine>(&audio),
            Err(DecodeError::OddAudioLength)
        );
        // Audio whose end overflows.
        let mut overflow = vec![0x01, 0, 0, 0, 0];
        overflow.extend_from_slice(&u64::MAX.to_le_bytes());
        overflow.extend_from_slice(&16_000_u32.to_le_bytes());
        overflow.extend_from_slice(&[0, 0]);
        assert_eq!(
            decode_body::<ToEngine>(&overflow),
            Err(DecodeError::RangeOverflow)
        );
        // A transcript that ends before it starts, and one that isn't text.
        let mut transcript = vec![0x81, 0, 0, 0, 0];
        transcript.extend_from_slice(&5_u64.to_le_bytes());
        transcript.extend_from_slice(&4_u64.to_le_bytes());
        assert_eq!(
            decode_body::<FromEngine>(&transcript),
            Err(DecodeError::InvertedRange)
        );
        let mut not_text = vec![0x81, 0, 0, 0, 0];
        not_text.extend_from_slice(&4_u64.to_le_bytes());
        not_text.extend_from_slice(&5_u64.to_le_bytes());
        not_text.push(0xff);
        assert_eq!(
            decode_body::<FromEngine>(&not_text),
            Err(DecodeError::NotUtf8)
        );
    }

    #[test]
    fn too_large_to_encode() {
        let audio = chunk(0, vec![0; MAX_AUDIO_SAMPLES + 1]);
        assert_eq!(
            encode(&Frame::Message(ToEngine::Audio(audio))),
            Err(EncodeError::TooLarge)
        );
        let fits = chunk(0, vec![0; MAX_AUDIO_SAMPLES]);
        let bytes = encode(&Frame::Message(ToEngine::Audio(fits))).unwrap();
        assert!(bytes.len() - 4 <= MAX_BODY_LEN);
        let text = Transcript {
            track: TrackId::new(0),
            range: SampleRange::new(SampleIndex::ZERO, SampleIndex::ZERO).unwrap(),
            text: "x".repeat(MAX_TEXT_LEN + 1),
        };
        assert_eq!(
            encode(&Frame::Message(FromEngine::Transcript(text))),
            Err(EncodeError::TooLarge)
        );
    }

    #[test]
    fn reader_refuses_a_huge_length_without_allocating_it() {
        let bytes = u32::MAX.to_le_bytes();
        let mut reader = FrameReader::new(&bytes[..]);
        assert!(matches!(
            reader.read_frame::<FromEngine>(),
            Err(ReadError::TooLarge(u32::MAX))
        ));
        assert!(matches!(
            reader.read_frame::<FromEngine>(),
            Err(ReadError::Failed)
        ));
    }

    #[test]
    fn reader_ends_cleanly_only_between_frames() {
        let bytes = encode::<FromEngine>(&Frame::Hello(ProtocolVersion::CURRENT)).unwrap();
        let mut reader = FrameReader::new(&bytes[..]);
        assert_eq!(
            reader.read_frame::<FromEngine>().unwrap(),
            Some(Frame::Hello(ProtocolVersion::CURRENT))
        );
        assert!(reader.read_frame::<FromEngine>().unwrap().is_none());

        for cut in 1..bytes.len() {
            let mut reader = FrameReader::new(&bytes[..cut]);
            assert!(
                matches!(reader.read_frame::<FromEngine>(), Err(ReadError::Truncated)),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn write_frame_writes_the_encoding() {
        let frame = Frame::Message(ToEngine::Flush {
            track: TrackId::new(2),
        });
        let mut out = Vec::new();
        write_frame(&mut out, &frame).unwrap();
        assert_eq!(out, encode(&frame).unwrap());

        let too_big = Frame::Message(ToEngine::Audio(chunk(0, vec![0; MAX_AUDIO_SAMPLES + 1])));
        let err = write_frame(&mut Vec::new(), &too_big).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
