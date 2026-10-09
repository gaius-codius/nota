//! A session's row, kept in its directory (`session.txt`) so it can be
//! adopted with its title and tracks if the library database never took
//! it.
//!
//! It's written once, durably, when the session's tracks have started, and
//! never changed. The format is lines of text, the first naming the format:
//!
//! ```text
//! nota session 1
//! title Week 3: cell signalling
//! language en
//! started_at 1760004000
//! track 0 microphone Built-in microphone
//! track 1 system
//! ```
//!
//! A line that's missing is a value not known, and a track line without a
//! source is a track with none. In values, `\` is written `\\`, a line
//! break `\n` and a carriage return `\r`. Lines with a name this version
//! doesn't know are skipped, so a later version can add some.

use std::io;
use std::path::Path;

use nota_core::{SessionId, TrackId, WallTime};
use nota_recorder::fs::{Fs, FsFile as _, StdFs};
use nota_store::{NewSession, Track, TrackKind};

/// The file's name in the session's directory, and the name it's written
/// under first.
pub(super) const KEPT: &str = "session.txt";
pub(super) const KEPT_PARTIAL: &str = "session.txt.partial";

/// The first line.
const HEADER: &str = "nota session 1";

/// Writes `session`'s row into `dir`, its directory: written under another
/// name, synced, renamed into place and the directory synced, so a crash
/// leaves the whole file or none.
pub(crate) fn write(dir: &Path, session: &NewSession) -> io::Result<()> {
    let partial = dir.join(KEPT_PARTIAL);
    let mut file = StdFs.create(&partial)?;
    file.write_all(encode(session).as_bytes())?;
    file.sync()?;
    drop(file);
    StdFs.rename(&partial, &dir.join(KEPT))?;
    StdFs.sync_dir(dir)
}

/// The row kept in `dir`, the directory of session `id`. `Ok(None)` if
/// there's none, as for a session from before nota kept one.
///
/// # Errors
///
/// If the file can't be read, or doesn't parse.
pub(crate) fn read(dir: &Path, id: SessionId) -> Result<Option<NewSession>, String> {
    match StdFs.read(&dir.join(KEPT)) {
        Ok(bytes) => decode(id, &bytes).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// `session` as the file holds it. Its number isn't written: the
/// directory's name is the number.
fn encode(session: &NewSession) -> String {
    let mut lines = vec![HEADER.to_owned()];
    if let Some(title) = &session.title {
        lines.push(format!("title {}", escape(title)));
    }
    if let Some(language) = &session.language {
        lines.push(format!("language {}", escape(language)));
    }
    if let Some(at) = session.started_at {
        lines.push(format!("started_at {}", at.unix_seconds()));
    }
    for track in &session.tracks {
        let kind = match track.kind {
            TrackKind::Microphone => "microphone",
            TrackKind::System => "system",
        };
        let number = track.track.get();
        lines.push(match &track.source {
            Some(source) => format!("track {number} {kind} {}", escape(source)),
            None => format!("track {number} {kind}"),
        });
    }
    lines.push(String::new());
    lines.join("\n")
}

/// Session `id`'s row from the file's `bytes`.
fn decode(id: SessionId, bytes: &[u8]) -> Result<NewSession, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not text: {e}"))?;
    let mut lines = text.split_terminator('\n');
    match lines.next() {
        Some(HEADER) => {}
        Some(other) => return Err(format!("unknown format {other:?}")),
        None => return Err("empty".to_owned()),
    }
    let mut session = NewSession::bare(id);
    for line in lines {
        let (name, value) = line
            .split_once(' ')
            .ok_or_else(|| format!("a line without a value: {line:?}"))?;
        match name {
            "title" => set_once(&mut session.title, unescape(value)?, name)?,
            "language" => set_once(&mut session.language, unescape(value)?, name)?,
            "started_at" => {
                let at = value
                    .parse::<i64>()
                    .ok()
                    .and_then(WallTime::from_unix_seconds)
                    .ok_or_else(|| format!("not a start time: {value:?}"))?;
                set_once(&mut session.started_at, at, name)?;
            }
            "track" => {
                let track = track(value)?;
                if session.tracks.iter().any(|t| t.track == track.track) {
                    return Err(format!("track {} twice", track.track.get()));
                }
                session.tracks.push(track);
            }
            _ => {}
        }
    }
    Ok(session)
}

/// A track line's value: `<number> <kind>`, then ` <source>` if it has one.
fn track(value: &str) -> Result<Track, String> {
    let mut parts = value.splitn(3, ' ');
    let number = parts.next().unwrap_or_default();
    let track = number
        .parse::<u32>()
        .ok()
        .filter(|n| n.to_string() == number)
        .map(TrackId::new)
        .ok_or_else(|| format!("not a track number: {number:?}"))?;
    let kind = match parts.next() {
        Some("microphone") => TrackKind::Microphone,
        Some("system") => TrackKind::System,
        other => return Err(format!("not a track kind: {other:?}")),
    };
    let source = parts.next().map(unescape).transpose()?;
    Ok(Track {
        track,
        kind,
        source,
    })
}

fn set_once<T>(slot: &mut Option<T>, value: T, name: &str) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("{name} twice"));
    }
    *slot = Some(value);
    Ok(())
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(value: &str) -> Result<String, String> {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\r' {
            return Err("a carriage return not escaped".to_owned());
        }
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            other => return Err(format!("an unknown escape: \\{}", other.unwrap_or(' '))),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn any_track() -> impl Strategy<Value = Track> {
        (
            any::<u32>(),
            prop_oneof![Just(TrackKind::Microphone), Just(TrackKind::System)],
            proptest::option::of(any::<String>()),
        )
            .prop_map(|(track, kind, source)| Track {
                track: TrackId::new(track),
                kind,
                source,
            })
    }

    fn any_session() -> impl Strategy<Value = NewSession> {
        (
            any::<u64>(),
            proptest::option::of(any::<String>()),
            proptest::option::of(any::<String>()),
            proptest::option::of(0..=i64::MAX),
            proptest::collection::vec(any_track(), 0..4),
        )
            .prop_map(|(id, title, language, at, tracks)| {
                let mut tracks = tracks;
                let mut seen = std::collections::BTreeSet::new();
                tracks.retain(|t| seen.insert(t.track));
                NewSession {
                    id: SessionId::new(id),
                    title,
                    language,
                    started_at: at.and_then(WallTime::from_unix_seconds),
                    tracks,
                }
            })
    }

    proptest! {
        #[test]
        fn what_is_written_reads_back_the_same(session in any_session()) {
            let text = encode(&session);
            prop_assert_eq!(decode(session.id, text.as_bytes()), Ok(session));
        }

        #[test]
        fn any_bytes_parse_or_are_refused_without_a_panic(
            bytes in proptest::collection::vec(any::<u8>(), 0..256),
        ) {
            let _ = decode(SessionId::new(1), &bytes);
            let mut with_header = format!("{HEADER}\n").into_bytes();
            with_header.extend(&bytes);
            let _ = decode(SessionId::new(1), &with_header);
        }
    }

    #[test]
    fn the_format_reads_as_documented() {
        let text = "nota session 1\n\
                    title Week 3: cell signalling\\nPart 2\n\
                    language en\n\
                    started_at 1760004000\n\
                    track 0 microphone Built-in microphone\n\
                    track 1 system\n\
                    later something a later version adds\n";
        let session = decode(SessionId::new(4), text.as_bytes()).unwrap();
        assert_eq!(
            session,
            NewSession {
                id: SessionId::new(4),
                title: Some("Week 3: cell signalling\nPart 2".to_owned()),
                language: Some("en".to_owned()),
                started_at: WallTime::from_unix_seconds(1_760_004_000),
                tracks: vec![
                    Track {
                        track: TrackId::new(0),
                        kind: TrackKind::Microphone,
                        source: Some("Built-in microphone".to_owned()),
                    },
                    Track {
                        track: TrackId::new(1),
                        kind: TrackKind::System,
                        source: None,
                    },
                ],
            }
        );
        // A bare session is the header alone.
        assert_eq!(
            encode(&NewSession::bare(SessionId::new(4))),
            "nota session 1\n"
        );
    }

    #[test]
    fn a_file_that_doesnt_parse_is_refused() {
        for bad in [
            "",
            "nota session 2\n",
            "nota session 1\ntitle\n",
            "nota session 1\ntitle a\ntitle b\n",
            "nota session 1\nstarted_at -1\n",
            "nota session 1\nstarted_at soon\n",
            "nota session 1\ntrack 07 microphone\n",
            "nota session 1\ntrack 0 speaker\n",
            "nota session 1\ntrack 0 system\ntrack 0 microphone\n",
            "nota session 1\ntitle a\\x\n",
            "nota session 1\ntitle a\\\n",
            "nota session 1\ntitle a\rb\n",
        ] {
            assert!(
                decode(SessionId::new(1), bad.as_bytes()).is_err(),
                "{bad:?} was read"
            );
        }
        assert!(decode(SessionId::new(1), &[0xFF, 0xFE]).is_err());
    }
}
