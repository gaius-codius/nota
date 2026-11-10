//! nota's model adapters. For now, the [`Decider`](nota_core::decide::Decider)
//! on a local model: the stock `llama-server` from llama.cpp, run by nota
//! when a job needs it and stopped after ([`local::LocalDecider`]).
//!
//! It isn't the engine crate's: it runs no model in nota's process and has
//! no FFI. llama-server is a process of its own, which nota talks to over a
//! private UNIX socket in plain HTTP ([`http`]), with the bench's prompt
//! ([`prompt`], [`request`]); its replies are read into typed decisions at
//! the boundary ([`reply`]). The server and the decider are Unix only, for
//! now: the socket, the process group and the exec are. The request, the
//! reply and the prompt build everywhere.

pub mod http;
pub mod lifeline;
#[cfg(unix)]
pub mod local;
pub mod prompt;
pub mod reply;
pub mod request;
#[cfg(unix)]
pub mod server;
