//! A test double for the speech engine child, used by the supervisor's
//! integration tests. It speaks the engine protocol with no models behind it.
//!
//! Usage: `nota-fake-engine MODE [--every N] [--after K]`
//!
//! `K` (`--after`, default 0) counts the audio frames received.
//!
//! | Mode | Behaviour |
//! |---|---|
//! | `echo` | A good engine: every `N` audio frames per track (`--every`, default 1) it answers the buffered audio with a transcript of its range, text `"start-end"`, then a confirmation. `Flush` answers what is buffered. Exits 3 if a track's audio isn't contiguous. |
//! | `hang` | Sends `Hello`, then never writes again. |
//! | `hang-after` | `echo` for `K` audio frames, then goes silent. |
//! | `crash-after` | `echo`; exits 101 when audio frame `K + 1` arrives, unanswered. |
//! | `garbage-after` | `echo` for `K` audio frames; then writes an over-long length prefix and goes silent. |
//! | `bad-transcript` | Answers the first audio frame with a confirmation far past the audio sent, then goes silent. |
//! | `no-hello` | Never sends `Hello`. |
//! | `wrong-version` | Sends a `Hello` with the wrong protocol version. |
//!
//! Every mode first requires the recorder's `Hello` (else exit 2) and exits 0
//! when stdin ends. Bad arguments exit 2.

use std::collections::BTreeMap;
use std::io::{self, BufReader, Read, Write};
use std::process::ExitCode;

use nota_core::messages::{FromEngine, ProtocolVersion, ToEngine, Transcript};
use nota_core::protocol::{Frame, FrameReader, write_frame};
use nota_core::{SampleCount, SampleIndex, SampleRange, TrackId};

/// Exit code for a bad handshake, bad arguments or a failed read or write.
const EXIT_USAGE: u8 = 2;
/// Exit code for audio that doesn't continue where the track left off.
const EXIT_GAP: u8 = 3;
/// Exit code of `crash-after`, the code a Rust panic exits with.
const EXIT_CRASH: u8 = 101;
/// A length prefix over the frame limit, then some filler.
const GARBAGE: [u8; 7] = [0xff, 0xff, 0xff, 0x7f, 1, 2, 3];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Echo,
    Hang,
    HangAfter,
    CrashAfter,
    GarbageAfter,
    BadTranscript,
    NoHello,
    WrongVersion,
}

impl Mode {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "echo" => Self::Echo,
            "hang" => Self::Hang,
            "hang-after" => Self::HangAfter,
            "crash-after" => Self::CrashAfter,
            "garbage-after" => Self::GarbageAfter,
            "bad-transcript" => Self::BadTranscript,
            "no-hello" => Self::NoHello,
            "wrong-version" => Self::WrongVersion,
            _ => return None,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Args {
    mode: Mode,
    every: u64,
    after: u64,
}

impl Args {
    fn parse(args: impl IntoIterator<Item = String>) -> Option<Self> {
        let mut args = args.into_iter();
        let mode = Mode::parse(&args.next()?)?;
        let mut parsed = Self {
            mode,
            every: 1,
            after: 0,
        };
        while let Some(flag) = args.next() {
            let value: u64 = args.next()?.parse().ok()?;
            match flag.as_str() {
                "--every" if value > 0 => parsed.every = value,
                "--after" => parsed.after = value,
                _ => return None,
            }
        }
        Some(parsed)
    }
}

/// One track's audio received but not yet answered.
struct Track {
    /// Where the next audio must start; `None` before the first audio.
    next: Option<SampleIndex>,
    /// The unanswered audio's start, and the frames it came in.
    pending: Option<(SampleIndex, u64)>,
}

/// What `echo` should do with the stream.
enum Step {
    Continue,
    Exit(u8),
}

struct Engine<W> {
    args: Args,
    out: W,
    tracks: BTreeMap<TrackId, Track>,
    audio_frames: u64,
    /// Whether the engine has stopped answering.
    silent: bool,
}

impl<W: Write> Engine<W> {
    fn new(args: Args, out: W) -> Self {
        let silent = matches!(args.mode, Mode::Hang | Mode::NoHello | Mode::WrongVersion);
        Self {
            args,
            out,
            tracks: BTreeMap::new(),
            audio_frames: 0,
            silent,
        }
    }

    fn send(&mut self, message: FromEngine) -> io::Result<()> {
        write_frame(&mut self.out, &Frame::Message(message))
    }

    fn hello(&mut self) -> io::Result<()> {
        let version = match self.args.mode {
            Mode::NoHello => return Ok(()),
            Mode::WrongVersion => ProtocolVersion::new(ProtocolVersion::CURRENT.get() + 1),
            _ => ProtocolVersion::CURRENT,
        };
        write_frame(&mut self.out, &Frame::<FromEngine>::Hello(version))
    }

    fn handle(&mut self, message: ToEngine) -> io::Result<Step> {
        if self.silent {
            return Ok(Step::Continue);
        }
        match message {
            ToEngine::Flush { track } => self.answer(track, end_of(&self.tracks, track))?,
            ToEngine::Audio(chunk) => {
                self.audio_frames += 1;
                let range = chunk.range();
                if self.audio_frames > self.args.after {
                    match self.args.mode {
                        Mode::CrashAfter => return Ok(Step::Exit(EXIT_CRASH)),
                        Mode::HangAfter => {
                            self.silent = true;
                            return Ok(Step::Continue);
                        }
                        Mode::GarbageAfter => {
                            self.out.write_all(&GARBAGE)?;
                            self.out.flush()?;
                            self.silent = true;
                            return Ok(Step::Continue);
                        }
                        Mode::BadTranscript => {
                            let far = SampleCount::new(chunk.samples().len() as u64 + 1_000_000);
                            let up_to = range.start().checked_add(far).unwrap_or(range.end());
                            self.send(FromEngine::Confirmed {
                                track: chunk.track(),
                                up_to,
                            })?;
                            self.silent = true;
                            return Ok(Step::Continue);
                        }
                        _ => {}
                    }
                }
                let track = self.tracks.entry(chunk.track()).or_insert(Track {
                    next: None,
                    pending: None,
                });
                if track.next.is_some_and(|next| next != range.start()) {
                    return Ok(Step::Exit(EXIT_GAP));
                }
                track.next = Some(range.end());
                let (start, frames) = track.pending.get_or_insert((range.start(), 0));
                let (start, frames) = (*start, *frames + 1);
                track.pending = Some((start, frames));
                if frames >= self.args.every {
                    self.answer(chunk.track(), Some(range.end()))?;
                }
            }
        }
        Ok(Step::Continue)
    }

    /// Answers the track's unanswered audio, if any, up to `end`.
    fn answer(&mut self, track: TrackId, end: Option<SampleIndex>) -> io::Result<()> {
        let Some(end) = end else { return Ok(()) };
        let Some(state) = self.tracks.get_mut(&track) else {
            return Ok(());
        };
        let Some((start, _)) = state.pending.take() else {
            return Ok(());
        };
        let Some(range) = SampleRange::new(start, end) else {
            return Ok(());
        };
        let text = format!("{}-{}", start.get(), end.get());
        self.send(FromEngine::Transcript(Transcript { track, range, text }))?;
        self.send(FromEngine::Confirmed { track, up_to: end })
    }
}

/// Where a track's received audio ends.
fn end_of(tracks: &BTreeMap<TrackId, Track>, track: TrackId) -> Option<SampleIndex> {
    tracks.get(&track).and_then(|state| state.next)
}

fn run(args: Args, input: impl Read, output: impl Write) -> u8 {
    let mut reader = FrameReader::new(BufReader::new(input));
    match reader.read_frame::<ToEngine>() {
        Ok(Some(Frame::Hello(version))) if version == ProtocolVersion::CURRENT => {}
        _ => return EXIT_USAGE,
    }
    let mut engine = Engine::new(args, output);
    if engine.hello().is_err() {
        return EXIT_USAGE;
    }
    loop {
        match reader.read_frame::<ToEngine>() {
            Ok(None) => return 0,
            Ok(Some(Frame::Hello(_))) => {}
            Ok(Some(Frame::Message(message))) => match engine.handle(message) {
                Ok(Step::Continue) => {}
                Ok(Step::Exit(code)) => return code,
                // The recorder went away; there is nobody to answer.
                Err(_) => return EXIT_USAGE,
            },
            Err(_) => return EXIT_USAGE,
        }
    }
}

fn main() -> ExitCode {
    let Some(args) = Args::parse(std::env::args().skip(1)) else {
        return ExitCode::from(EXIT_USAGE);
    };
    ExitCode::from(run(args, io::stdin().lock(), io::stdout().lock()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Option<Args> {
        Args::parse(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn parses_flags_and_defaults() {
        assert_eq!(
            parse(&["echo"]),
            Some(Args {
                mode: Mode::Echo,
                every: 1,
                after: 0
            })
        );
        assert_eq!(
            parse(&["crash-after", "--after", "3", "--every", "2"]),
            Some(Args {
                mode: Mode::CrashAfter,
                every: 2,
                after: 3
            })
        );
    }

    #[test]
    fn rejects_bad_arguments() {
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&["nope"]), None);
        assert_eq!(parse(&["echo", "--every", "0"]), None);
        assert_eq!(parse(&["echo", "--after"]), None);
        assert_eq!(parse(&["echo", "--bogus", "1"]), None);
    }
}
