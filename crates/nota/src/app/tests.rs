use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use crate::record::last_setup;
use nota_core::recorder::Input;
use nota_core::{EpochId, FakeClock, SampleIndex, SampleRange, TrackId};
use nota_store::{NewSession, SegmentRow, Sha256Digest, Track, TrackKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;

/// A fresh directory under the system temp dir, removed when dropped.
pub(super) struct TestDir(pub(super) PathBuf);

impl TestDir {
    #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
    pub(super) fn new(name: &str) -> Self {
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
pub(super) fn library_of_three(dir: &Path) -> Library {
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
            mic: Some(Input::Default),
            system: Some(Input::Default),
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
            mic: Some(Input::Device("usb-mic".into())),
            system: Some(Input::Default),
        })
    );
}

/// Home distinguishes failed cleanup from audio still waiting to publish.
#[test]
fn home_names_an_undeletable_published_journal() {
    let held = crate::record::cleanup_report();
    assert_eq!(held.not_deleted().len(), 1);
    // The listing sees the remaining journal; salvage supplies its cause.
    let listed = Listed {
        id: SessionId::new(1),
        title: None,
        started_at: None,
        recorded: Some(Duration::from_secs(1)),
        needs: Needs::Attention("audio still to save".into()),
    };
    let shown = session(
        listed,
        &[Salvaged::Left(
            SessionId::new(1),
            Vec::new(),
            Box::new(held),
        )],
        &dates(),
    );
    assert_eq!(shown.status, Status::NeedsYou);
    assert_eq!(
        shown.detail.as_deref(),
        Some("journal-000000: couldn't delete it (permission denied); all its audio is published")
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
        shown(Needs::Nothing, &[Salvaged::Done(id, Vec::new())]),
        (
            Status::Ready,
            Some("recovered after a crash · nothing lost".into())
        )
    );
    // A journal set aside as damaged: recovered, but not "nothing lost".
    let aside = vec![PathBuf::from("journal-000003.unreadable")];
    assert_eq!(
        shown(Needs::Nothing, &[Salvaged::Left(id, aside, Box::default())]),
        (Status::Ready, Some("recovered after a crash".into()))
    );
    // Another session's salvage says nothing about this one.
    assert_eq!(
        shown(Needs::Nothing, &[Salvaged::Done(other, Vec::new())]),
        (Status::Ready, None)
    );
    // Salvage at start failed, and its journals are still there: its
    // error says why.
    assert_eq!(
        shown(
            Needs::Attention("audio still to save".into()),
            &[Salvaged::Failed(id, "disk".into())]
        ),
        (Status::NeedsYou, Some("salvage failed: disk".into()))
    );
    // A later salvage (a recording's start) left nothing to save: the old
    // failure isn't shown.
    assert_eq!(
        shown(Needs::Nothing, &[Salvaged::Failed(id, "disk".into())]),
        (Status::Ready, None)
    );
    assert_eq!(
        shown(
            Needs::Attention("why".into()),
            &[Salvaged::Done(id, Vec::new())]
        ),
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
        fake_engine: false,
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

/// A listing of `library` in UTC, timed by `clock`, with the calendar's
/// time read from `wall` (UTC seconds).
fn listing_of<'a>(
    library: &'a Library,
    clock: &Arc<FakeClock>,
    wall: &Arc<AtomicI64>,
) -> Listing<'a> {
    let wall = Arc::clone(wall);
    Listing {
        library,
        salvaged: &[],
        zone: || TimeZone::UTC,
        clock: Arc::clone(clock) as Arc<dyn Clock>,
        wall: Box::new(move || WallTime::from_unix_seconds(wall.load(Ordering::SeqCst))),
        listed_at: None,
        said: None,
    }
}

fn date_of(home: &Home, id: u64) -> Option<String> {
    let session = home.sessions().iter().find(|s| s.id == id).unwrap();
    session.date.clone()
}

/// A Home left open past midnight words yesterday's session as its date
/// as soon as the date changes, without waiting for the next listing.
#[test]
fn home_open_past_midnight_dates_yesterday() {
    let tmp = TestDir::new("midnight");
    let library = library_of_three(&tmp.0);
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    // 23:59:58 UTC on 3 Oct.
    let wall = Arc::new(AtomicI64::new(day(3) + 15 * 3_600 - 2));
    let mut listing = listing_of(&library, &clock, &wall);
    let mut home = Home::new(listing.sessions().unwrap(), "parakeet", Theme::no_color());
    assert_eq!(date_of(&home, 3).as_deref(), Some("today"));
    // A second later, still the same day: nothing is listed again, so a
    // journal left meanwhile doesn't show yet.
    leave_journal(&tmp.0, 1);
    wall.fetch_add(1, Ordering::SeqCst);
    clock.advance(Duration::from_secs(1));
    listing.refresh(&mut home);
    assert_eq!(date_of(&home, 3).as_deref(), Some("today"));
    assert!(home.sessions().iter().all(|s| s.status == Status::Ready));
    // Past midnight, well within the interval: listed again.
    wall.fetch_add(2, Ordering::SeqCst);
    clock.advance(Duration::from_secs(2));
    listing.refresh(&mut home);
    assert_eq!(date_of(&home, 3).as_deref(), Some("3 Oct"));
    assert_eq!(date_of(&home, 2).as_deref(), Some("2 Oct"));
    assert_eq!(home.sessions()[0].status, Status::NeedsYou);
}

/// A session's `!` clears on Home once its journal is gone, at the next
/// listing, and the selection stays on the session it was on though the
/// order changes.
#[test]
fn home_shows_a_status_change_without_leaving() {
    let tmp = TestDir::new("relist");
    let library = library_of_three(&tmp.0);
    leave_journal(&tmp.0, 1);
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let wall = Arc::new(AtomicI64::new(day(3)));
    let mut listing = listing_of(&library, &clock, &wall);
    let mut home = Home::new(listing.sessions().unwrap(), "parakeet", Theme::no_color());
    let ids = |home: &Home| home.sessions().iter().map(|s| s.id).collect::<Vec<_>>();
    assert_eq!(ids(&home), [1, 3, 2]);
    home.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    home.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(home.selected(), Some(2));

    remove_journal(&tmp.0, 1);
    // Before the interval: not listed again, so still `!`.
    clock.advance(RELIST.checked_sub(Duration::from_millis(1)).unwrap());
    listing.refresh(&mut home);
    assert_eq!(home.sessions()[0].status, Status::NeedsYou);
    clock.advance(Duration::from_millis(1));
    listing.refresh(&mut home);
    assert_eq!(ids(&home), [3, 2, 1]);
    assert!(home.sessions().iter().all(|s| s.status == Status::Ready));
    assert_eq!(home.selected(), Some(2));

    let mut terminal = Terminal::new(TestBackend::new(62, 20)).unwrap();
    terminal.draw(|frame| home.draw(frame)).unwrap();
    assert!(
        row(&terminal, 0).ends_with(" ✓ ready ─╮"),
        "{}",
        row(&terminal, 0)
    );
}

/// If listing fails while Home is open, Home keeps its list and says so
/// once, and the next listing that works replaces it.
#[test]
fn a_failed_listing_keeps_the_list_and_says_so_once() {
    let tmp = TestDir::new("relist-fails");
    let library = library_of_three(&tmp.0);
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let wall = Arc::new(AtomicI64::new(day(3)));
    let mut listing = listing_of(&library, &clock, &wall);
    let mut home = Home::new(listing.sessions().unwrap(), "parakeet", Theme::no_color());
    let sessions = tmp.0.join("sessions");
    let aside = tmp.0.join("sessions-aside");
    rename(&sessions, &aside);
    // Where the sessions were, a file: listing it fails.
    write_file(&sessions);
    let mut terminal = Terminal::new(TestBackend::new(62, 20)).unwrap();
    let notice = |terminal: &mut Terminal<TestBackend>, home: &mut Home| {
        terminal.draw(|frame| home.draw(frame)).unwrap();
        row(terminal, 10)
    };
    clock.advance(RELIST);
    listing.refresh(&mut home);
    assert_eq!(home.sessions().len(), 3);
    assert!(
        notice(&mut terminal, &mut home).contains("the sessions couldn't be listed"),
        "{}",
        notice(&mut terminal, &mut home)
    );
    // Seen, then the next failure says nothing more.
    home.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    clock.advance(RELIST);
    listing.refresh(&mut home);
    assert!(!notice(&mut terminal, &mut home).contains("couldn't be listed"));
    assert_eq!(home.sessions().len(), 3);
    // Back again: listed, with what changed meanwhile.
    remove_file(&sessions);
    rename(&aside, &sessions);
    leave_journal(&tmp.0, 2);
    clock.advance(RELIST);
    listing.refresh(&mut home);
    assert_eq!(home.sessions()[0].id, 2);
    assert_eq!(home.sessions()[0].status, Status::NeedsYou);
    // A failure after that is said again.
    rename(&sessions, &aside);
    write_file(&sessions);
    clock.advance(RELIST);
    listing.refresh(&mut home);
    assert!(notice(&mut terminal, &mut home).contains("couldn't be listed"));
}

/// A listing that fails just after midnight waits the interval before
/// trying again, like any other failure, rather than at every draw.
#[test]
fn a_failed_listing_after_midnight_waits_to_try_again() {
    let tmp = TestDir::new("midnight-fails");
    let library = library_of_three(&tmp.0);
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    // 23:59:59 UTC on 3 Oct.
    let wall = Arc::new(AtomicI64::new(day(3) + 15 * 3_600 - 1));
    let mut listing = listing_of(&library, &clock, &wall);
    let mut home = Home::new(listing.sessions().unwrap(), "parakeet", Theme::no_color());
    let sessions = tmp.0.join("sessions");
    let aside = tmp.0.join("sessions-aside");
    rename(&sessions, &aside);
    write_file(&sessions);
    wall.fetch_add(2, Ordering::SeqCst);
    listing.refresh(&mut home);
    assert_eq!(date_of(&home, 3).as_deref(), Some("today"));
    // Listing works again, but the interval since the failure hasn't
    // passed: nothing is listed.
    remove_file(&sessions);
    rename(&aside, &sessions);
    clock.advance(RELIST.checked_sub(Duration::from_millis(1)).unwrap());
    listing.refresh(&mut home);
    assert_eq!(date_of(&home, 3).as_deref(), Some("today"));
    clock.advance(Duration::from_millis(1));
    listing.refresh(&mut home);
    assert_eq!(date_of(&home, 3).as_deref(), Some("3 Oct"));
}

/// A failed listing doesn't replace a problem Home already shows, and the
/// notice it gives is taken back once a listing works.
#[test]
fn a_listing_notice_leaves_another_and_goes_once_listing_works() {
    let tmp = TestDir::new("relist-notice");
    let library = library_of_three(&tmp.0);
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let wall = Arc::new(AtomicI64::new(day(3)));
    let mut listing = listing_of(&library, &clock, &wall);
    let mut home = Home::new(listing.sessions().unwrap(), "parakeet", Theme::no_color());
    home.set_notice(Some("the recording failed: no mic".into()));
    let sessions = tmp.0.join("sessions");
    let aside = tmp.0.join("sessions-aside");
    rename(&sessions, &aside);
    write_file(&sessions);
    clock.advance(RELIST);
    listing.refresh(&mut home);
    assert_eq!(home.notice(), Some("the recording failed: no mic"));
    // Once that's dismissed, the next failure says so.
    let mut terminal = Terminal::new(TestBackend::new(62, 20)).unwrap();
    terminal.draw(|frame| home.draw(frame)).unwrap();
    home.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    clock.advance(RELIST);
    listing.refresh(&mut home);
    let failed = home.notice().unwrap();
    assert!(
        failed.starts_with("the sessions couldn't be listed: "),
        "{failed}"
    );
    // Listing works again: the notice goes, unread or not.
    remove_file(&sessions);
    rename(&aside, &sessions);
    clock.advance(RELIST);
    listing.refresh(&mut home);
    assert_eq!(home.notice(), None);
}

/// Removes the journal [`leave_journal`] left.
#[expect(clippy::disallowed_methods, reason = "test scaffolding")]
fn remove_journal(dir: &Path, id: u64) {
    let name = nota_recorder::journal::JournalId::new(1).file_name();
    let audio = dir.join("sessions").join(id.to_string()).join("audio");
    std::fs::remove_file(audio.join(name)).unwrap();
}

#[expect(clippy::disallowed_methods, reason = "test scaffolding")]
fn rename(from: &Path, to: &Path) {
    std::fs::rename(from, to).unwrap();
}

#[expect(clippy::disallowed_methods, reason = "test scaffolding")]
fn write_file(path: &Path) {
    std::fs::write(path, b"").unwrap();
}

#[expect(clippy::disallowed_methods, reason = "test scaffolding")]
fn remove_file(path: &Path) {
    std::fs::remove_file(path).unwrap();
}

/// A stopped session's final pass and the page listing it.
fn stopped_job(library: &Library) -> nota_store::Job {
    let id = SessionId::new(3);
    library
        .db()
        .with(|db| db.finish_recording(id, None))
        .unwrap();
    library
        .db()
        .with(|db| db.session_jobs(id))
        .unwrap()
        .remove(0)
}

/// Session 3 as Home lists it after its job changes.
fn shown_job(library: &Library) -> Session {
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let wall = Arc::new(AtomicI64::new(day(3)));
    listing_of(library, &clock, &wall)
        .sessions()
        .unwrap()
        .into_iter()
        .find(|s| s.id == 3)
        .unwrap()
}

/// A queued final pass shows that the session is still being processed.
#[test]
fn home_shows_waiting_job() {
    let tmp = TestDir::new("job-waiting");
    let library = library_of_three(&tmp.0);
    // Stopping queues the pass without starting it.
    stopped_job(&library);
    assert_eq!(shown_job(&library).status, Status::Processing);
}

/// A running final pass keeps the session marked as processing.
#[test]
fn home_shows_running_job() {
    let tmp = TestDir::new("job-running");
    let library = library_of_three(&tmp.0);
    let job = stopped_job(&library);
    // Starting the queued pass must not make the session ready.
    library.db().with(|db| db.start_job(job.id)).unwrap();
    assert_eq!(shown_job(&library).status, Status::Processing);
}

/// A failed final pass shows its reason under the session.
#[test]
fn home_shows_failed_job_reason() {
    let tmp = TestDir::new("job-failed");
    let library = library_of_three(&tmp.0);
    let job = stopped_job(&library);
    // The stored reason is shown with the failed state.
    library
        .db()
        .with(|db| db.end_job(job.id, &nota_store::JobEnd::Failed("broken".into())))
        .unwrap();
    let shown = shown_job(&library);
    assert_eq!(shown.status, Status::NeedsYou);
    assert_eq!(shown.detail.as_deref(), Some("processing failed: broken"));
}

/// A finished final pass makes the session ready.
#[test]
fn home_shows_done_job() {
    let tmp = TestDir::new("job-done");
    let library = library_of_three(&tmp.0);
    let job = stopped_job(&library);
    // Ending the queued pass clears the processing status.
    library
        .db()
        .with(|db| db.end_job(job.id, &nota_store::JobEnd::Done))
        .unwrap();
    assert_eq!(shown_job(&library).status, Status::Ready);
}

/// Entering Home reads a changed session before the usual interval passes.
#[test]
fn entering_home_lists_before_the_interval() {
    let tmp = TestDir::new("enter-home-relist");
    let library = library_of_three(&tmp.0);
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let wall = Arc::new(AtomicI64::new(day(3)));
    let mut listing = listing_of(&library, &clock, &wall);
    let mut home = Home::new(listing.sessions().unwrap(), "parakeet", Theme::no_color());
    // A remaining journal changes the status without advancing the clock.
    leave_journal(&tmp.0, 1);
    listing.relist(&mut home);
    assert_eq!(home.sessions()[0].id, 1);
    assert_eq!(home.sessions()[0].status, Status::NeedsYou);
}

/// A read before a new recording keeps the listed sessions if it fails.
#[test]
fn a_failed_relist_before_recording_keeps_home() {
    let tmp = TestDir::new("before-recording-relist-fails");
    let library = library_of_three(&tmp.0);
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let wall = Arc::new(AtomicI64::new(day(3)));
    let mut listing = listing_of(&library, &clock, &wall);
    let mut home = Home::new(listing.sessions().unwrap(), "parakeet", Theme::no_color());
    let listed = home.sessions().to_vec();
    // Replacing the directory with a file makes the next read fail.
    let sessions = tmp.0.join("sessions");
    rename(&sessions, &tmp.0.join("sessions-aside"));
    write_file(&sessions);
    listing.relist(&mut home);
    assert_eq!(home.sessions(), listed);
    assert!(home.notice().unwrap().contains("couldn't be listed"));
}

/// A recording's default title is the local date and time, as Setup shows
/// it, and plain `Recording` where the calendar's time can't be read.
#[test]
fn the_default_title_is_the_local_date_and_time() {
    // 2026-10-03 09:00 in UTC, and 19:00 in Sydney.
    assert_eq!(dates().title(), "3 Oct, 09:00");
    let sydney = Dates {
        zone: TimeZone::get("Australia/Sydney").unwrap(),
        ..dates()
    };
    assert_eq!(sydney.title(), "3 Oct, 19:00");
    let unknown = Dates {
        now: None,
        ..dates()
    };
    assert_eq!(unknown.title(), "Recording");
}
