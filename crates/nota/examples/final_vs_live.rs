//! Compare pause-cut transcription through nota's isolated engine.
//!
//! Usage: `cargo run -p nota --example final_vs_live -- PARAKEET_DIR VAD_MODEL
//! THREADS PCM_FILE [PCM_FILE ...]`. Inputs are mono 16 kHz signed 16-bit
//! little-endian PCM (convert WAV files before running). All paths come from
//! the caller; redirect stdout to a private bench directory. No audio or text
//! is written to the library. Output includes sample ranges and transcripts
//! for scoring against a separately reviewed term list.
//!
//! `t5-live` targets pauses after 8 s with a 10 s cap; `live` uses today's
//! production settings (3 s target, 10 s cap); `final` uses the production
//! final settings (15 s target, 25 s cap). All use the same engine and VAD,
//! fed in 100 ms frames and flushed at each input's end. Disagreement is
//! Levenshtein word distance divided by the final pass's word count, after
//! lowercasing and ignoring punctuation. It is agreement, not accuracy.

use std::error::Error;
use std::ffi::OsString;
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use nota_core::lifeline::RECORDER_PID_VAR;
use nota_core::messages::{AudioChunk, FromEngine, ProtocolVersion, ToEngine};
use nota_core::protocol::{Frame, FrameReader, write_frame};
use nota_core::{SampleCount, SampleIndex, SampleRate, TrackId};
use nota_engine::chunker::ChunkerConfig;
use nota_engine::sherpa::ModelPaths;

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

fn main() -> Result<()> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--child") {
        if args.len() != 5 {
            return Err("child expects pass, model directory, VAD and threads".into());
        }
        let paths = paths(&args[2..5])?;
        nota_engine::run_asr(&paths, config(&args[1].to_string_lossy())?)?;
        return Ok(());
    }
    if args.len() < 4 {
        return Err("usage: final_vs_live PARAKEET_DIR VAD_MODEL THREADS PCM_FILE [...]".into());
    }
    // Validate before starting any engine.
    paths(&args[..3])?;
    let mut out = io::stdout().lock();
    let mut totals = [(0, 0); 2];
    let executable = std::env::current_exe()?;
    for file in &args[3..] {
        let audio = pcm(Path::new(file))?;
        writeln!(
            out,
            "INPUT\t{}\t{} samples",
            Path::new(file).display(),
            audio.len()
        )?;
        let mut passes = Vec::new();
        for pass in ["t5-live", "live", "final"] {
            let text = transcribe(&executable, pass, &args[..3], &audio, &mut out)?;
            passes.push(words(&text));
        }
        for (index, pass) in ["t5-live", "live"].iter().enumerate() {
            let distance = distance(&passes[index], &passes[2]);
            let denominator = passes[2].len();
            totals[index].0 += distance;
            totals[index].1 += denominator;
            report(&mut out, pass, distance, denominator)?;
        }
    }
    for (pass, (edits, count)) in ["t5-live", "live"].iter().zip(totals) {
        write!(out, "TOTAL\t")?;
        report(&mut out, pass, edits, count)?;
    }
    Ok(())
}

fn paths(args: &[OsString]) -> Result<ModelPaths> {
    let threads: u16 = args[2].to_string_lossy().parse()?;
    if threads == 0 {
        return Err("threads must be positive".into());
    }
    Ok(ModelPaths {
        parakeet_dir: PathBuf::from(&args[0]),
        vad_model: PathBuf::from(&args[1]),
        threads,
    })
}

fn config(pass: &str) -> Result<ChunkerConfig> {
    match pass {
        "live" => Ok(ChunkerConfig::live(SampleRate::SPEECH)),
        "final" => Ok(ChunkerConfig::final_pass(SampleRate::SPEECH)),
        "t5-live" => ChunkerConfig::new(
            SampleCount::new(2_400),
            SampleCount::new(8 * 16_000),
            SampleCount::new(10 * 16_000),
            SampleCount::new(8 * 16_000),
            SampleCount::new(480),
        )
        .ok_or_else(|| "invalid T5 chunker config".into()),
        _ => Err("unknown pass".into()),
    }
}

fn pcm(file: &Path) -> Result<Vec<i16>> {
    let bytes = std::fs::read(file)?;
    if bytes.is_empty() || bytes.len() % 2 != 0 {
        return Err("PCM must be nonempty and contain whole 16-bit samples".into());
    }
    Ok(bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
        .collect())
}

fn transcribe(
    executable: &Path,
    pass: &str,
    model_args: &[OsString],
    audio: &[i16],
    out: &mut impl Write,
) -> Result<String> {
    let mut child = Command::new(executable)
        .arg("--child")
        .arg(pass)
        .args(model_args)
        .env(RECORDER_PID_VAR, std::process::id().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    // Drain replies while feeding, so neither pipe can deadlock when full.
    let result = thread::scope(|scope| -> Result<String> {
        let mut input = child.stdin.take().ok_or("no engine stdin")?;
        let output = child.stdout.take().ok_or("no engine stdout")?;
        let writer = scope.spawn(move || -> Result<()> {
            write_frame(
                &mut input,
                &Frame::<ToEngine>::Hello(ProtocolVersion::CURRENT),
            )?;
            let mut first = SampleIndex::ZERO;
            for samples in audio.chunks(1_600) {
                let chunk =
                    AudioChunk::new(TrackId::new(0), first, SampleRate::SPEECH, samples.to_vec())
                        .ok_or("sample index overflow")?;
                first = chunk.range().end();
                write_frame(&mut input, &Frame::Message(ToEngine::Audio(chunk)))?;
            }
            write_frame(
                &mut input,
                &Frame::Message(ToEngine::Flush {
                    track: TrackId::new(0),
                }),
            )?;
            Ok(())
        });
        let read = read_pass(BufReader::new(output), pass, audio.len(), out);
        // On a malformed reply or an output failure, terminate this exact
        // child before joining a writer that may be blocked on its pipe.
        if read.is_err() {
            let _ = child.kill();
        }
        writer.join().map_err(|_| "engine feeder thread failed")??;
        read
    });
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    if !status.success() {
        return Err(format!("{pass} engine exited {status}").into());
    }
    result
}

fn read_pass(
    input: impl io::Read,
    pass: &str,
    samples: usize,
    out: &mut impl Write,
) -> Result<String> {
    let mut reader = FrameReader::new(input);
    if reader.read_frame::<FromEngine>()? != Some(Frame::Hello(ProtocolVersion::CURRENT)) {
        return Err("engine did not send the expected hello".into());
    }
    let mut confirmed = SampleIndex::ZERO;
    let mut text = String::new();
    while let Some(frame) = reader.read_frame::<FromEngine>()? {
        match frame {
            Frame::Message(FromEngine::Transcript(t)) => {
                writeln!(
                    out,
                    "TEXT\t{pass}\t{}\t{}\t{}",
                    t.range().start().get(),
                    t.range().end().get(),
                    t.text()
                )?;
                text.push_str(t.text());
                text.push(' ');
            }
            Frame::Message(FromEngine::Confirmed { track, up_to }) => {
                if track != TrackId::new(0) || up_to < confirmed || up_to.get() > samples as u64 {
                    return Err("invalid engine confirmation".into());
                }
                confirmed = up_to;
            }
            Frame::Hello(_) => return Err("engine repeated hello".into()),
        }
    }
    if confirmed.get() != samples as u64 {
        return Err("engine did not confirm all input samples".into());
    }
    Ok(text)
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

fn distance(a: &[String], b: &[String]) -> usize {
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, left) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, right) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (above + 1)
                .min(row[j] + 1)
                .min(diagonal + usize::from(left != right));
            diagonal = above;
        }
    }
    row[b.len()]
}

fn report(out: &mut impl Write, pass: &str, edits: usize, count: usize) -> io::Result<()> {
    // An empty final transcript cannot establish the rule's <1% premise.
    if count == 0 {
        writeln!(out, "DISAGREEMENT\t{pass}\t{edits}\t{count}\tundefined")
    } else {
        #[expect(
            clippy::cast_precision_loss,
            reason = "bench word counts fit exactly in f64"
        )]
        let percent = 100.0 * edits as f64 / count as f64;
        writeln!(out, "DISAGREEMENT\t{pass}\t{edits}\t{count}\t{percent:.6}%")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_distance_counts_insertions_deletions_and_substitutions() {
        assert_eq!(distance(&words("a b c"), &words("a x c d")), 2);
        assert_eq!(distance(&words("a b c"), &[]), 3);
        assert_eq!(distance(&[], &words("a b")), 2);
        assert_eq!(distance(&words("a b a"), &words("b a b")), 2);
        assert_eq!(distance(&words("Same, words!"), &words("same words")), 0);
    }

    #[test]
    fn empty_final_is_not_zero_disagreement() {
        let mut out = Vec::new();
        report(&mut out, "live", 0, 0).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "DISAGREEMENT\tlive\t0\t0\tundefined\n"
        );
    }

    #[test]
    fn incomplete_engine_output_is_rejected() {
        let mut frames = Vec::new();
        write_frame(
            &mut frames,
            &Frame::<FromEngine>::Hello(ProtocolVersion::CURRENT),
        )
        .unwrap();
        assert!(read_pass(frames.as_slice(), "live", 160, &mut Vec::new()).is_err());
    }
}
