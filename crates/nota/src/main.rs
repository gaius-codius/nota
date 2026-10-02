//! The `nota` executable. Every role is this one program; for now the only
//! one is the speech engine child, `nota engine asr`, which the recorder
//! starts and talks to over stdin and stdout.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use nota_engine::sherpa::ModelPaths;

const USAGE: &str = "usage: nota engine asr --parakeet DIR --vad FILE [--threads N]";

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let paths = match parse(&args) {
        Ok(paths) => paths,
        Err(err) => return fail(&format!("{err}\n{USAGE}"), 2),
    };
    // The child's stderr is the recorder's to route; it never reaches the
    // TUI's terminal.
    match nota_engine::run_asr(&paths) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => fail(&format!("nota engine asr: {err}"), 1),
    }
}

fn fail(message: &str, code: u8) -> ExitCode {
    // Nothing more can be done if stderr is gone too.
    let _ = writeln!(io::stderr(), "{message}");
    ExitCode::from(code)
}

/// Parses `engine asr` and its options.
fn parse(args: &[OsString]) -> Result<ModelPaths, String> {
    let mut args = args.iter();
    if args.next().and_then(|a| a.to_str()) != Some("engine")
        || args.next().and_then(|a| a.to_str()) != Some("asr")
    {
        return Err("unknown command".into());
    }
    let (mut parakeet, mut vad, mut threads) = (None, None, 4_u16);
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("{} needs a value", flag.display()))?;
        match flag.to_str() {
            Some("--parakeet") => parakeet = Some(PathBuf::from(value)),
            Some("--vad") => vad = Some(PathBuf::from(value)),
            Some("--threads") => {
                threads = value
                    .to_str()
                    .and_then(|v| v.parse().ok())
                    .filter(|&n| n > 0)
                    .ok_or("--threads takes a positive number")?;
            }
            _ => return Err(format!("unknown option {}", flag.display())),
        }
    }
    Ok(ModelPaths {
        parakeet_dir: parakeet.ok_or("--parakeet is required")?,
        vad_model: vad.ok_or("--vad is required")?,
        threads,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_engine_asr() {
        let paths = parse(&args(&[
            "engine",
            "asr",
            "--vad",
            "v.onnx",
            "--parakeet",
            "p",
            "--threads",
            "2",
        ]))
        .unwrap();
        assert_eq!(paths.parakeet_dir, PathBuf::from("p"));
        assert_eq!(paths.vad_model, PathBuf::from("v.onnx"));
        assert_eq!(paths.threads, 2);
        let default = parse(&args(&["engine", "asr", "--vad", "v", "--parakeet", "p"])).unwrap();
        assert_eq!(default.threads, 4);
    }

    #[test]
    fn refuses_bad_arguments() {
        for bad in [
            &[][..],
            &["engine"],
            &["engine", "tts"],
            &["engine", "asr"],
            &["engine", "asr", "--vad", "v"],
            &["engine", "asr", "--vad", "v", "--parakeet"],
            &[
                "engine",
                "asr",
                "--vad",
                "v",
                "--parakeet",
                "p",
                "--threads",
                "0",
            ],
            &[
                "engine",
                "asr",
                "--vad",
                "v",
                "--parakeet",
                "p",
                "--loud",
                "1",
            ],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?}");
        }
    }
}
