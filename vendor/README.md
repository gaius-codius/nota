# Vendored crates

Copies of published crates, each with a small change nota needs that its
published version lacks. `[patch.crates-io]` in the root `Cargo.toml`
points the build at them. They aren't workspace members, so nota's lints
and tests don't run on them.

## sherpa-onnx 1.13.8

The crates.io release of `sherpa-onnx` 1.13.8 (Apache-2.0, `LICENSE`
beside it), without the registry's `.cargo-ok`, `.cargo_vcs_info.json`
and `Cargo.lock`. One change, in `src/offline_asr.rs`: a field on
`OfflineRecognizerResult`,

```rust
#[serde(default)]
pub ys_log_probs: Option<Vec<f32>>,
```

each token's log-probability, which the C library's result JSON already
holds and the published crate drops. The version stays 1.13.8, matching
the pinned native libraries (`scripts/sherpa-onnx.sh`). To check the copy
against the release:

```sh
mkdir -p ~/.cache/sherpa-onnx-crate && cd ~/.cache/sherpa-onnx-crate
curl -sL https://static.crates.io/crates/sherpa-onnx/sherpa-onnx-1.13.8.crate | tar -xz
cd - && diff -r ~/.cache/sherpa-onnx-crate/sherpa-onnx-1.13.8/src vendor/sherpa-onnx/src
```

Remove this copy and the patch once a release of the crate has the field.
