//! The real engine child (`nota engine asr`) under the recorder's
//! supervisor: killed with SIGKILL mid-chunk, it's restarted, no audio goes
//! untranscribed, and text resumes within 10 s. Skips (and says so on
//! stderr) when the test models or the fixture are absent, and fails instead
//! when `NOTA_REQUIRE_TEST_MODELS=1`, as in CI; see
//! `scripts/fetch-test-models.sh`.

// Test code throughout: clippy allows unwraps and panics in it.
#![cfg(test)]

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use nota_core::messages::AudioChunk;
use nota_core::{Clock, SampleIndex, SampleRate, SystemClock, TrackId};
use nota_recorder::engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineStatus, EngineStderr, EngineSupervisor,
    OfflineReason,
};

fn test_models() -> Option<PathBuf> {
    let root = std::env::var_os("NOTA_TEST_MODELS")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/nota/test-models"))
        })
        .unwrap_or_default();
    let present = root
        .join("parakeet-tdt-0.6b-v3-int8/encoder.int8.onnx")
        .is_file()
        && root.join("silero_vad_v6.onnx").is_file()
        && root.join("fixtures/invented-lecture.wav").is_file();
    if !present {
        assert!(
            std::env::var_os("NOTA_REQUIRE_TEST_MODELS").is_none_or(|v| v != "1"),
            "NOTA_REQUIRE_TEST_MODELS=1 but no test models in {} (run scripts/fetch-test-models.sh)",
            root.display()
        );
        let _ = writeln!(
            std::io::stderr(),
            "skipped: no test models in {} (run scripts/fetch-test-models.sh)",
            root.display()
        );
    }
    present.then_some(root)
}

/// The samples of a 16 kHz mono 16-bit PCM WAV file.
fn read_wav(path: &Path) -> Vec<i16> {
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    let mut at = 12;
    let mut format = None;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let body = &bytes[at + 8..at + 8 + len];
        if id == b"fmt " {
            let channels = u16::from_le_bytes([body[2], body[3]]);
            let rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            let bits = u16::from_le_bytes([body[14], body[15]]);
            format = Some((channels, rate, bits));
        } else if id == b"data" {
            assert_eq!(format, Some((1, 16_000, 16)));
            return body
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&p| i16::from_le_bytes(p))
                .collect();
        }
        at += 8 + len + len % 2;
    }
    panic!("no data chunk in {}", path.display());
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

/// The real engine, from this build, with the shipped timings.
fn engine_config(root: &Path) -> EngineConfig {
    EngineConfig::new(EngineCommand {
        program: PathBuf::from(env!("CARGO_BIN_EXE_nota")),
        args: [
            "engine",
            "asr",
            "--parakeet",
            root.join("parakeet-tdt-0.6b-v3-int8").to_str().unwrap(),
            "--vad",
            root.join("silero_vad_v6.onnx").to_str().unwrap(),
            "--threads",
            "2",
        ]
        .into_iter()
        .map(OsString::from)
        .collect(),
        stderr: EngineStderr::Null,
    })
}

/// Acceptance (GAI-128) with the real engine.
#[test]
fn killed_mid_chunk_the_real_engine_resumes_without_losing_audio() {
    let Some(root) = test_models() else {
        return;
    };
    let samples = read_wav(&root.join("fixtures/invented-lecture.wav"));
    let track = TrackId::new(0);
    let config = engine_config(&root);
    let clock = Arc::new(SystemClock::start().unwrap());
    let (mut supervisor, events) = EngineSupervisor::start(config, clock.clone()).unwrap();
    let mut seen = Vec::new();
    let mut next_event = |within: Duration| {
        let event = events
            .recv_timeout(within)
            .unwrap_or_else(|_| panic!("timed out"));
        seen.push((clock.now(), event.clone()));
        event
    };

    let pid = loop {
        if let EngineEvent::Status(EngineStatus::Online { pid }) =
            next_event(Duration::from_secs(60))
        {
            break pid;
        }
    };

    // The first 8 s, then wait for the first text.
    let pieces: Vec<&[i16]> = samples.chunks(1_600).collect();
    let mut sent = 0;
    let mut feed = |supervisor: &mut EngineSupervisor, k: usize| {
        let chunk = AudioChunk::new(
            track,
            SampleIndex::new(sent),
            SampleRate::SPEECH,
            pieces[k].to_vec(),
        )
        .unwrap();
        sent += pieces[k].len() as u64;
        supervisor.send_audio(chunk).unwrap();
    };
    for k in 0..80 {
        feed(&mut supervisor, k);
    }
    let sent_at_kill = 80 * 1_600;
    // The supervisor sends each text before the confirmation that releases
    // it, so wait for that confirmation too.
    let mut confirmed;
    let mut heard = false;
    loop {
        match next_event(Duration::from_secs(30)) {
            EngineEvent::Transcript(_) => heard = true,
            EngineEvent::Confirmed { up_to, .. } => {
                confirmed = up_to.get();
                if heard {
                    break;
                }
            }
            _ => {}
        }
    }

    // Mid-chunk: the engine holds audio it hasn't confirmed.
    assert!(confirmed < sent_at_kill, "{confirmed}");
    let status = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let killed_at = clock.now();
    for k in 80..pieces.len() {
        feed(&mut supervisor, k);
    }
    supervisor.flush(track);

    // Down, then up again, then text again.
    let mut resumed = None;
    let mut offline = false;
    let total = samples.len() as u64;
    while confirmed < total {
        match next_event(Duration::from_secs(30)) {
            EngineEvent::Status(EngineStatus::Offline(reason)) => {
                assert!(matches!(reason, OfflineReason::Exited(_)), "{reason:?}");
                offline = true;
            }
            EngineEvent::Transcript(_) if offline && resumed.is_none() => {
                resumed = Some(clock.now());
            }
            EngineEvent::Confirmed { up_to, .. } => {
                if !offline {
                    // Confirmed by the old engine after the kill was sent.
                    assert!(up_to.get() < sent_at_kill);
                }
                confirmed = up_to.get();
            }
            _ => {}
        }
    }
    assert!(offline);
    // Every sample was dealt with: none skipped, all confirmed.
    assert!(
        !seen
            .iter()
            .any(|(_, e)| matches!(e, EngineEvent::Skipped { .. })),
        "audio skipped"
    );
    let took = resumed.unwrap().checked_duration_since(killed_at).unwrap();
    assert!(
        took < Duration::from_secs(10),
        "text resumed after {took:?}"
    );

    assert_text_reads_as_the_fixture(&seen);
}

/// The text runs in order without overlap and reads as the fixture.
fn assert_text_reads_as_the_fixture(seen: &[(nota_core::SessionTime, EngineEvent)]) {
    let mut last_end = 0;
    let mut text = Vec::new();
    for (_, event) in seen {
        if let EngineEvent::Transcript(t) = event {
            assert!(t.range.start().get() >= last_end, "overlapping text");
            last_end = t.range.end().get();
            text.push(t.text.clone());
        }
    }
    let heard = words(&text.join(" "));
    let said = words(include_str!(
        "../../nota-engine/tests/fixtures/invented-lecture.txt"
    ));
    let errors = distance(&heard, &said);
    assert!(errors * 10 <= said.len(), "{errors} errors: {heard:?}");
}
