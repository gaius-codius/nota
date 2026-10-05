//! nota's speech engine.
//!
//! Runs as a child process (`nota engine asr`), so a crash in native model
//! code can't take the recording down. The recorder sends audio over stdin
//! and reads text back over stdout, framed by [`nota_core::protocol`]; see
//! [`child::run`]. The child exits when its stdin closes, and on Linux the
//! kernel also kills it when the recorder dies, even mid-load or mid-decode
//! (see [`nota_core::lifeline`]).
//!
//! This is the only crate with native model code (cargo-deny's `wrappers`
//! rule keeps it so). The `sherpa-onnx` crate's build script links a static
//! sherpa-onnx archive for its exact version:
//!
//! | Library | Licence |
//! |---|---|
//! | sherpa-onnx, kaldi-native-fbank, kaldi-decoder, `OpenFst`, sentencepiece | Apache-2.0 |
//! | onnxruntime | MIT. It bundles others, all permissive or MPL-2.0 (onnxruntime's `ThirdPartyNotices.txt`): abseil and flatbuffers (Apache-2.0), protobuf and re2 (BSD-3-Clause), Eigen (MPL-2.0) |
//! | kissfft | BSD-3-Clause |
//!
//! Every build links an archive pinned by SHA-256, which
//! `scripts/sherpa-onnx.sh` checks and unpacks (`.cargo/config.toml` points
//! `SHERPA_ONNX_LIB_DIR` at it), never the sys crate's unchecked download.
//! On Linux x86-64 that is upstream's archive built without text-to-speech.
//! On macOS and Windows it is still the default archive, which adds
//! piper-phonemize (MIT) and espeak-ng and ucd-tools (GPL-3.0-or-later):
//! nota doesn't use text-to-speech, but the archive's C API references it,
//! so the linker keeps them, and nothing may be distributed from those
//! builds. A build for distribution uses `scripts/sherpa-onnx.sh --no-tts`
//! and passes `scripts/check-no-gpl.sh`; CI does both.
//! cargo-deny doesn't see native archives.

pub mod child;
pub mod chunker;
pub mod sherpa;

use std::fmt;
use std::io;

use nota_core::lifeline::{self, Tie};
use nota_core::messages::ProtocolVersion;
use nota_core::protocol::ReadError;

/// Why the engine stopped.
#[derive(Debug)]
pub enum EngineError {
    /// The recorder's frames couldn't be read.
    Read(ReadError),
    /// Writing to the recorder failed (it has probably gone).
    Write(io::Error),
    /// The recorder speaks another protocol version.
    Version(ProtocolVersion),
    /// The recorder broke the protocol.
    Protocol(&'static str),
    /// A model couldn't be loaded.
    Model(String),
    /// The engine couldn't be tied to the recorder's life: the recorder's
    /// process id it was given isn't one.
    Tie(io::Error),
}

impl From<ReadError> for EngineError {
    fn from(err: ReadError) -> Self {
        Self::Read(err)
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(err) => write!(f, "reading from the recorder: {err}"),
            Self::Write(err) => write!(f, "writing to the recorder: {err}"),
            Self::Version(version) => write!(
                f,
                "the recorder speaks protocol v{}, this engine v{}",
                version.get(),
                ProtocolVersion::CURRENT.get()
            ),
            Self::Protocol(what) => write!(f, "protocol error: {what}"),
            Self::Model(what) => write!(f, "{what}"),
            Self::Tie(err) => write!(f, "tying the engine to the recorder: {err}"),
        }
    }
}

impl std::error::Error for EngineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(err) => Some(err),
            Self::Write(err) | Self::Tie(err) => Some(err),
            Self::Version(_) | Self::Protocol(_) | Self::Model(_) => None,
        }
    }
}

/// Runs `nota engine asr`: the protocol on stdin and stdout with the real
/// models, until stdin closes or the recorder dies.
///
/// It first ties itself to the recorder ([`lifeline::tie_to_recorder`]),
/// before loading anything; if the recorder has already gone, it stops
/// there, as if stdin had closed.
///
/// Native code may print to stdout, which would corrupt the protocol, so on
/// Unix the protocol gets its own copy of stdout and stdout itself is
/// pointed at stderr first.
///
/// # Errors
///
/// As [`child::run`], or if the recorder's process id it was given isn't
/// one, or stdout can't be set aside.
pub fn run_asr(paths: &sherpa::ModelPaths) -> Result<(), EngineError> {
    if lifeline::tie_to_recorder().map_err(EngineError::Tie)? == Tie::Orphaned {
        return Ok(());
    }
    let output = protocol_output().map_err(EngineError::Write)?;
    let input = io::BufReader::new(io::stdin().lock());
    child::run(input, output, || sherpa::SherpaModels::load(paths))
}

#[cfg(unix)]
fn protocol_output() -> io::Result<std::fs::File> {
    let protocol = rustix::io::dup(io::stdout())?;
    rustix::stdio::dup2_stdout(io::stderr())?;
    Ok(std::fs::File::from(protocol))
}

#[cfg(not(unix))]
fn protocol_output() -> io::Result<io::Stdout> {
    Ok(io::stdout())
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    #[test]
    fn errors_say_what_went_wrong() {
        let version = EngineError::Version(ProtocolVersion::new(3));
        assert_eq!(
            version.to_string(),
            "the recorder speaks protocol v3, this engine v0"
        );
        assert_eq!(
            EngineError::Protocol("second hello").to_string(),
            "protocol error: second hello"
        );
        assert_eq!(
            EngineError::Model("no model".into()).to_string(),
            "no model"
        );
        let read = EngineError::from(ReadError::Truncated);
        assert!(read.to_string().starts_with("reading from the recorder"));
        assert!(read.source().is_some());
        let write = EngineError::Write(io::Error::from(io::ErrorKind::BrokenPipe));
        assert!(write.to_string().starts_with("writing to the recorder"));
        assert!(write.source().is_some());
        let tie = EngineError::Tie(io::Error::from(io::ErrorKind::InvalidInput));
        assert!(
            tie.to_string()
                .starts_with("tying the engine to the recorder")
        );
        assert!(tie.source().is_some());
        assert!(version.source().is_none());
    }
}
