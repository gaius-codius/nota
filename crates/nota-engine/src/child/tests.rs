//! The child loop's protocol behaviour, with fake models: a detector that
//! calls any non-zero sample speech, and a recogniser that counts it.

use std::io;

use nota_core::protocol::{Frame, FrameReader, encode};
use nota_core::{SampleIndex, SampleRange, SampleRate, TrackId};

use super::*;

#[derive(Debug, Default)]
struct FakeModels {
    transcribed: Vec<usize>,
}

/// Non-zero samples are speech. A run of them is reported once a zero
/// follows it.
#[derive(Debug)]
struct FakeDetector {
    next: u64,
    run_start: Option<u64>,
}

impl Detector for FakeDetector {
    fn accept(&mut self, samples: &[f32]) -> Labels {
        let mut labels = Labels::default();
        for &s in samples {
            match (s != 0.0, self.run_start) {
                (true, None) => self.run_start = Some(self.next),
                (false, Some(from)) => {
                    labels.segments.push(range(from, self.next));
                    self.run_start = None;
                }
                _ => {}
            }
            self.next += 1;
        }
        labels.silent_until = SampleIndex::new(self.run_start.unwrap_or(self.next));
        labels
    }

    fn flush(&mut self) -> Labels {
        let mut labels = Labels::default();
        if let Some(from) = self.run_start.take() {
            labels.segments.push(range(from, self.next));
        }
        labels.silent_until = SampleIndex::new(self.next);
        labels
    }
}

impl Transcriber for FakeModels {
    fn transcribe(&mut self, audio: &[f32]) -> String {
        self.transcribed.push(audio.len());
        let loud = audio.iter().filter(|&&s| s != 0.0).count();
        if loud == 0 {
            String::new()
        } else {
            format!("{loud} loud")
        }
    }
}

impl Models for FakeModels {
    type Detector = FakeDetector;
    type Transcriber = Self;

    fn detector(&self, first: SampleIndex) -> Result<FakeDetector, EngineError> {
        Ok(FakeDetector {
            next: first.get(),
            run_start: None,
        })
    }

    fn transcriber(&mut self) -> &mut Self {
        self
    }
}

fn range(from: u64, to: u64) -> SampleRange {
    SampleRange::new(SampleIndex::new(from), SampleIndex::new(to)).unwrap()
}

const TRACK: TrackId = TrackId::new(3);

fn hello() -> Vec<u8> {
    encode::<ToEngine>(&Frame::Hello(ProtocolVersion::CURRENT)).unwrap()
}

fn audio(first: u64, rate: SampleRate, samples: Vec<i16>) -> Vec<u8> {
    let chunk = AudioChunk::new(TRACK, SampleIndex::new(first), rate, samples).unwrap();
    encode(&Frame::Message(ToEngine::Audio(chunk))).unwrap()
}

fn flush() -> Vec<u8> {
    encode(&Frame::Message(ToEngine::Flush { track: TRACK })).unwrap()
}

/// Runs the loop over `input`; returns its result and the frames it wrote.
fn run_on(input: &[u8]) -> (Result<(), EngineError>, Vec<Frame<FromEngine>>) {
    let mut output = Vec::new();
    let result = run(input, &mut output, || Ok(FakeModels::default()));
    let mut reader = FrameReader::new(&output[..]);
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_frame().unwrap() {
        frames.push(frame);
    }
    (result, frames)
}

/// `speech` samples of speech framed by `silence` samples of silence each
/// side, 16 kHz.
fn speech_between_silence(silence: usize, speech: usize) -> Vec<i16> {
    let mut samples = vec![0_i16; silence];
    samples.extend(std::iter::repeat_n(1_000, speech));
    samples.extend(std::iter::repeat_n(0, silence));
    samples
}

#[test]
fn transcribes_speech_and_confirms_everything_on_flush() {
    // 4 s of silence, 1 s of speech, 4 s of silence, from sample 1000.
    let samples = speech_between_silence(64_000, 16_000);
    let total = samples.len() as u64;
    let mut input = hello();
    for (k, part) in samples.chunks(1_600).enumerate() {
        input.extend(audio(
            1_000 + k as u64 * 1_600,
            SampleRate::SPEECH,
            part.to_vec(),
        ));
    }
    input.extend(flush());

    let (result, frames) = run_on(&input);
    result.unwrap();
    assert_eq!(frames[0], Frame::Hello(ProtocolVersion::CURRENT));

    let mut confirmed = 1_000;
    let mut text = Vec::new();
    for frame in &frames[1..] {
        match frame {
            Frame::Message(FromEngine::Confirmed { track, up_to }) => {
                assert_eq!(*track, TRACK);
                assert!(up_to.get() > confirmed, "confirmed goes forward");
                confirmed = up_to.get();
            }
            Frame::Message(FromEngine::Transcript(t)) => {
                assert_eq!(t.track(), TRACK);
                assert_eq!(
                    t.range().start().get(),
                    confirmed,
                    "text starts where the last confirmed did"
                );
                text.push((t.range(), t.text().to_owned()));
            }
            Frame::Hello(_) => panic!("second hello"),
        }
    }
    assert_eq!(confirmed, 1_000 + total);
    // One chunk holds all the speech, and only it is transcribed.
    assert_eq!(text.len(), 1, "{text:?}");
    assert_eq!(text[0].1, "16000 loud");
    assert!(text[0].0.contains(SampleIndex::new(1_000 + 64_000)));
    assert!(text[0].0.contains(SampleIndex::new(1_000 + 80_000 - 1)));
}

#[test]
fn silence_is_confirmed_without_transcribing() {
    let mut input = hello();
    for k in 0..100 {
        input.extend(audio(k * 1_600, SampleRate::SPEECH, vec![0; 1_600]));
    }
    let (result, frames) = run_on(&input);
    result.unwrap();
    let confirmed: Vec<u64> = frames[1..]
        .iter()
        .map(|frame| match frame {
            Frame::Message(FromEngine::Confirmed { up_to, .. }) => up_to.get(),
            other => panic!("{other:?}"),
        })
        .collect();
    // 10 s of silence, confirmed without a flush: a silent chunk is cut
    // in the middle of the pause once that's past the 3 s target, so at 3 s
    // and 6 s. The rest is confirmed when stdin closes.
    assert_eq!(confirmed, [48_000, 96_000, 160_000]);
}

#[test]
fn stdin_closing_ends_the_loop_cleanly() {
    let (result, frames) = run_on(&hello());
    result.unwrap();
    assert_eq!(frames, [Frame::Hello(ProtocolVersion::CURRENT)]);

    // Before the hello, too, without loading the models.
    let mut output = Vec::new();
    run(&[][..], &mut output, || -> Result<FakeModels, _> {
        panic!("loaded models with no recorder")
    })
    .unwrap();
    assert!(output.is_empty());
}

#[test]
fn stdin_closing_transcribes_what_each_track_still_holds() {
    let other = TrackId::new(4);
    let mut input = hello();
    input.extend(audio(0, SampleRate::SPEECH, vec![1_000; 1_600]));
    let chunk = AudioChunk::new(other, SampleIndex::new(50), SampleRate::SPEECH, vec![0; 10]);
    input.extend(encode(&Frame::Message(ToEngine::Audio(chunk.unwrap()))).unwrap());
    let (result, frames) = run_on(&input);
    result.unwrap();
    assert_eq!(
        frames[1..],
        [
            Frame::Message(FromEngine::Transcript(
                Transcript::new(TRACK, range(0, 1_600), "1600 loud".into()).unwrap()
            )),
            Frame::Message(FromEngine::Confirmed {
                track: TRACK,
                up_to: SampleIndex::new(1_600),
            }),
            Frame::Message(FromEngine::Confirmed {
                track: other,
                up_to: SampleIndex::new(60),
            }),
        ]
    );
}

#[test]
fn a_mismatched_or_missing_hello_is_refused_before_loading() {
    let refuse = |input: &[u8]| {
        let mut output = Vec::new();
        let result = run(input, &mut output, || -> Result<FakeModels, _> {
            panic!("loaded models before a good hello")
        });
        assert!(output.is_empty());
        result.unwrap_err()
    };
    let other = encode::<ToEngine>(&Frame::Hello(ProtocolVersion::new(9))).unwrap();
    assert!(matches!(refuse(&other), EngineError::Version(v) if v.get() == 9));
    assert!(matches!(refuse(&flush()), EngineError::Protocol(_)));
    assert!(matches!(refuse(&[1, 2]), EngineError::Read(_)));
}

#[test]
fn protocol_errors_stop_the_engine() {
    let check = |tail: Vec<u8>, want: &str| {
        let mut input = hello();
        input.extend(audio(0, SampleRate::SPEECH, vec![0; 10]));
        input.extend(tail);
        let (result, _) = run_on(&input);
        match result {
            Err(EngineError::Protocol(what)) => assert_eq!(what, want),
            other => panic!("{other:?}"),
        }
    };
    check(
        audio(11, SampleRate::SPEECH, vec![0; 10]),
        "audio doesn't follow on",
    );
    check(
        audio(9, SampleRate::SPEECH, vec![0; 10]),
        "audio doesn't follow on",
    );
    check(
        audio(10, SampleRate::new(48_000).unwrap(), vec![0; 10]),
        "audio not at 16 kHz",
    );
    check(hello(), "second hello");
}

#[test]
fn after_a_flush_a_track_starts_afresh() {
    let mut input = hello();
    input.extend(audio(0, SampleRate::SPEECH, vec![1_000; 10]));
    input.extend(flush());
    // A new run of audio, anywhere.
    input.extend(audio(500, SampleRate::SPEECH, vec![1_000; 10]));
    input.extend(flush());
    // A flush of a track with nothing buffered says nothing.
    input.extend(flush());
    let (result, frames) = run_on(&input);
    result.unwrap();
    let confirmed: Vec<_> = frames
        .iter()
        .filter_map(|f| match f {
            Frame::Message(FromEngine::Confirmed { up_to, .. }) => Some(up_to.get()),
            _ => None,
        })
        .collect();
    assert_eq!(confirmed, [10, 510]);
}

#[test]
fn a_failed_write_stops_the_engine() {
    struct Closed;
    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let result = run(&hello()[..], Closed, || Ok(FakeModels::default()));
    assert!(matches!(result, Err(EngineError::Write(_))));
}

#[test]
fn a_model_failure_is_returned() {
    let result = run(&hello()[..], Vec::new(), || -> Result<FakeModels, _> {
        Err(EngineError::Model("no model".into()))
    });
    assert!(matches!(result, Err(EngineError::Model(_))));
}
