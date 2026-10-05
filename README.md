# nota

> **Early development.** There is no working version yet. Nothing here is
> ready to install or use.

nota will be a local-first terminal app that records lectures and workshops,
transcribes them, fixes misheard specialist terms as tracked corrections you
can undo, writes summaries that cite their source, and keeps a searchable
library of your sessions. It runs on your own machine by default; cloud
models are opt-in and always marked.

Linux comes first; macOS and Windows are planned.

## Building

nota builds with the Rust toolchain pinned in `rust-toolchain.toml`. On
Linux, the recorder needs the PipeWire and ALSA headers, pkg-config and
libclang (`scripts/install-linux-build-deps.sh` lists them for Debian and
Ubuntu).

The speech engine links sherpa-onnx's static libraries. Fetch them once per
clone, and again after `cargo clean`:

```sh
scripts/sherpa-onnx.sh
cargo build
```

The script downloads the archive for your platform, checks it against the
SHA-256 it pins and unpacks it into `target/sherpa-onnx/lib`, which
`.cargo/config.toml` points the build at. Until it has run, the build stops
with "SHERPA_ONNX_LIB_DIR does not exist".

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT licence ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
licence, shall be dual licensed as above, without any additional terms or
conditions.
