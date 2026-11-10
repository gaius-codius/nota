//! Puts the research bench's yes/no questions to a real llama-server
//! through the local decider, and reports its answers against the bench's.
//!
//! ```sh
//! cargo run -p nota-llm --example bench_decider -- \
//!     --server ~/.local/opt/llama.cpp/bin/llama-server \
//!     --model ~/models/Qwen3.5-4B-Q4_K_M.gguf --device Vulkan0 \
//!     --questions questions.json --subject "an Australian pharmacy lecture"
//! ```
//!
//! The questions file is a JSON list, one entry per chunk the bench asked
//! about: `previous` (or null), `current`, `candidates` (each `heard`,
//! `term` and `fact`, or null) and `bench` (the bench's answer per
//! candidate: true, false, or null where its reply failed). It holds
//! transcript text, so it's made locally from the bench and never
//! committed. The report gives counts and candidate numbers only.
//!
//! The server runs on the device named (`cpu` for none), with its socket
//! in `$XDG_RUNTIME_DIR/nota-bench`, and is stopped at the end.
//! `--server-flags "-c 8192 -fa on"` adds flags after the decider's own
//! (through `sh`, as a launcher), to compare with the bench's server.
//!
//! Unix only, as the decider is.

#[cfg(unix)]
mod unix;

#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    unix::main()
}

#[cfg(not(unix))]
fn main() {}
