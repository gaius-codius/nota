//! nota's model adapters: the [`Decider`](nota_core::decide::Decider) on a
//! local model, the stock `llama-server` from llama.cpp, run by nota when a
//! job needs it and stopped after ([`local::LocalDecider`]); on a cloud
//! model through the user's own `claude` or `codex` CLI ([`cli::CliDecider`]);
//! and on any OpenAI-compatible server nota doesn't run
//! ([`endpoint::EndpointDecider`]). Each says where it runs, for the ☁
//! marker.
//!
//! It isn't the engine crate's: it runs no model in nota's process and has
//! no FFI. llama-server is a process of its own, which nota talks to over a
//! private UNIX socket in plain HTTP ([`http`]), with the bench's prompt
//! ([`prompt`], [`request`]); its replies are read into typed decisions at
//! the boundary ([`reply`]). The deciders are Unix only, for now: the
//! socket, the process groups and the exec are. The request, the reply and
//! the prompt build everywhere.

#[cfg(unix)]
pub mod cli;
#[cfg(unix)]
pub mod endpoint;
pub mod http;
pub mod lifeline;
#[cfg(unix)]
pub mod local;
pub mod prompt;
pub mod reply;
pub mod request;
#[cfg(unix)]
pub mod server;
