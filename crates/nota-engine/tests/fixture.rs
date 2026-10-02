//! The real models on the invented-lecture fixture: speech the engine has
//! never heard, synthesized from `fixtures/invented-lecture.txt` by
//! `scripts/fetch-test-models.sh`. Skips (and says so on stderr) when the
//! models or the fixture are absent; they're never committed.

// Test code throughout: clippy allows unwraps and panics in it.
#![cfg(test)]

use std::io::Write;
use std::path::PathBuf;

use nota_core::messages::{AudioChunk, FromEngine, ProtocolVersion, ToEngine};
use nota_core::protocol::{Frame, FrameReader, encode};
use nota_core::{SampleIndex, SampleRate, TrackId};
use nota_engine::child;
use nota_engine::sherpa::{ModelPaths, SherpaModels};

/// The test-models directory, if it holds everything.
fn test_models() -> Option<(ModelPaths, PathBuf)> {
    let root = std::env::var_os("NOTA_TEST_MODELS")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/nota/test-models"))
        })?;
    let paths = ModelPaths {
        parakeet_dir: root.join("parakeet-tdt-0.6b-v3-int8"),
        vad_model: root.join("silero_vad_v6.onnx"),
        threads: 4,
    };
    let wav = root.join("fixtures/invented-lecture.wav");
    let present = paths.parakeet_dir.join("encoder.int8.onnx").is_file()
        && paths.vad_model.is_file()
        && wav.is_file();
    if !present {
        let _ = writeln!(
            std::io::stderr(),
            "skipped: no test models in {} (run scripts/fetch-test-models.sh)",
            root.display()
        );
    }
    present.then_some((paths, wav))
}

fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// Word-level edit distance.
fn distance(a: &[String], b: &[String]) -> usize {
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let here = (row[j + 1] + 1)
                .min(row[j] + 1)
                .min(prev + usize::from(x != y));
            prev = row[j + 1];
            row[j + 1] = here;
        }
    }
    row[b.len()]
}

#[test]
fn the_fixture_transcribes() {
    let Some((paths, wav)) = test_models() else {
        return;
    };
    let wave = sherpa_onnx::Wave::read(wav.to_str().unwrap()).unwrap();
    assert_eq!(wave.sample_rate(), 16_000);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the wave's samples are 16-bit values scaled by 1/32768, clamped back into range"
    )]
    let samples: Vec<i16> = wave
        .samples()
        .iter()
        .map(|&s| (s * 32_768.0).clamp(-32_768.0, 32_767.0) as i16)
        .collect();

    // As the recorder sends it: 100 ms at a time, from a non-zero position.
    let track = TrackId::new(0);
    let first = 48_000_u64;
    let mut input = encode::<ToEngine>(&Frame::Hello(ProtocolVersion::CURRENT)).unwrap();
    for (k, part) in samples.chunks(1_600).enumerate() {
        let at = SampleIndex::new(first + k as u64 * 1_600);
        let chunk = AudioChunk::new(track, at, SampleRate::SPEECH, part.to_vec()).unwrap();
        input.extend(encode(&Frame::Message(ToEngine::Audio(chunk))).unwrap());
    }
    input.extend(encode(&Frame::Message(ToEngine::Flush { track })).unwrap());

    let mut output = Vec::new();
    child::run(&input[..], &mut output, || SherpaModels::load(&paths)).unwrap();

    let mut reader = FrameReader::new(&output[..]);
    assert_eq!(
        reader.read_frame::<FromEngine>().unwrap(),
        Some(Frame::Hello(ProtocolVersion::CURRENT))
    );
    let mut texts = Vec::new();
    let mut confirmed = first;
    while let Some(frame) = reader.read_frame::<FromEngine>().unwrap() {
        match frame {
            Frame::Message(FromEngine::Transcript(t)) => {
                assert_eq!(t.range.start().get(), confirmed);
                assert!(t.range.len().get() <= 160_000, "a chunk over the 10 s cap");
                texts.push(t.text);
            }
            Frame::Message(FromEngine::Confirmed { up_to, .. }) => confirmed = up_to.get(),
            Frame::Hello(_) => panic!("second hello"),
        }
    }
    assert_eq!(confirmed, first + samples.len() as u64);
    // 17.6 s of speech with a 10 s cap: cut at least once.
    assert!(texts.len() >= 2, "{texts:?}");

    let heard = words(&texts.join(" "));
    let said = words(include_str!("fixtures/invented-lecture.txt"));
    let errors = distance(&heard, &said);
    // At most one word in twenty wrong.
    assert!(errors * 20 <= said.len(), "{errors} errors: {heard:?}");
}
