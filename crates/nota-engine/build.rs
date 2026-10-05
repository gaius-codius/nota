//! Refuses to build the engine unless `SHERPA_ONNX_LIB_DIR` is set.
//!
//! Without it, sherpa-onnx-sys's build script downloads its archive with no
//! checksum. `.cargo/config.toml` sets it to the libraries
//! `scripts/sherpa-onnx.sh` checked, but cargo only reads that file when it
//! runs inside the repo; a `cargo build --manifest-path` from elsewhere
//! would link the unchecked download. This stops that build before anything
//! links it.

use std::env;

fn main() -> Result<(), String> {
    println!("cargo::rerun-if-env-changed=SHERPA_ONNX_LIB_DIR");
    if env::var_os("SHERPA_ONNX_LIB_DIR").is_none() {
        return Err(
            "SHERPA_ONNX_LIB_DIR is unset, so sherpa-onnx-sys would link an \
                    unchecked download. Run cargo from inside the repo (its \
                    .cargo/config.toml sets it, after scripts/sherpa-onnx.sh), or set \
                    it to the directory scripts/sherpa-onnx.sh prints."
                .to_owned(),
        );
    }
    Ok(())
}
