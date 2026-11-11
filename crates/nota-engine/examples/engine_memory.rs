//! T9: the engine's memory with Parakeet greedy and a hotword-boosted beam
//! recogniser loaded together, the time to recreate the boosted recogniser
//! with a new hotword list, and whether per-stream hotwords work.
//!
//! Usage: `cargo run --release -p nota-engine --example engine_memory --
//! PARAKEET_DIR VAD_MODEL WAV CHUNKS GLOSSARY GLOSSARY_VOCAB PACK PACK_VOCAB`.
//!
//! - `WAV` is mono 16 kHz speech; the first `CHUNKS` 25 s chunks of it are
//!   decoded (the final pass's cap), so the measured memory includes what
//!   onnxruntime grows on first inference.
//! - `GLOSSARY` and `PACK` are hotword lists, one term per line, lower
//!   case: a small glossary and a list the size of a term pack.
//! - Each `*_VOCAB` is the `bpe.vocab` sherpa-onnx splits that list's
//!   hotwords with (`piece<TAB>score` per line). A naive one is
//!   `awk '{print $1"\t-"$2}' tokens.txt`. It splits some words
//!   differently from Parakeet's own tokenizer, so hotwords boost less,
//!   but the graph is about the same size; for boosting that works, use a
//!   vocab whose scores make sherpa-onnx split every hotword as the
//!   tokenizer does.
//!
//! Every scenario runs in a fresh child process of this example, one after
//! another, so one scenario's memory never counts towards another's. The
//! greedy recogniser is loaded through [`SherpaModels::load`], as the
//! engine child loads it, with a voice activity detector open for each of
//! the two tracks; the beam recogniser takes the same model files, threads
//! and provider. The example holds the chunks it decodes, which the engine
//! child doesn't: each scenario's `start` reading, taken with the chunks
//! loaded, is that baseline. Each scenario prints `scenario key=value ...`
//! lines: memory in MiB from `/proc/self/status` (`VmRSS`, its peak
//! `VmHWM`, and `VmSwap`), times in ms. No transcript text is printed,
//! only counts.

use std::error::Error;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use nota_core::{Clock, SampleIndex, SystemClock};
use nota_engine::child::{Models, Transcriber};
use nota_engine::sherpa::{ModelPaths, SherpaModels, SileroDetector};
use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, Wave};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

/// The models' only sample rate.
const RATE_HZ: i32 = 16_000;

/// The final pass's chunk cap: 25 s at 16 kHz.
const CHUNK: usize = 25 * 16_000;

/// The engine child's default thread count (`nota engine asr --threads`).
const THREADS: u16 = 4;

/// The boost the bench found safest (11 of 21 terms at 2.0).
const BOOST: f32 = 2.0;

/// How often a recogniser or a stream is built when timing it.
const REPEATS: usize = 5;

/// The bench's beam width (`--max-active-paths=4`).
const BEAM_PATHS: i32 = 4;

/// How the example tells a child process which scenario to run.
const SCENARIO_FLAG: &str = "--scenario";

/// The tracks the engine child opens a detector for: mic and system.
const TRACKS: usize = 2;

/// The command line, parsed: every scenario's inputs.
#[derive(Debug, Clone)]
struct Inputs {
    /// Parakeet TDT 0.6B v3 int8.
    parakeet: PathBuf,
    /// Silero VAD.
    vad: PathBuf,
    /// The speech to decode.
    wav: PathBuf,
    /// How many 25 s chunks of it to decode.
    chunks: usize,
    /// The small glossary and its vocab.
    glossary: Hotwords,
    /// The pack-sized list and its vocab.
    pack: Hotwords,
}

/// A hotword list and the vocab sherpa-onnx splits it with.
#[derive(Debug, Clone)]
struct Hotwords {
    /// One term per line.
    list: PathBuf,
    /// `piece<TAB>score` per line.
    vocab: PathBuf,
}

fn main() -> Result<()> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|a| a == SCENARIO_FLAG) {
        let scenario = args
            .get(1)
            .and_then(|s| s.to_str())
            .ok_or("no scenario named")?;
        return run_scenario(scenario, &inputs(&args[2..])?);
    }
    // Check the arguments before starting any child.
    inputs(&args)?;
    let exe = std::env::current_exe()?;
    for scenario in [
        "greedy",
        "both-glossary",
        "both-pack",
        "recreate-glossary",
        "recreate-pack",
        "per-stream-glossary",
        "per-stream-pack",
    ] {
        let status = Command::new(&exe)
            .arg(SCENARIO_FLAG)
            .arg(scenario)
            .args(&args)
            .status()?;
        if !status.success() {
            return Err(format!("scenario {scenario} failed: {status}").into());
        }
    }
    Ok(())
}

/// Parses the command line after the program name (and scenario).
fn inputs(args: &[OsString]) -> Result<Inputs> {
    let [
        parakeet,
        vad,
        wav,
        chunks,
        glossary,
        glossary_vocab,
        pack,
        pack_vocab,
    ] = args
    else {
        return Err("usage: engine_memory PARAKEET_DIR VAD_MODEL WAV CHUNKS \
                    GLOSSARY GLOSSARY_VOCAB PACK PACK_VOCAB"
            .into());
    };
    let chunks = chunks
        .to_str()
        .and_then(|c| c.parse().ok())
        .filter(|&c: &usize| c > 0)
        .ok_or("CHUNKS takes a positive number")?;
    Ok(Inputs {
        parakeet: parakeet.into(),
        vad: vad.into(),
        wav: wav.into(),
        chunks,
        glossary: Hotwords {
            list: glossary.into(),
            vocab: glossary_vocab.into(),
        },
        pack: Hotwords {
            list: pack.into(),
            vocab: pack_vocab.into(),
        },
    })
}

/// Runs one scenario in this process.
fn run_scenario(scenario: &str, inputs: &Inputs) -> Result<()> {
    let audio = chunks(&inputs.wav, inputs.chunks)?;
    match scenario {
        "greedy" => greedy(inputs, &audio),
        "both-glossary" => both(scenario, inputs, &inputs.glossary, &audio),
        "both-pack" => both(scenario, inputs, &inputs.pack, &audio),
        "recreate-glossary" => recreate(scenario, inputs, &inputs.glossary),
        "recreate-pack" => recreate(scenario, inputs, &inputs.pack),
        "per-stream-glossary" => per_stream(scenario, inputs, &inputs.glossary, &audio),
        "per-stream-pack" => per_stream(scenario, inputs, &inputs.pack, &audio),
        other => Err(format!("no scenario {other}").into()),
    }
}

/// The baseline: the greedy recogniser alone, as the engine child holds
/// it today.
fn greedy(inputs: &Inputs, audio: &[Vec<f32>]) -> Result<()> {
    report("greedy", "start", &memory()?)?;
    let (mut models, _detectors) = engine(inputs)?;
    report("greedy", "loaded", &memory()?)?;
    for chunk in audio {
        models.transcriber().transcribe(chunk);
    }
    report("greedy", "decoded", &memory()?)
}

/// What the engine child holds while it records: the greedy recogniser
/// and a detector for each track.
fn engine(inputs: &Inputs) -> Result<(SherpaModels, Vec<SileroDetector>)> {
    let models = SherpaModels::load(&model_paths(inputs))?;
    let detectors = (0..TRACKS)
        .map(|_| models.detector(SampleIndex::ZERO))
        .collect::<std::result::Result<_, _>>()?;
    Ok((models, detectors))
}

/// Greedy and the boosted beam recogniser loaded together, each decoding
/// every chunk.
fn both(scenario: &str, inputs: &Inputs, hotwords: &Hotwords, audio: &[Vec<f32>]) -> Result<()> {
    report(scenario, "start", &memory()?)?;
    let (mut models, _detectors) = engine(inputs)?;
    report(scenario, "greedy-loaded", &memory()?)?;
    let beam = create(&beam_config(inputs, hotwords, true))?;
    report(scenario, "beam-loaded", &memory()?)?;
    for chunk in audio {
        models.transcriber().transcribe(chunk);
        decode(&beam, &beam.create_stream(), chunk);
    }
    report(scenario, "decoded", &memory()?)
}

/// The time to build the boosted recogniser with a hotword list, and to
/// build one stream with the list as per-stream hotwords.
fn recreate(scenario: &str, inputs: &Inputs, hotwords: &Hotwords) -> Result<()> {
    let clock = SystemClock::start()?;
    let mut builds = Vec::with_capacity(REPEATS);
    for _ in 0..REPEATS {
        let start = clock.now();
        let recognizer = create(&beam_config(inputs, hotwords, true))?;
        builds.push(since(&clock, start));
        drop(recognizer);
    }
    let (terms, count) = per_stream_list(&hotwords.list)?;
    let plain = create(&beam_config(inputs, hotwords, false))?;
    let mut streams = Vec::with_capacity(REPEATS);
    for _ in 0..REPEATS {
        let start = clock.now();
        let stream = plain.create_stream_with_hotwords(&terms);
        streams.push(since(&clock, start));
        drop(stream);
    }
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "{scenario} terms={count} recognizer_ms={} stream_ms={}",
        timings(&mut builds),
        timings(&mut streams)
    )?;
    Ok(())
}

/// Whether per-stream hotwords boost like the recogniser's own: the same
/// chunks through plain beam, beam with the list as its hotwords file, and
/// plain beam with the list as per-stream hotwords (separated by `/`, then
/// by newlines).
fn per_stream(
    scenario: &str,
    inputs: &Inputs,
    hotwords: &Hotwords,
    audio: &[Vec<f32>],
) -> Result<()> {
    let plain = create(&beam_config(inputs, hotwords, false))?;
    let boosted = create(&beam_config(inputs, hotwords, true))?;
    let (slashed, _) = per_stream_list(&hotwords.list)?;
    let lined = slashed.replace('/', "\n");
    let mut counts = Agreement::default();
    for chunk in audio {
        let base = decode(&plain, &plain.create_stream(), chunk);
        let file = decode(&boosted, &boosted.create_stream(), chunk);
        let by_slash = decode(&plain, &plain.create_stream_with_hotwords(&slashed), chunk);
        let by_line = decode(&plain, &plain.create_stream_with_hotwords(&lined), chunk);
        counts.add(&base, &file, &by_slash, &by_line);
    }
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "{scenario} chunks={} file_differs_from_plain={} slash_matches_file={} \
         slash_differs_from_plain={} newline_matches_file={} newline_differs_from_plain={}",
        audio.len(),
        counts.file_differs,
        counts.slash_matches,
        counts.slash_differs,
        counts.line_matches,
        counts.line_differs
    )?;
    Ok(())
}

/// Per chunk, which decodes agreed.
#[derive(Debug, Default)]
struct Agreement {
    /// Chunks where the hotwords file changed the text.
    file_differs: usize,
    /// Chunks where `/`-separated per-stream hotwords gave the file's text.
    slash_matches: usize,
    /// Chunks where they changed the plain text.
    slash_differs: usize,
    /// Chunks where newline-separated ones gave the file's text.
    line_matches: usize,
    /// Chunks where they changed the plain text.
    line_differs: usize,
}

impl Agreement {
    /// Counts one chunk's four decodes.
    fn add(&mut self, plain: &str, file: &str, slash: &str, line: &str) {
        self.file_differs += usize::from(file != plain);
        self.slash_matches += usize::from(slash == file);
        self.slash_differs += usize::from(slash != plain);
        self.line_matches += usize::from(line == file);
        self.line_differs += usize::from(line != plain);
    }
}

/// The engine child's model paths and threads.
fn model_paths(inputs: &Inputs) -> ModelPaths {
    ModelPaths {
        parakeet_dir: inputs.parakeet.clone(),
        vad_model: inputs.vad.clone(),
        threads: THREADS,
    }
}

/// Parakeet as [`SherpaModels::load`] configures it, but decoded by
/// modified beam search with a bpe vocab for hotwords; with `hotwords` as
/// its hotwords file if `with_file`.
fn beam_config(inputs: &Inputs, hotwords: &Hotwords, with_file: bool) -> OfflineRecognizerConfig {
    let file = |name: &str| Some(inputs.parakeet.join(name).to_string_lossy().into_owned());
    let mut config = OfflineRecognizerConfig::default();
    config.model_config.transducer.encoder = file("encoder.int8.onnx");
    config.model_config.transducer.decoder = file("decoder.int8.onnx");
    config.model_config.transducer.joiner = file("joiner.int8.onnx");
    config.model_config.tokens = file("tokens.txt");
    config.model_config.model_type = Some("nemo_transducer".into());
    config.model_config.num_threads = i32::from(THREADS);
    config.model_config.provider = Some("cpu".into());
    config.model_config.modeling_unit = Some("bpe".into());
    config.model_config.bpe_vocab = Some(hotwords.vocab.to_string_lossy().into_owned());
    config.decoding_method = Some("modified_beam_search".into());
    config.max_active_paths = BEAM_PATHS;
    // The per-stream graph takes its score from here too.
    config.hotwords_score = BOOST;
    if with_file {
        config.hotwords_file = Some(hotwords.list.to_string_lossy().into_owned());
    }
    config
}

/// Builds a recogniser, or says sherpa-onnx refused it, naming the
/// vocab and the hotwords file it was given.
fn create(config: &OfflineRecognizerConfig) -> Result<OfflineRecognizer> {
    OfflineRecognizer::create(config).ok_or_else(|| {
        format!(
            "sherpa-onnx refused the beam config (vocab {:?}, hotwords file {:?})",
            config.model_config.bpe_vocab, config.hotwords_file
        )
        .into()
    })
}

/// Decodes one chunk on `stream`; its text, trimmed.
fn decode(
    recognizer: &OfflineRecognizer,
    stream: &sherpa_onnx::OfflineStream,
    chunk: &[f32],
) -> String {
    stream.accept_waveform(RATE_HZ, chunk);
    recognizer.decode(stream);
    stream
        .get_result()
        .map(|r| r.text.trim().to_owned())
        .unwrap_or_default()
}

/// A hotword list file as one per-stream string and its term count (see
/// [`join_terms`]).
fn per_stream_list(path: &Path) -> Result<(String, usize)> {
    let list = std::fs::read_to_string(path)
        .map_err(|e| format!("can't read the hotword list {}: {e}", path.display()))?;
    join_terms(&list)
}

/// A hotword list (one term per line) as one per-stream string, terms
/// separated by `/`, and how many terms it holds; blank lines are skipped.
///
/// # Errors
///
/// If the list has no terms, or a term holds `/` or NUL: `/` would split
/// the term, and NUL panics inside sherpa-onnx's crate.
fn join_terms(list: &str) -> Result<(String, usize)> {
    let terms: Vec<&str> = list
        .lines()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if let Some(bad) = terms.iter().find(|t| t.contains(['/', '\0'])) {
        return Err(format!("hotword {bad:?} holds '/' or NUL").into());
    }
    if terms.is_empty() {
        return Err("the hotword list has no terms".into());
    }
    Ok((terms.join("/"), terms.len()))
}

/// The first `count` 25 s chunks of a 16 kHz WAV file.
fn chunks(path: &Path, count: usize) -> Result<Vec<Vec<f32>>> {
    let wave = Wave::read(&path.to_string_lossy())
        .ok_or_else(|| format!("can't read the WAV file {}", path.display()))?;
    if wave.sample_rate() != RATE_HZ {
        return Err(format!("the WAV file is {} Hz, not 16 kHz", wave.sample_rate()).into());
    }
    let chunks: Vec<Vec<f32>> = wave
        .samples()
        .chunks(CHUNK)
        .take(count)
        .map(<[f32]>::to_vec)
        .collect();
    if chunks.len() < count {
        return Err(format!("the WAV file holds only {} chunks", chunks.len()).into());
    }
    Ok(chunks)
}

/// This process's memory, in KiB, from `/proc/self/status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Memory {
    /// Resident now (`VmRSS`).
    rss: u64,
    /// The resident peak so far (`VmHWM`).
    peak: u64,
    /// Swapped out now (`VmSwap`).
    swap: u64,
}

/// Reads this process's [`Memory`].
fn memory() -> Result<Memory> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    parse_memory(&status).ok_or_else(|| "no VmRSS, VmHWM or VmSwap in /proc/self/status".into())
}

/// The [`Memory`] in a `/proc/<pid>/status` text, if it has all three.
fn parse_memory(status: &str) -> Option<Memory> {
    let field = |key: &str| {
        status.lines().find_map(|line| {
            let rest = line.strip_prefix(key)?.strip_prefix(':')?;
            rest.trim().strip_suffix("kB")?.trim().parse().ok()
        })
    };
    Some(Memory {
        rss: field("VmRSS")?,
        peak: field("VmHWM")?,
        swap: field("VmSwap")?,
    })
}

/// Prints one memory reading, in MiB.
fn report(scenario: &str, step: &str, memory: &Memory) -> Result<()> {
    writeln!(
        io::stdout().lock(),
        "{scenario} step={step} rss_mib={} peak_mib={} swap_mib={}",
        memory.rss / 1024,
        memory.peak / 1024,
        memory.swap / 1024
    )?;
    Ok(())
}

/// The time since `start` on `clock`; zero if the clock went backwards,
/// which the session clock never does.
fn since(clock: &SystemClock, start: nota_core::SessionTime) -> Duration {
    clock
        .now()
        .checked_duration_since(start)
        .unwrap_or_default()
}

/// Timings as `first/median/max` in ms (the upper median for an even
/// count). Sorts them.
fn timings(times: &mut [Duration]) -> String {
    let Some(&first) = times.first() else {
        return "none".to_owned();
    };
    times.sort_unstable();
    let ms = |d: Duration| d.as_millis();
    format!(
        "{}/{}/{}",
        ms(first),
        ms(times[times.len() / 2]),
        ms(times[times.len() - 1])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The memory figures come from the three status lines, in KiB.
    #[test]
    fn memory_is_read_from_the_status_lines() {
        let status = "Name:\tengine\nVmHWM:\t  2048 kB\nVmRSS:\t  1024 kB\nVmSwap:\t     0 kB\n";
        assert_eq!(
            parse_memory(status),
            Some(Memory {
                rss: 1024,
                peak: 2048,
                swap: 0
            })
        );
    }

    /// A status without swap (a kernel without it) reads as nothing, not
    /// as zero swap.
    #[test]
    fn a_status_missing_a_line_gives_no_memory() {
        assert_eq!(parse_memory("VmHWM:\t2048 kB\nVmRSS:\t1024 kB\n"), None);
    }

    /// Timings report the first run, then the median and the maximum.
    #[test]
    fn timings_report_first_median_and_max() {
        let mut times = [300, 100, 200, 500, 400].map(Duration::from_millis);
        assert_eq!(timings(&mut times), "300/300/500");
    }

    /// Terms are joined by `/`, without the blank lines, and counted.
    #[test]
    fn terms_are_joined_by_slashes_and_counted() {
        assert_eq!(
            join_terms("aspirin\n\n  ramipril \n").ok(),
            Some(("aspirin/ramipril".to_owned(), 2))
        );
    }

    /// A term with `/` would be split in two by the per-stream format, and
    /// one with NUL would panic in sherpa-onnx's crate; the error names it.
    #[test]
    fn a_term_holding_a_slash_or_nul_is_refused_by_name() {
        let error = |list| join_terms(list).err().map(|e| e.to_string());
        assert_eq!(
            error("aspirin\nco-trimoxazole/x\n"),
            Some(r#"hotword "co-trimoxazole/x" holds '/' or NUL"#.to_owned())
        );
        assert_eq!(
            error("aspirin\nram\0ipril\n"),
            Some(r#"hotword "ram\0ipril" holds '/' or NUL"#.to_owned())
        );
    }

    /// An empty list would time an empty graph and report it as a term.
    #[test]
    fn an_empty_list_is_refused() {
        assert_eq!(
            join_terms("\n  \n").err().map(|e| e.to_string()),
            Some("the hotword list has no terms".to_owned())
        );
    }
}
