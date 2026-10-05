//! A test double for the speech engine child, used by the supervisor's
//! integration tests. It speaks the engine protocol with no models behind it.
//!
//! Usage: `nota-fake-engine MODE [--every N] [--after K] [--delay-ms D] [--poison V] [--on-poison exit|hang]`
//!
//! `D` (`--delay-ms`, default 0) is how long every answer takes: a slow but
//! working engine. `--hello-delay-ms L` (default 0) is how long it takes to
//! say hello, as loading models does. With `--poison V`, any mode exits 101 on receiving audio
//! that holds the sample value `V`, as an engine might abort on some input;
//! with `--on-poison hang` it goes silent instead.
//!
//! `K` (`--after`, default 0) counts the audio frames received.
//!
//! | Mode | Behaviour |
//! |---|---|
//! | `echo` | A good engine: every `N` audio frames per track (`--every`, default 1) it answers the buffered audio with a transcript of its range, text `"start-end"`, then a confirmation. `Flush` answers what is buffered and ends the track's stream. Exits 3 if a track's audio isn't contiguous. |
//! | `text-only` | `echo`, but never confirms: it sends only the transcripts. |
//! | `hang` | Sends `Hello`, then never writes again. |
//! | `hang-after` | `echo` for `K` audio frames, then goes silent. |
//! | `crash-after` | `echo`; exits 101 when audio frame `K + 1` arrives, unanswered. |
//! | `garbage-after` | `echo` for `K` audio frames; then writes an over-long length prefix and goes silent. |
//! | `torn-after` | `echo` for `K` audio frames; then writes half a frame and exits 101, as a crash mid-write would. |
//! | `bad-transcript` | Answers the first audio frame with a confirmation far past the audio sent, then goes silent. |
//! | `deaf` | Sends `Hello`, then never reads stdin again, so closing it doesn't stop it. |
//! | `no-hello` | Never sends `Hello`. |
//! | `wrong-version` | Sends a `Hello` with the wrong protocol version. |
//!
//! Every mode first requires the recorder's `Hello` (else exit 2) and exits 0
//! when stdin ends. Bad arguments exit 2. Like the real engine, every mode
//! first ties itself to the recorder ([`nota_core::lifeline`]), so on Linux
//! it dies with the recorder even while it sleeps, and exits 0 at once if
//! the recorder has already gone (exit 2 if the recorder's process id it's
//! given isn't one).

use std::collections::BTreeMap;
use std::io::{self, BufReader, Read, Write};
use std::process::ExitCode;

use nota_core::lifeline::{Tie, tie_to_recorder};
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
    TextOnly,
    Hang,
    HangAfter,
    CrashAfter,
    GarbageAfter,
    TornAfter,
    BadTranscript,
    NoHello,
    Deaf,
    WrongVersion,
}

impl Mode {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "echo" => Self::Echo,
            "text-only" => Self::TextOnly,
            "hang" => Self::Hang,
            "hang-after" => Self::HangAfter,
            "crash-after" => Self::CrashAfter,
            "garbage-after" => Self::GarbageAfter,
            "torn-after" => Self::TornAfter,
            "bad-transcript" => Self::BadTranscript,
            "no-hello" => Self::NoHello,
            "deaf" => Self::Deaf,
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
    delay_ms: u64,
    hello_delay_ms: u64,
    poison: Option<i16>,
    /// Whether poison hangs the engine rather than killing it.
    poison_hangs: bool,
}

impl Args {
    fn parse(args: impl IntoIterator<Item = String>) -> Option<Self> {
        let mut args = args.into_iter();
        let mode = Mode::parse(&args.next()?)?;
        let mut parsed = Self {
            mode,
            every: 1,
            after: 0,
            delay_ms: 0,
            hello_delay_ms: 0,
            poison: None,
            poison_hangs: false,
        };
        while let Some(flag) = args.next() {
            let raw = args.next()?;
            if flag == "--poison" {
                parsed.poison = Some(raw.parse().ok()?);
                continue;
            }
            if flag == "--on-poison" {
                parsed.poison_hangs = match raw.as_str() {
                    "exit" => false,
                    "hang" => true,
                    _ => return None,
                };
                continue;
            }
            let value: u64 = raw.parse().ok()?;
            match flag.as_str() {
                "--every" if value > 0 => parsed.every = value,
                "--after" => parsed.after = value,
                "--delay-ms" => parsed.delay_ms = value,
                "--hello-delay-ms" => parsed.hello_delay_ms = value,
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
        slow_down(self.args.hello_delay_ms);
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
            ToEngine::Flush { track } => {
                self.answer(track, end_of(&self.tracks, track))?;
                // As the real engine: the next audio may start anywhere.
                self.tracks.remove(&track);
            }
            ToEngine::Audio(chunk) => {
                self.audio_frames += 1;
                let range = chunk.range();
                if self
                    .args
                    .poison
                    .is_some_and(|v| chunk.samples().contains(&v))
                {
                    if self.args.poison_hangs {
                        self.silent = true;
                        return Ok(Step::Continue);
                    }
                    return Ok(Step::Exit(EXIT_CRASH));
                }
                if self.audio_frames > self.args.after {
                    match self.args.mode {
                        Mode::CrashAfter => return Ok(Step::Exit(EXIT_CRASH)),
                        Mode::HangAfter => {
                            self.silent = true;
                            return Ok(Step::Continue);
                        }
                        Mode::TornAfter => {
                            // The first bytes of a confirmation's frame.
                            self.out.write_all(&[13, 0, 0, 0, 0x82, 0])?;
                            self.out.flush()?;
                            return Ok(Step::Exit(EXIT_CRASH));
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
        if self.args.delay_ms > 0 {
            slow_down(self.args.delay_ms);
        }
        let text = format!("{}-{}", start.get(), end.get());
        let Some(transcript) = Transcript::new(track, range, text) else {
            return Ok(());
        };
        self.send(FromEngine::Transcript(transcript))?;
        if self.args.mode == Mode::TextOnly {
            return Ok(());
        }
        self.send(FromEngine::Confirmed { track, up_to: end })
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "a test double standing in for an engine that takes this long to decode"
)]
fn slow_down(ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

/// Where a track's received audio ends.
fn end_of(tracks: &BTreeMap<TrackId, Track>, track: TrackId) -> Option<SampleIndex> {
    tracks.get(&track).and_then(|state| state.next)
}

fn run(args: Args, input: impl Read, output: impl Write) -> u8 {
    let args_mode = args.mode;
    let mut reader = FrameReader::new(BufReader::new(input));
    match reader.read_frame::<ToEngine>() {
        Ok(Some(Frame::Hello(version))) if version == ProtocolVersion::CURRENT => {}
        _ => return EXIT_USAGE,
    }
    let mut engine = Engine::new(args, output);
    if engine.hello().is_err() {
        return EXIT_USAGE;
    }
    if args_mode == Mode::Deaf {
        loop {
            std::thread::park();
        }
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
    match tie_to_recorder() {
        Ok(Tie::Tied | Tie::Unsupported) => {}
        Ok(Tie::Orphaned) => return ExitCode::SUCCESS,
        Err(_) => return ExitCode::from(EXIT_USAGE),
    }
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
                after: 0,
                delay_ms: 0,
                hello_delay_ms: 0,
                poison: None,
                poison_hangs: false,
            })
        );
        assert_eq!(
            parse(&[
                "crash-after",
                "--after",
                "3",
                "--every",
                "2",
                "--delay-ms",
                "5",
                "--poison",
                "-7",
                "--on-poison",
                "hang",
            ]),
            Some(Args {
                mode: Mode::CrashAfter,
                every: 2,
                after: 3,
                delay_ms: 5,
                hello_delay_ms: 0,
                poison: Some(-7),
                poison_hangs: true,
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
        assert_eq!(parse(&["echo", "--on-poison", "sulk"]), None);
        assert_eq!(
            parse(&["text-only", "--on-poison", "exit"]).map(|a| (a.mode, a.poison_hangs)),
            Some((Mode::TextOnly, false))
        );
    }
}
