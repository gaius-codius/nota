//! What Setup chose for the last recording, kept in the data directory
//! (`setup.txt`) so `R` records with it again.
//!
//! The library database holds each track's source as a display name
//! (`mic`, `system audio`, or a pinned device's node name), which can't
//! tell a default from a device that happens to be named `mic`. This file
//! keeps the choice itself, and what to listen to as well. The format is
//! lines of text, the first naming the format:
//!
//! ```text
//! nota setup 1
//! listen both
//! system default
//! microphone device alsa_input.usb-Seiren_Mini
//! ```
//!
//! A line that's missing is the default (the system audio, on the default
//! devices). Lines with a name this version doesn't know are skipped, so a
//! later version can add some. In a device's name, `\` is written `\\`, a
//! line break `\n` and a carriage return `\r`, as in a session's kept row.

use std::io;
use std::path::Path;

use nota_core::recorder::{Input, Setup};
use nota_recorder::fs::{Fs, FsFile as _};
use nota_tui::Listen;

use crate::library::kept::{escape, unescape};

/// The file's name in the data directory, and the name it's written under
/// first.
const FILE: &str = "setup.txt";
const PARTIAL: &str = "setup.txt.partial";

/// The first line.
const HEADER: &str = "nota setup 1";

/// The longest file read: the file is a few lines.
const MAX_LEN: usize = 16 * 1024;

/// What Setup chose: what to listen to, and each source's device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Remembered {
    /// What to listen to.
    pub(super) listen: Listen,
    /// Where the system audio is recorded from.
    pub(super) system: Input,
    /// Where the microphone is recorded from.
    pub(super) microphone: Input,
}

impl Remembered {
    /// Both sources, on the default devices: what a first recording gets.
    pub(super) const fn both() -> Self {
        Self {
            listen: Listen::Both,
            system: Input::Default,
            microphone: Input::Default,
        }
    }

    /// The setup that records `title` as this chose: a source not listened
    /// to has no track.
    pub(super) fn setup(&self, title: String) -> Setup {
        let (mic, system) = match self.listen {
            Listen::System => (false, true),
            Listen::Microphone => (true, false),
            Listen::Both => (true, true),
        };
        Setup {
            title,
            mic: mic.then(|| self.microphone.clone()),
            system: system.then(|| self.system.clone()),
        }
    }
}

/// Writes `remembered` into `dir`, the data directory, on `fs`: written
/// under another name, synced, renamed into place and the directory
/// synced, so a crash leaves the whole file or the one before.
///
/// # Errors
///
/// If any step of the write fails.
pub(super) fn write<F: Fs>(fs: &F, dir: &Path, remembered: &Remembered) -> io::Result<()> {
    let partial = dir.join(PARTIAL);
    // A write that failed may have left this name.
    match fs.remove(&partial) {
        Ok(()) => fs.sync_dir(dir)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut file = fs.create(&partial)?;
    file.write_all(encode(remembered).as_bytes())?;
    file.sync()?;
    drop(file);
    fs.rename(&partial, &dir.join(FILE))?;
    fs.sync_dir(dir)
}

/// What Setup last chose, from `dir` on `fs`. `Ok(None)` if there's no
/// file, or it doesn't parse or is longer than any nota writes: the last
/// recording's tracks stand in for it.
///
/// # Errors
///
/// If the file is there but can't be read, which may pass.
pub(super) fn read<F: Fs>(fs: &F, dir: &Path) -> io::Result<Option<Remembered>> {
    match fs.read(&dir.join(FILE)) {
        Ok(bytes) if bytes.len() > MAX_LEN => Ok(None),
        Ok(bytes) => Ok(decode(&bytes).ok()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// `remembered` as the file holds it.
fn encode(remembered: &Remembered) -> String {
    let listen = match remembered.listen {
        Listen::System => "system",
        Listen::Microphone => "microphone",
        Listen::Both => "both",
    };
    let input = |name: &str, input: &Input| match input {
        Input::Default => format!("{name} default"),
        Input::Device(node) => format!("{name} device {}", escape(node)),
    };
    [
        HEADER.to_owned(),
        format!("listen {listen}"),
        input("system", &remembered.system),
        input("microphone", &remembered.microphone),
        String::new(),
    ]
    .join("\n")
}

/// What the file's `bytes` hold.
fn decode(bytes: &[u8]) -> Result<Remembered, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))?;
    let mut lines = text.split_terminator('\n');
    match lines.next() {
        Some(HEADER) => {}
        Some(other) => return Err(format!("unknown format {other:?}")),
        None => return Err("empty".to_owned()),
    }
    let mut remembered = Remembered {
        listen: Listen::System,
        system: Input::Default,
        microphone: Input::Default,
    };
    let mut seen = Vec::new();
    for line in lines {
        let (name, value) = line.split_once(' ').unwrap_or((line, ""));
        if !["listen", "system", "microphone"].contains(&name) {
            continue;
        }
        if seen.contains(&name) {
            return Err(format!("{name} twice"));
        }
        seen.push(name);
        match name {
            "listen" => remembered.listen = listen(value)?,
            "system" => remembered.system = input(value)?,
            _ => remembered.microphone = input(value)?,
        }
    }
    Ok(remembered)
}

/// What `value` says to listen to.
fn listen(value: &str) -> Result<Listen, String> {
    match value {
        "system" => Ok(Listen::System),
        "microphone" => Ok(Listen::Microphone),
        "both" => Ok(Listen::Both),
        other => Err(format!("not something to listen to: {other:?}")),
    }
}

/// The input `value` says: `default`, or `device` and a node name.
fn input(value: &str) -> Result<Input, String> {
    if value == "default" {
        return Ok(Input::Default);
    }
    match value.strip_prefix("device ") {
        Some(node) if !node.is_empty() => Ok(Input::Device(unescape(node)?)),
        _ => Err(format!("not an input: {value:?}")),
    }
}

#[cfg(test)]
mod tests;
