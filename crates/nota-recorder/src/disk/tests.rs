use std::sync::mpsc;

use nota_core::SampleCount;

use super::*;
use crate::fs::fake::{CrashOutcome, FakeFs};

fn p(path: &str) -> PathBuf {
    PathBuf::from(path)
}

/// `nota record`'s rate and windows: 16 kHz, five minutes.
fn speech(tracks: usize) -> Usage {
    Usage::new(
        tracks,
        SampleRate::SPEECH,
        SegmentLength::default_at(SampleRate::SPEECH),
    )
}

/// The crash tests' rate and windows: 1 kHz, a second and a half.
fn small(tracks: usize) -> Usage {
    Usage::new(
        tracks,
        SampleRate::new(1_000).unwrap(),
        SegmentLength::new(SampleCount::new(1_500)).unwrap(),
    )
}

const MB: u64 = 1_000_000;

#[test]
fn a_track_at_16_khz_takes_about_60_mb_of_flac_an_hour() {
    let one = speech(1);
    let hour = one.bytes_per_second() * 3_600;
    assert!((60 * MB..60 * MB + 3_600).contains(&hour), "{hour}");
    assert_eq!(speech(2).bytes_per_second(), 2 * one.bytes_per_second());
    // In proportion at other rates.
    let half = Usage::new(1, SampleRate::new(8_000).unwrap(), small(1).length);
    assert_eq!(half.bytes_per_second(), 8_334);
}

#[test]
fn the_reserve_covers_two_journals_and_a_flac_window_per_track() {
    // 1,500 samples: 3,000 bytes of journal, 3,750 with framing, twice;
    // and 1,563 bytes of FLAC.
    assert_eq!(small(1).reserve(), 2 * 3_750 + 1_563);
    assert_eq!(small(2).reserve(), 2 * (2 * 3_750 + 1_563));
    // Five minutes at 16 kHz: 12 MB of journal each, and 5 MB of FLAC.
    assert_eq!(speech(1).reserve(), 2 * 12 * MB + 5_000_100);
}

#[test]
fn the_time_left_is_what_s_past_the_reserve_at_the_rate() {
    let two = speech(2);
    let reserve = two.reserve();
    assert_eq!(two.time_left(0), Duration::ZERO);
    assert_eq!(two.time_left(reserve), Duration::ZERO);
    // Two tracks for two hours and ten minutes: 260 MB past the reserve.
    let free = reserve + 2 * 7_800 * two.bytes_per_second() / 2;
    assert_eq!(two.time_left(free), Duration::from_mins(130));
    assert_eq!(two.time_left(free - 1), Duration::from_secs(7_799));
    assert_eq!(
        two.time_left(u64::MAX).as_secs(),
        (u64::MAX - reserve) / 33_334
    );
}

#[test]
fn the_check_reads_the_fake_statvfs() {
    let fs = FakeFs::with_dirs(["/data/s"]);
    let usage = small(2);
    fs.set_capacity(Some(usage.reserve() + 2_084 * 90));
    let disk = check(&fs, &p("/data/s"), usage).unwrap();
    assert_eq!(disk.free_bytes, usage.reserve() + 2_084 * 90);
    assert_eq!(disk.left, Some(Duration::from_secs(90)));
    // Writing takes from it.
    let mut f = fs.create(&p("/data/s/journal")).unwrap();
    f.write_all(&vec![0; 2_084 * 30]).unwrap();
    let disk = check(&fs, &p("/data/s"), usage).unwrap();
    assert_eq!(disk.left, Some(Duration::from_secs(60)));
    assert!(check(&fs, &p("/data/missing"), usage).is_err());
}

#[test]
fn a_ballast_is_made_only_with_room_for_it_twice_over() {
    let fs = FakeFs::with_dirs(["/data"]);
    fs.set_capacity(Some(8_191));
    assert_eq!(
        Ballast::keep(&fs, &p("/data"), 4_096, || false).unwrap(),
        None
    );
    assert!(fs.paths().is_empty());
    fs.set_capacity(Some(8_192));
    let ballast = Ballast::keep(&fs, &p("/data"), 4_096, || false)
        .unwrap()
        .unwrap();
    assert_eq!(ballast.path(), p("/data/ballast"));
    assert_eq!(fs.paths(), [p("/data/ballast")]);
    assert_eq!(fs.free_space(&p("/data")).unwrap(), 4_096);
    // Durable, and noise rather than zeros.
    let survivor = fs.crash(CrashOutcome::LoseUnsynced);
    let bytes = survivor.read(&p("/data/ballast")).unwrap();
    assert_eq!(bytes.len(), 4_096);
    assert!(bytes.iter().map(|&b| u32::from(b == 0)).sum::<u32>() < 100);
}

#[test]
fn a_ballast_already_there_is_kept_and_a_leftover_temp_is_removed() {
    let fs = FakeFs::with_dirs(["/data"]);
    Ballast::keep(&fs, &p("/data"), 100, || false)
        .unwrap()
        .unwrap();
    let ops = fs.attempted();
    // Kept as it is, even with no room for another.
    fs.set_capacity(Some(100));
    let again = Ballast::keep(&fs, &p("/data"), 100, || false).unwrap();
    assert_eq!(
        again.map(|b| b.path().to_path_buf()),
        Some(p("/data/ballast"))
    );
    assert_eq!(fs.attempted(), ops + 1, "only a listing");

    let fs = FakeFs::with_dirs(["/data"]);
    let mut temp = fs.create(&p("/data/ballast.tmp")).unwrap();
    temp.write_all(b"half").unwrap();
    Ballast::keep(&fs, &p("/data"), 100, || false)
        .unwrap()
        .unwrap();
    assert_eq!(fs.paths(), [p("/data/ballast")]);
    assert_eq!(fs.read(&p("/data/ballast")).unwrap().len(), 100);
}

#[test]
fn giving_up_or_failing_while_writing_leaves_no_ballast() {
    let fs = FakeFs::with_dirs(["/data"]);
    let calls = std::cell::Cell::new(0);
    let give_up = || {
        calls.set(calls.get() + 1);
        calls.get() > 1
    };
    // Three megabytes: given up after the first one.
    let none = Ballast::keep(&fs, &p("/data"), 3 << 20, give_up).unwrap();
    assert_eq!(none, None);
    assert!(fs.paths().is_empty());

    // A disk that fills while it's written.
    let fs = FakeFs::with_dirs(["/data"]);
    let other = std::cell::RefCell::new(fs.create(&p("/data/other")).unwrap());
    fs.set_capacity(Some(3 << 20));
    let filling = || {
        // Someone else takes the room meanwhile.
        let _ = other.borrow_mut().write_all(&vec![1; 1 << 20]);
        false
    };
    let err = Ballast::keep(&fs, &p("/data"), 3 << 19, filling).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::StorageFull);
    assert_eq!(fs.paths(), [p("/data/other")]);
}

#[test]
fn a_crash_while_making_the_ballast_never_leaves_a_partial_one() {
    let clean = FakeFs::with_dirs(["/data"]);
    Ballast::keep(&clean, &p("/data"), 3_000, || false).unwrap();
    let ops = clean.attempted();
    assert!(ops >= 6, "{ops}");
    for at in 0..ops {
        for outcome in [CrashOutcome::LoseUnsynced, CrashOutcome::KeepAll] {
            let fs = FakeFs::with_dirs(["/data"]);
            fs.crash_after(at);
            assert!(Ballast::keep(&fs, &p("/data"), 3_000, || false).is_err());
            let survivor = fs.crash(outcome);
            if let Ok(bytes) = survivor.read(&p("/data/ballast")) {
                assert_eq!(bytes.len(), 3_000, "crash after {at}, {outcome:?}");
            }
            // The next start makes a whole one.
            Ballast::keep(&survivor, &p("/data"), 3_000, || false)
                .unwrap()
                .unwrap();
            assert_eq!(survivor.paths(), [p("/data/ballast")]);
            assert_eq!(survivor.read(&p("/data/ballast")).unwrap().len(), 3_000);
        }
    }
}

/// A watch over `fs` holding a ballast of `len` bytes in `/data`.
fn watched(fs: &FakeFs, len: u64) -> Arc<DiskWatch<FakeFs>> {
    let watch = DiskWatch::new(fs.clone());
    let ballast = Ballast::keep(fs, &p("/data"), len, || false)
        .unwrap()
        .unwrap();
    watch.hold(ballast);
    watch
}

#[test]
fn the_first_write_that_meets_a_full_disk_frees_the_ballast_before_failing() {
    let fs = FakeFs::with_dirs(["/data/s"]);
    let watch = watched(&fs, 1_000);
    fs.set_capacity(Some(1_500));
    let disk = watch.fs();
    let mut journal = disk.create(&p("/data/s/journal")).unwrap();
    journal.write_all(&[7; 400]).unwrap();
    assert_eq!(watch.full(), None);
    let err = journal.write_all(&[7; 400]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::StorageFull);
    // Freed by the time the error came back: the next try fits.
    assert_eq!(
        watch.full(),
        Some(Full {
            path: Some(p("/data/s/journal")),
            ballast: Freed::Freed,
        })
    );
    assert!(!watch.holds_ballast());
    assert_eq!(fs.free_space(&p("/data")).unwrap(), 1_000);
    let mut replacement = disk.create(&p("/data/s/journal-2")).unwrap();
    replacement.write_all(&[7; 400]).unwrap();
    // Its removal is durable.
    let survivor = fs.crash(CrashOutcome::LoseUnsynced);
    assert!(!survivor.paths().contains(&p("/data/ballast")));
}

#[test]
fn a_full_disk_is_noted_once_and_later_failures_change_nothing() {
    let fs = FakeFs::with_dirs(["/data/s"]);
    let watch = watched(&fs, 100);
    let disk = watch.fs();
    let mut file = disk.create(&p("/data/s/a")).unwrap();
    fs.fail_after(0, io::ErrorKind::QuotaExceeded);
    assert!(file.sync().is_err());
    let first = watch.full().unwrap();
    assert_eq!(first.path, Some(p("/data/s/a")));
    assert_eq!(first.ballast, Freed::Freed);
    fs.fail_after(0, io::ErrorKind::StorageFull);
    assert!(disk.create(&p("/data/s/b")).is_err());
    assert_eq!(watch.full(), Some(first));
}

#[test]
fn errors_that_arent_for_space_free_nothing() {
    let fs = FakeFs::with_dirs(["/data/s"]);
    let watch = watched(&fs, 100);
    let disk = watch.fs();
    let mut file = disk.create(&p("/data/s/a")).unwrap();
    for kind in [io::ErrorKind::Other, io::ErrorKind::PermissionDenied] {
        fs.fail_after(0, kind);
        assert!(file.write_all(b"x").is_err());
    }
    assert_eq!(watch.full(), None);
    assert!(watch.holds_ballast());
}

#[test]
fn every_kind_of_operation_is_watched_and_passed_on() {
    type Op = fn(&WatchedFs<FakeFs>) -> io::Result<()>;
    let ops: [(&str, Op); 6] = [
        ("create", |fs| fs.create(&p("/data/s/new")).map(drop)),
        ("create_dir", |fs| fs.create_dir(&p("/data/s/dir"))),
        ("rename", |fs| fs.rename(&p("/data/s/a"), &p("/data/s/b"))),
        ("sync_dir", |fs| fs.sync_dir(&p("/data/s"))),
        ("remove", |fs| fs.remove(&p("/data/s/a"))),
        ("syncer", |fs| {
            let f = fs.create(&p("/data/s/c"))?;
            let syncer = f.syncer()?;
            fs.inner.fail_after(0, io::ErrorKind::StorageFull);
            syncer.sync().map(drop)
        }),
    ];
    for (name, op) in ops {
        let fs = FakeFs::with_dirs(["/data/s"]);
        fs.create(&p("/data/s/a")).unwrap();
        let watch = watched(&fs, 100);
        let disk = watch.fs();
        if name != "syncer" {
            fs.fail_after(0, io::ErrorKind::StorageFull);
        }
        assert!(op(&disk).is_err(), "{name}");
        assert_eq!(
            watch.full().map(|f| f.ballast),
            Some(Freed::Freed),
            "{name}"
        );
    }
    // What isn't a change passes straight through.
    let fs = FakeFs::with_dirs(["/data/s"]);
    fs.set_capacity(Some(1_000));
    let watch = DiskWatch::new(fs);
    let disk = watch.fs();
    let mut f = disk.create(&p("/data/s/a")).unwrap();
    f.write_all(b"abc").unwrap();
    f.sync().unwrap();
    assert_eq!(disk.read(&p("/data/s/a")).unwrap(), b"abc");
    assert_eq!(disk.list(&p("/data/s")).unwrap(), [p("/data/s/a")]);
    assert_eq!(disk.free_space(&p("/data/s")).unwrap(), 997);
    let _lock = disk.lock_dir(&p("/data/s")).unwrap();
    assert!(disk.lock_dir(&p("/data/s")).is_err());
}

#[test]
fn a_full_disk_without_a_ballast_says_so_and_one_held_later_is_freed_at_once() {
    let fs = FakeFs::with_dirs(["/data"]);
    let watch = DiskWatch::new(fs.clone());
    watch.note_full(None);
    assert_eq!(
        watch.full(),
        Some(Full {
            path: None,
            ballast: Freed::None,
        })
    );
    let ballast = Ballast::keep(&fs, &p("/data"), 100, || false)
        .unwrap()
        .unwrap();
    watch.hold(ballast);
    assert!(!watch.holds_ballast());
    assert!(fs.paths().is_empty());
    assert_eq!(watch.full().map(|f| f.ballast), Some(Freed::Freed));
}

#[test]
fn a_ballast_that_cant_be_removed_is_reported() {
    use crate::fs::fake::Fault;
    let fs = FakeFs::with_dirs(["/data"]);
    let watch = watched(&fs, 100);
    fs.fail_on(
        &p("/data/ballast"),
        Fault::Remove,
        io::ErrorKind::PermissionDenied,
    );
    watch.note_full(None);
    assert_eq!(
        watch.full().map(|f| f.ballast),
        Some(Freed::Failed(io::ErrorKind::PermissionDenied))
    );
}

#[test]
fn a_disk_with_less_than_the_reserve_free_records_on_with_the_warning() {
    // Two 16 kHz tracks keep back 58 MB; with 50 MB free, nothing is left
    // by the estimate, but the disk isn't full: nota records what fits.
    let usage = speech(2);
    let fs = FakeFs::with_dirs(["/data/s"]);
    fs.set_capacity(Some(50 * MB));
    let watch = DiskWatch::new(fs);
    let (monitor, reports) = spawn_monitor(&watch, config(usage, 100, WAIT));
    assert_eq!(
        reports.recv_timeout(WAIT).unwrap(),
        DiskReport::Space(Disk {
            free_bytes: 50 * MB,
            left: Some(Duration::ZERO),
        })
    );
    assert_eq!(
        reports.recv_timeout(WAIT).unwrap(),
        DiskReport::Low(WarningState::Raised)
    );
    assert_eq!(
        reports.recv_timeout(WAIT).unwrap(),
        DiskReport::Ballast(Ok(true))
    );
    assert_eq!(monitor.stop().unwrap().full, None);
    assert_eq!(watch.full(), None);
}

/// Long enough for any thread here, short of a hang.
const WAIT: Duration = Duration::from_secs(10);

fn config(usage: Usage, ballast_len: u64, interval: Duration) -> MonitorConfig {
    MonitorConfig {
        data_dir: p("/data"),
        audio_dir: p("/data/s"),
        usage,
        ballast_len,
        interval,
    }
}

fn spawn_monitor(
    watch: &Arc<DiskWatch<FakeFs>>,
    config: MonitorConfig,
) -> (DiskMonitor<FakeFs>, mpsc::Receiver<DiskReport>) {
    let (tx, rx) = mpsc::channel();
    let monitor = DiskMonitor::spawn(Arc::clone(watch), config, move |report| {
        let _ = tx.send(report);
    })
    .unwrap();
    (monitor, rx)
}

#[test]
fn the_monitor_checks_at_once_keeps_the_ballast_and_warns_when_low() {
    let usage = small(2);
    let fs = FakeFs::with_dirs(["/data/s"]);
    // Room for the ballast twice over, and minutes of recording past the
    // floor after the ballast: under the warning.
    let capacity = 2 * 10_000 + FULL_FLOOR + usage.reserve() + 2_084 * 5;
    fs.set_capacity(Some(capacity));
    let watch = DiskWatch::new(fs.clone());
    let (monitor, reports) = spawn_monitor(&watch, config(usage, 10_000, WAIT));
    assert_eq!(
        reports.recv_timeout(WAIT).unwrap(),
        DiskReport::Space(Disk {
            free_bytes: capacity,
            left: Some(usage.time_left(capacity)),
        })
    );
    assert_eq!(
        reports.recv_timeout(WAIT).unwrap(),
        DiskReport::Low(WarningState::Raised)
    );
    assert_eq!(
        reports.recv_timeout(WAIT).unwrap(),
        DiskReport::Ballast(Ok(true))
    );
    let summary = monitor.stop().unwrap();
    assert!(summary.ballast_held);
    assert_eq!(summary.full, None);
    assert_eq!(fs.paths(), [p("/data/ballast")]);
    assert!(watch.holds_ballast());
}

#[test]
fn the_monitor_clears_the_warning_when_room_comes_back() {
    let usage = small(1);
    let fs = FakeFs::with_dirs(["/data/s"]);
    // Half an hour left while the hog is there; over an hour once it's gone.
    let mut hog = fs.create(&p("/data/s/hog")).unwrap();
    hog.write_all(&vec![1; 4_000_000]).unwrap();
    fs.set_capacity(Some(4_000_000 + FULL_FLOOR + usage.reserve() + 1_042 * 600));
    let watch = DiskWatch::new(fs.clone());
    let (monitor, reports) =
        spawn_monitor(&watch, config(usage, 1 << 30, Duration::from_millis(5)));
    let mut seen = Vec::new();
    while !seen.contains(&DiskReport::Low(WarningState::Raised)) {
        seen.push(reports.recv_timeout(WAIT).unwrap());
    }
    fs.remove(&p("/data/s/hog")).unwrap();
    while seen.last() != Some(&DiskReport::Low(WarningState::Cleared)) {
        seen.push(reports.recv_timeout(WAIT).unwrap());
    }
    let summary = monitor.stop().unwrap();
    // No room for a ballast of a gigabyte: none, and no error.
    assert!(!summary.ballast_held);
    assert_eq!(summary.ballast_error, None);
    assert_eq!(
        seen.iter()
            .filter(|r| matches!(r, DiskReport::Low(_)))
            .count(),
        2,
        "{seen:?}"
    );
}

#[test]
fn the_monitor_reports_a_full_disk_at_once() {
    let usage = small(1);
    let fs = FakeFs::with_dirs(["/data/s"]);
    let watch = DiskWatch::new(fs.clone());
    let (monitor, reports) = spawn_monitor(&watch, config(usage, 1_000, WAIT));
    // The first check, then the ballast is made.
    assert!(matches!(
        reports.recv_timeout(WAIT).unwrap(),
        DiskReport::Space(_)
    ));
    assert_eq!(
        reports.recv_timeout(WAIT).unwrap(),
        DiskReport::Ballast(Ok(true))
    );
    // A journal meets a full disk: the monitor says so well before its
    // next check, an interval of ten seconds away.
    fs.set_capacity(Some(1_000 + 10));
    let mut journal = watch.fs().create(&p("/data/s/journal")).unwrap();
    assert!(journal.write_all(&[0; 20]).is_err());
    let full = loop {
        if let DiskReport::Full(full) = reports.recv_timeout(Duration::from_secs(5)).unwrap() {
            break full;
        }
    };
    assert_eq!(full.path, Some(p("/data/s/journal")));
    assert_eq!(full.ballast, Freed::Freed);
    let summary = monitor.stop().unwrap();
    assert_eq!(summary.full, Some(full));
    assert!(summary.ballast_held);
}

#[test]
fn a_check_with_less_than_the_floor_free_fills_the_disk() {
    let usage = small(1);
    let fs = FakeFs::with_dirs(["/data/s"]);
    fs.set_capacity(Some(FULL_FLOOR - 1));
    let watch = DiskWatch::new(fs.clone());
    let (monitor, reports) = spawn_monitor(&watch, config(usage, 1_000, WAIT));
    let got: Vec<DiskReport> = (0..4)
        .map(|_| reports.recv_timeout(WAIT).unwrap())
        .collect();
    assert_eq!(
        got,
        [
            DiskReport::Space(Disk {
                free_bytes: FULL_FLOOR - 1,
                left: Some(usage.time_left(FULL_FLOOR - 1)),
            }),
            DiskReport::Low(WarningState::Raised),
            DiskReport::Ballast(Ok(false)),
            DiskReport::Full(Full {
                path: None,
                ballast: Freed::None,
            }),
        ]
    );
    let summary = monitor.stop().unwrap();
    assert!(!summary.ballast_held);
    assert!(fs.paths().is_empty(), "no ballast is made on a full disk");
}

#[test]
fn a_check_that_fails_is_reported_once_and_recording_goes_on() {
    let fs = FakeFs::with_dirs(["/data"]);
    let watch = DiskWatch::new(fs);
    // No audio directory to check.
    let (monitor, reports) = spawn_monitor(&watch, config(small(1), 100, Duration::from_millis(1)));
    let first = reports.recv_timeout(WAIT).unwrap();
    assert!(matches!(first, DiskReport::Unchecked(_)), "{first:?}");
    assert_eq!(
        reports.recv_timeout(WAIT).unwrap(),
        DiskReport::Ballast(Ok(true))
    );
    // Many more checks fail, unreported.
    assert!(reports.recv_timeout(Duration::from_millis(100)).is_err());
    let summary = monitor.stop().unwrap();
    assert!(
        reports
            .try_iter()
            .all(|r| !matches!(r, DiskReport::Unchecked(_)))
    );
    assert_eq!(summary.full, None);
}

#[test]
fn a_ballast_that_cant_be_made_is_in_the_summary() {
    use crate::fs::fake::Fault;
    let fs = FakeFs::with_dirs(["/data/s"]);
    fs.fail_on(
        &p("/data/ballast.tmp"),
        Fault::Create,
        io::ErrorKind::PermissionDenied,
    );
    let watch = DiskWatch::new(fs);
    let (monitor, reports) = spawn_monitor(&watch, config(small(1), 100, WAIT));
    let ballast = loop {
        if let DiskReport::Ballast(b) = reports.recv_timeout(WAIT).unwrap() {
            break b;
        }
    };
    assert!(ballast.is_err(), "{ballast:?}");
    let summary = monitor.stop().unwrap();
    assert!(!summary.ballast_held);
    assert!(summary.ballast_error.is_some());
}

#[test]
fn a_dropped_monitor_stops_its_thread() {
    let fs = FakeFs::with_dirs(["/data/s"]);
    let watch = DiskWatch::new(fs);
    let (monitor, reports) = spawn_monitor(&watch, config(small(1), 100, WAIT));
    drop(monitor);
    // The thread ends, dropping its sender.
    loop {
        match reports.recv_timeout(WAIT) {
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => panic!("the monitor kept running"),
        }
    }
}
