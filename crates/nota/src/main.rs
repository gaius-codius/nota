//! The `nota` executable: see the library's docs for its commands.

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    nota::main(&args)
}
