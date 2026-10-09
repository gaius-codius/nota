use std::path::{Path, PathBuf};
use std::time::Duration;

use nota_core::{EpochId, SampleIndex, SampleRange, TrackId};
use nota_store::{NewSession, SegmentRow, Sha256Digest, Track, TrackKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::*;

/// A fresh directory under the system temp dir, removed when dropped.
struct TestDir(PathBuf);

impl TestDir {
    #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nota-app-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TestDir {
    #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 09:00 UTC on 2026-10-0`n`, as UTC seconds.
fn day(n: u64) -> i64 {
    // 2026-10-01 09:00 UTC.
    1_790_845_200 + i64::try_from(n - 1).unwrap() * 86_400
}

/// Dates in UTC, with now at 2026-10-03 09:00 (19:00 in Sydney).
fn dates() -> Dates {
    Dates {
        now: WallTime::from_unix_seconds(day(3)),
        zone: TimeZone::UTC,
    }
}

/// A library with sessions 1 to 3 in the database, each a minute of
/// audio per minute of its number, and their tracks.
fn library_of_three(dir: &Path) -> Library {
    let library = Library::open(dir).unwrap();
    for (title, mic) in [
        ("Joinery", "mic"),
        ("Turning", "usb-mic"),
        ("Finishing", "mic"),
    ] {
        let paths = library.create().unwrap();
        let id = paths.id;
        let samples = id.get() * 60 * u64::from(RATE.hz());
        let row = SegmentRow::new(
            TrackId::new(0),
            EpochId::new(0),
            SampleRange::new(SampleIndex::new(0), SampleIndex::new(samples)).unwrap(),
            Sha256Digest::new([1; 32]),
        )
        .unwrap();
        library
            .db()
            .with(|db| {
                db.create_session(&NewSession {
                    id,
                    title: Some(title.into()),
                    language: None,
                    started_at: WallTime::from_unix_seconds(day(id.get())),
                    tracks: vec![
                        Track {
                            track: TrackId::new(0),
                            kind: TrackKind::Microphone,
                            source: Some(mic.into()),
                        },
                        Track {
                            track: TrackId::new(1),
                            kind: TrackKind::System,
                            source: Some("system audio".into()),
                        },
                    ],
                })?;
                db.insert_segment(id, &row)?;
                Ok(())
            })
            .unwrap();
    }
    library
}

/// Leaves a journal in the session's audio directory.
#[expect(clippy::disallowed_methods, reason = "test scaffolding")]
fn leave_journal(dir: &Path, id: u64) {
    let name = nota_recorder::journal::JournalId::new(1).file_name();
    let audio = dir.join("sessions").join(id.to_string()).join("audio");
    std::fs::write(audio.join(name), b"").unwrap();
}

fn row(terminal: &Terminal<TestBackend>, y: u16) -> String {
    let buf = terminal.backend().buffer();
    (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
}

/// Home lists the library database's sessions newest first, with their
/// length, and one that needs the user is pinned at the top with the line
/// that says why.
#[test]
fn home_lists_the_library_newest_first_and_says_what_needs_you() {
    let tmp = TestDir::new("home");
    let library = library_of_three(&tmp.0);
    leave_journal(&tmp.0, 1);
    let sessions = library
        .listing(RATE)
        .unwrap()
        .into_iter()
        .map(|listed| session(listed, &[], &dates()))
        .collect();
    let mut home = Home::new(sessions, "parakeet", Theme::no_color());
    let ids: Vec<u64> = home.sessions().iter().map(|s| s.id).collect();
    assert_eq!(ids, [1, 3, 2]);

    let mut terminal = Terminal::new(TestBackend::new(62, 20)).unwrap();
    terminal.draw(|frame| home.draw(frame)).unwrap();
    assert!(row(&terminal, 0).ends_with(" ! 1 needs you ─╮"));
    let lines: Vec<String> = (12..16).map(|y| row(&terminal, y)).collect();
    assert_eq!(
        lines,
        [
            "│ ▸ ! Joinery                                     1m  1 Oct  │",
            "│     audio still to save · nota tries again when it starts  │",
            "│   ✓ Finishing                                   3m  today  │",
            "│   ✓ Turning                                     2m  2 Oct  │",
        ]
    );
}

/// `R` records with the last session's title and sources: a named device
/// stays named, the default stays the default.
#[test]
fn the_last_settings_come_from_the_last_session() {
    let tmp = TestDir::new("last");
    let library = Library::open(&tmp.0).unwrap();
    assert_eq!(last_setup(&library), None);
    let library = library_of_three(&tmp.0);
    // The newest is session 3: the default mic and system audio.
    assert_eq!(
        last_setup(&library),
        Some(Setup {
            title: "Finishing".into(),
            mic: Input::Default,
            system: Input::Default,
        })
    );
    // A session with a named mic, and no system audio track.
    let paths = library.create().unwrap();
    library
        .db()
        .with(|db| {
            db.create_session(&NewSession {
                id: paths.id,
                title: None,
                language: None,
                started_at: None,
                tracks: vec![Track {
                    track: TrackId::new(0),
                    kind: TrackKind::Microphone,
                    source: Some("usb-mic".into()),
                }],
            })
        })
        .unwrap();
    assert_eq!(
        last_setup(&library),
        Some(Setup {
            title: "Recording".into(),
            mic: Input::Device("usb-mic".into()),
            system: Input::Default,
        })
    );
}

#[test]
fn each_session_shows_what_salvage_found() {
    let listed = |needs| Listed {
        id: SessionId::new(4),
        title: None,
        started_at: None,
        recorded: Some(Duration::from_secs(60)),
        needs,
    };
    let shown = |needs, salvaged: &[Salvaged]| {
        let s = session(listed(needs), salvaged, &dates());
        (s.status, s.detail)
    };
    let id = SessionId::new(4);
    let other = SessionId::new(5);
    assert_eq!(shown(Needs::Nothing, &[]), (Status::Ready, None));
    assert_eq!(
        shown(Needs::Nothing, &[Salvaged::Done(id)]),
        (
            Status::Ready,
            Some("recovered after a crash · nothing lost".into())
        )
    );
    // Another session's salvage says nothing about this one.
    assert_eq!(
        shown(Needs::Nothing, &[Salvaged::Done(other)]),
        (Status::Ready, None)
    );
    assert_eq!(
        shown(Needs::Nothing, &[Salvaged::Failed(id, "disk".into())]),
        (Status::NeedsYou, Some("salvage failed: disk".into()))
    );
    assert_eq!(
        shown(Needs::Attention("why".into()), &[Salvaged::Done(id)]),
        (Status::NeedsYou, Some("why".into()))
    );
    assert_eq!(
        shown(Needs::InUse, &[]),
        (Status::Processing, Some("being recorded".into()))
    );
    let untitled = session(listed(Needs::Nothing), &[], &dates());
    assert_eq!(untitled.title, "session 4");
    assert_eq!(untitled.date, None);
    assert_eq!(untitled.duration, Some(Duration::from_secs(60)));
}

#[test]
fn the_engines_are_named_when_there_are_any() {
    let mut args = RecordArgs {
        data: PathBuf::from("/d"),
        start: Command::Stop,
        models: None,
        tone: false,
        latency_log: None,
    };
    assert_eq!(engines(&args), "no live text");
    args.models = Some((PathBuf::from("p"), PathBuf::from("v")));
    assert_eq!(engines(&args), "parakeet");
}

#[test]
fn dates_read_today_then_day_and_month_then_month_and_year() {
    let dates = dates();
    let at = |s| WallTime::from_unix_seconds(s).unwrap();
    assert_eq!(dates.label(at(day(3))).as_deref(), Some("today"));
    assert_eq!(dates.label(at(day(2))).as_deref(), Some("2 Oct"));
    // 2025-12-31 23:00 UTC: last year.
    assert_eq!(dates.label(at(1_767_222_000)).as_deref(), Some("Dec 2025"));
    // The local time zone decides the day: 23:30 UTC on 2 Oct is 3 Oct
    // in Sydney (UTC+10), so today there.
    let sydney = Dates {
        now: dates.now,
        zone: TimeZone::fixed(jiff::tz::offset(10)),
    };
    let late = at(day(2) + 14 * 3_600 + 1_800);
    assert_eq!(dates.label(late).as_deref(), Some("2 Oct"));
    assert_eq!(sydney.label(late).as_deref(), Some("today"));
    // Without now, nothing is today.
    let unknown = Dates {
        now: None,
        zone: TimeZone::UTC,
    };
    assert_eq!(unknown.label(at(day(3))).as_deref(), Some("3 Oct"));
}
