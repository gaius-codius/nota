//! Tying llama-server to nota, so it ends when nota does.
//!
//! The speech engine ties itself to nota when it starts
//! ([`nota_core::lifeline`]). llama-server can't, and nota's code may not
//! run anything in a child between its fork and exec (that needs `unsafe`).
//! So nota starts it through itself: `nota engine llama-server PROGRAM
//! ARGS…` runs [`exec_tied`], which ties that process to nota and then
//! becomes llama-server by exec. The kernel's tie lasts through an exec,
//! so llama-server is killed when nota dies, however it dies, even while
//! it's loading a model.

use std::ffi::OsString;
use std::io;

use nota_core::lifeline::{Tie, tie_to_recorder};

/// Ties this process to the nota that started it, then runs `args` (a
/// program and its arguments) in its place. Returns only if that fails.
///
/// Off Linux the tie isn't made, and the program runs untied.
#[must_use]
pub fn exec_tied(args: &[OsString]) -> io::Error {
    let Some((program, args)) = args.split_first() else {
        return io::Error::new(io::ErrorKind::InvalidInput, "no program to run");
    };
    match tie_to_recorder() {
        Ok(Tie::Tied | Tie::Unsupported) => {}
        // nota died before the tie was made: don't start a server for
        // nobody.
        Ok(Tie::Orphaned) => return io::Error::other("nota has already gone"),
        Err(e) => return e,
    }
    std::os::unix::process::CommandExt::exec(std::process::Command::new(program).args(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With nothing to run, it says so rather than exec anything.
    #[test]
    fn nothing_to_run_is_refused() {
        let err = exec_tied(&[]);
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "no program to run");
    }
}
