//! Refuses to build the engine unless `SHERPA_ONNX_LIB_DIR` is set.
//!
//! Without it, sherpa-onnx-sys's build script downloads its archive with no
//! checksum. `.cargo/config.toml` sets it to the libraries
//! `scripts/sherpa-onnx.sh` checked, but cargo only reads that file when it
//! runs inside the repo; a `cargo build --manifest-path` from elsewhere
//! would link the unchecked download. This stops that build before anything
//! links it.
//!
//! It can't reliably stop the download itself. Cargo doesn't order this
//! build script after sherpa-onnx-sys's (a transitive dependency), so the
//! sys crate's can run first, or alongside, and fetch and unpack its
//! archive into that target directory; with `--keep-going`, or a target
//! directory where this one already failed, it will. Those bytes are never
//! linked: this build fails, and a build from inside the repo sets the
//! variable, which makes the sys crate's build script run again and use the
//! checked libraries instead. So an unchecked archive can sit in `target/`,
//! unused; `cargo clean` removes it.

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
