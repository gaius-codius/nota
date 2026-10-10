use nota_core::recorder::{Disk, Level};
use nota_core::{SampleIndex, SampleRate, TrackTimeline};

use super::*;

const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);

fn secs(s: u64) -> SessionTime {
    SessionTime::from_elapsed(Duration::from_secs(s)).unwrap()
}

/// Warnings for a mic and the system audio, as `nota record` records.
fn two_tracks() -> Warnings {
    let mut warnings = Warnings::default();
    warnings.set_tracks(vec![
        Track {
            id: MIC,
            role: TrackRole::Microphone,
            source: "mic".to_owned(),
            recording: true,
        },
        Track {
            id: SYSTEM,
            role: TrackRole::System,
            source: "system audio".to_owned(),
            recording: true,
        },
    ]);
    warnings
}

fn warning(cause: Cause, track: Option<TrackId>, at: u64, state: WarningState) -> Event {
    Event::Warning(Warning {
        cause,
        track,
        at: secs(at),
        state,
    })
}

fn raise(warnings: &mut Warnings, cause: Cause, track: Option<TrackId>, at: u64) {
    warnings.update(&warning(cause, track, at, WarningState::Raised), secs(at));
}

fn clear(warnings: &mut Warnings, cause: Cause, track: Option<TrackId>, at: u64) {
    warnings.update(&warning(cause, track, at, WarningState::Cleared), secs(at));
}

fn device(warnings: &mut Warnings, track: TrackId, change: DeviceChange, at: u64) {
    let event = Event::Device {
        track,
        change,
        at: secs(at),
    };
    warnings.update(&event, secs(at));
}

fn disk(warnings: &mut Warnings, left_minutes: u64) {
    let event = Event::Disk(Disk {
        free_bytes: 1,
        left: Some(Duration::from_secs(left_minutes * 60)),
    });
    warnings.update(&event, secs(0));
}

fn shown(text: &str, tone: Tone, more: usize) -> Shown {
    Shown {
        text: text.to_owned(),
        tone,
        more,
    }
}

/// Each cause, raised alone, shows its words in its colour, and the top
/// border goes back to `● REC` once it clears.
#[test]
fn each_warning_shows_its_words_and_clears_with_its_cause() {
    // Quiet and nothing playing warn only while every track is silent, so
    // for those the other track has been quiet since 0 s; alone, that
    // warns of nothing.
    let cases: [(Cause, Option<TrackId>, bool, &str, Tone); 9] = [
        (
            Cause::StreamFailed("gone".into()),
            Some(MIC),
            false,
            "⚠ mic not recording",
            Tone::Accent,
        ),
        (
            Cause::Stalled,
            Some(SYSTEM),
            false,
            "⚠ system not responding",
            Tone::Accent,
        ),
        (
            Cause::DigitalZeros,
            Some(MIC),
            false,
            "⚠ mic muted",
            Tone::Gold,
        ),
        (Cause::DiskLow, None, false, "⚠ disk: 40m left", Tone::Gold),
        (
            Cause::LibraryUnavailable,
            None,
            false,
            "⚠ library offline",
            Tone::Gold,
        ),
        (Cause::SleepNotHeld, None, false, "⚠ may sleep", Tone::Gold),
        (Cause::Quiet, Some(MIC), true, "⚠ quiet 0:30", Tone::Gold),
        (Cause::Quiet, Some(SYSTEM), true, "⚠ quiet 0:30", Tone::Gold),
        (
            Cause::DigitalZeros,
            Some(SYSTEM),
            true,
            "⚠ system: nothing playing",
            Tone::Gold,
        ),
    ];
    for (cause, track, other_quiet, words, tone) in cases {
        let mut warnings = two_tracks();
        disk(&mut warnings, 40);
        if other_quiet {
            let other = if track == Some(MIC) { SYSTEM } else { MIC };
            raise(&mut warnings, Cause::Quiet, Some(other), 0);
        }
        raise(&mut warnings, cause.clone(), track, 30);
        // Nothing playing leaves every track silent, and one quiet: the
        // quiet is counted behind it.
        let more = usize::from(matches!(cause, Cause::DigitalZeros) && other_quiet);
        assert_eq!(
            warnings.top(secs(60)),
            Some(shown(words, tone, more)),
            "{cause:?}"
        );
        clear(&mut warnings, cause.clone(), track, 70);
        assert_eq!(warnings.top(secs(70)), None, "{cause:?} after its clear");
    }
}

/// A lost device warns until it comes back, and its coming back shows
/// `✓ mic back` for 10 s, then nothing.
#[test]
fn a_lost_device_warns_until_it_is_back() {
    let mut warnings = two_tracks();
    device(&mut warnings, MIC, DeviceChange::Lost, 5);
    assert_eq!(
        warnings.top(secs(6)),
        Some(shown("⚠ mic lost", Tone::Accent, 0))
    );
    device(
        &mut warnings,
        MIC,
        DeviceChange::Changed("Headset".into()),
        20,
    );
    assert_eq!(
        warnings.top(secs(20)),
        Some(shown("✓ mic back", Tone::Good, 0))
    );
    assert_eq!(
        warnings.top(secs(29)),
        Some(shown("✓ mic back", Tone::Good, 0))
    );
    assert_eq!(warnings.top(secs(30)), None);
    assert_eq!(
        warnings.sources().as_deref(),
        Some("Headset + system audio")
    );
}

/// The transcriber offline warns that live text is off until it's back.
#[test]
fn live_text_is_off_while_the_engine_is_down() {
    let mut warnings = two_tracks();
    let offline = Event::Engine(EngineState::Offline("it exited".into()));
    warnings.update(&offline, secs(1));
    assert_eq!(
        warnings.top(secs(2)),
        Some(shown("⚠ live text off", Tone::Gold, 0))
    );
    warnings.update(&Event::Engine(EngineState::Online), secs(3));
    assert_eq!(warnings.top(secs(3)), None);
}

/// A route change shows where the track went for 10 s, and the footer
/// names the new device from then on.
#[test]
fn a_route_change_shows_for_ten_seconds() {
    let mut warnings = two_tracks();
    device(
        &mut warnings,
        MIC,
        DeviceChange::Changed("Headphones".into()),
        100,
    );
    assert_eq!(
        warnings.top(secs(105)),
        Some(shown("↪ mic: Headphones", Tone::Plain, 0))
    );
    assert_eq!(warnings.top(secs(110)), None);
    assert_eq!(
        warnings.sources().as_deref(),
        Some("Headphones + system audio")
    );
    assert_eq!(warnings.changes(), [secs(100)]);
}

/// The most severe condition shows, with a count of the rest.
#[test]
fn the_most_severe_shows_with_a_count_of_the_rest() {
    let mut warnings = two_tracks();
    disk(&mut warnings, 40);
    raise(&mut warnings, Cause::SleepNotHeld, None, 0);
    raise(&mut warnings, Cause::DiskLow, None, 1);
    assert_eq!(
        warnings.top(secs(2)),
        Some(shown("⚠ disk: 40m left", Tone::Gold, 1))
    );
    raise(&mut warnings, Cause::DigitalZeros, Some(MIC), 2);
    assert_eq!(
        warnings.top(secs(3)),
        Some(shown("⚠ mic muted", Tone::Gold, 2))
    );
    device(&mut warnings, SYSTEM, DeviceChange::Lost, 4);
    assert_eq!(
        warnings.top(secs(5)),
        Some(shown("⚠ system lost", Tone::Accent, 3))
    );
    // Under 15 minutes, the disk comes before a muted mic, in the accent
    // colour.
    disk(&mut warnings, 12);
    assert_eq!(
        warnings.conditions(secs(5)),
        [
            Condition::Lost(SYSTEM),
            Condition::DiskCritical,
            Condition::Muted(MIC),
            Condition::MaySleep
        ]
    );
}

/// A quiet mic while the system audio plays is normal: quiet and nothing
/// playing warn only while every track is silent.
#[test]
fn one_quiet_track_is_not_a_warning() {
    let mut warnings = two_tracks();
    raise(&mut warnings, Cause::Quiet, Some(MIC), 10);
    assert_eq!(warnings.top(secs(50)), None);
    raise(&mut warnings, Cause::Quiet, Some(SYSTEM), 20);
    // Silent since the second went quiet.
    assert_eq!(
        warnings.top(secs(62)),
        Some(shown("⚠ quiet 0:42", Tone::Gold, 0))
    );
    clear(&mut warnings, Cause::Quiet, Some(SYSTEM), 70);
    assert_eq!(warnings.top(secs(71)), None);
}

/// Exact zeros on the system audio while the mic carries the lecture
/// aren't worth a warning, but zeros on the mic are a fault on their own.
#[test]
fn zeros_warn_on_the_mic_alone_but_not_on_the_system_audio_alone() {
    let mut warnings = two_tracks();
    raise(&mut warnings, Cause::DigitalZeros, Some(SYSTEM), 10);
    assert_eq!(warnings.top(secs(40)), None);
    assert!(warnings.changes().is_empty());
    raise(&mut warnings, Cause::DigitalZeros, Some(MIC), 30);
    assert_eq!(
        warnings.top(secs(40)),
        Some(shown("⚠ mic muted", Tone::Gold, 1))
    );
    assert_eq!(warnings.changes(), [secs(30)]);
}

/// An event shows over a gold condition, but never over a loss.
#[test]
fn an_event_gives_way_to_a_loss_but_not_to_a_gold_condition() {
    let mut warnings = two_tracks();
    raise(&mut warnings, Cause::SleepNotHeld, None, 0);
    device(
        &mut warnings,
        SYSTEM,
        DeviceChange::Changed("HDMI".into()),
        10,
    );
    assert_eq!(
        warnings.top(secs(11)),
        Some(shown("↪ system: HDMI", Tone::Plain, 1))
    );
    raise(&mut warnings, Cause::Stalled, Some(MIC), 12);
    assert_eq!(
        warnings.top(secs(12)),
        Some(shown("⚠ mic not responding", Tone::Accent, 1))
    );
}

/// A sleep shows with the length of the gap after it, once the gap comes.
#[test]
fn a_sleep_shows_how_long_it_lasted() {
    let mut warnings = two_tracks();
    raise(&mut warnings, Cause::Slept, None, 200);
    assert_eq!(
        warnings.top(secs(200)),
        Some(shown("⚠ slept", Tone::Gold, 0))
    );
    let rate = SampleRate::new(1_000).unwrap();
    let mut timeline = TrackTimeline::new(MIC);
    timeline
        .open_epoch(secs(0), SampleIndex::ZERO, rate)
        .unwrap();
    timeline
        .open_epoch(secs(200), SampleIndex::new(108_000), rate)
        .unwrap();
    let gap = timeline.gaps().next().unwrap();
    warnings.update(&Event::Gap { track: MIC, gap }, secs(201));
    assert_eq!(
        warnings.top(secs(201)),
        Some(shown("⚠ slept 1m 32s", Tone::Gold, 0))
    );
    assert_eq!(warnings.top(secs(210)), None);
}

/// A broken journal shows `not recording` for 10 s: recording goes on in
/// a new one.
#[test]
fn a_broken_journal_shows_for_ten_seconds() {
    let mut warnings = two_tracks();
    raise(
        &mut warnings,
        Cause::JournalFailed("no space".into()),
        Some(SYSTEM),
        50,
    );
    assert_eq!(
        warnings.top(secs(59)),
        Some(shown("⚠ system not recording", Tone::Accent, 0))
    );
    assert_eq!(warnings.top(secs(60)), None);
    assert_eq!(warnings.changes(), [secs(50)]);
}

/// A refused permission is a track not recording; a format change is only
/// a change on the band.
#[test]
fn a_refusal_is_not_recording_and_a_format_change_is_a_mark() {
    let mut warnings = two_tracks();
    device(&mut warnings, MIC, DeviceChange::Format, 3);
    assert_eq!(warnings.top(secs(3)), None);
    device(&mut warnings, MIC, DeviceChange::PermissionDenied, 4);
    assert_eq!(
        warnings.top(secs(4)),
        Some(shown("⚠ mic not recording", Tone::Accent, 0))
    );
    assert_eq!(warnings.changes(), [secs(3), secs(4)]);
}

/// Changes are kept in session order, whatever order they come in.
#[test]
fn changes_are_kept_in_order() {
    let mut warnings = two_tracks();
    raise(&mut warnings, Cause::Stalled, Some(MIC), 30);
    device(&mut warnings, SYSTEM, DeviceChange::Lost, 10);
    raise(
        &mut warnings,
        Cause::StreamFailed("gone".into()),
        Some(SYSTEM),
        20,
    );
    // Clears aren't changes.
    clear(&mut warnings, Cause::Stalled, Some(MIC), 40);
    assert_eq!(warnings.changes(), [secs(10), secs(20), secs(30)]);
}

/// A full disk is kept as the end of the recording, with its time.
#[test]
fn a_full_disk_is_the_end() {
    let mut warnings = two_tracks();
    assert_eq!(warnings.stopped_by_full_disk(), None);
    raise(&mut warnings, Cause::DiskFull, None, 4_368);
    assert_eq!(warnings.stopped_by_full_disk(), Some(secs(4_368)));
}

/// Drift is corrected as it's measured, so the screen says nothing of it.
#[test]
fn drift_shows_nothing() {
    let mut warnings = two_tracks();
    raise(&mut warnings, Cause::Drift, Some(MIC), 1);
    assert_eq!(warnings.top(secs(1)), None);
    assert!(warnings.changes().is_empty());
}

/// A track the screen wasn't told of is named by its number, and its
/// zeros are taken for a muted mic.
#[test]
fn an_unknown_track_is_named_by_its_number() {
    let mut warnings = Warnings::default();
    let level = Event::Level {
        track: TrackId::new(7),
        at: secs(1),
        level: Level::SILENT,
    };
    warnings.update(&level, secs(1));
    raise(&mut warnings, Cause::DigitalZeros, Some(TrackId::new(7)), 2);
    assert_eq!(
        warnings.top(secs(9)),
        Some(shown("⚠ track 7 muted", Tone::Gold, 0))
    );
    assert_eq!(warnings.sources(), None);
}

#[test]
fn durations_read_as_the_spec_writes_them() {
    assert_eq!(duration_words(Duration::from_secs(45)), "45s");
    assert_eq!(duration_words(Duration::from_secs(92)), "1m 32s");
    assert_eq!(duration_words(Duration::from_mins(123)), "2h 3m");
    assert_eq!(minutes_seconds(Duration::from_secs(42)), "0:42");
    assert_eq!(minutes_seconds(Duration::from_secs(725)), "12:05");
}

/// A stalled track records nothing, so it counts as silent: with the
/// other track quiet, quiet is counted behind `not responding`.
#[test]
fn a_stalled_track_counts_as_silent() {
    let mut warnings = two_tracks();
    raise(&mut warnings, Cause::Stalled, Some(MIC), 10);
    raise(&mut warnings, Cause::Quiet, Some(SYSTEM), 20);
    assert_eq!(
        warnings.top(secs(30)),
        Some(shown("⚠ mic not responding", Tone::Accent, 1))
    );
}

/// A route change's device name is the audio server's: control and
/// bidirectional formatting characters are dropped from the words and
/// the footer.
#[test]
fn a_route_change_keeps_only_what_is_drawn() {
    let mut warnings = two_tracks();
    device(
        &mut warnings,
        MIC,
        DeviceChange::Changed("Head\u{202e}set\u{7}".into()),
        5,
    );
    assert_eq!(
        warnings.top(secs(5)),
        Some(shown("↪ mic: Headset", Tone::Plain, 0))
    );
    assert_eq!(
        warnings.sources().as_deref(),
        Some("Headset + system audio")
    );
}

/// A journal broken with no track named is a change on the band, but no
/// track is said not to record.
#[test]
fn a_journal_broken_without_a_track_is_only_a_change() {
    let mut warnings = two_tracks();
    raise(
        &mut warnings,
        Cause::JournalFailed("fsync".into()),
        None,
        40,
    );
    assert_eq!(warnings.top(secs(41)), None);
    assert_eq!(warnings.changes(), [secs(40)]);
}

/// A track given as not recording keeps its role, so its failure to start
/// reads `⚠ mic not recording`, but the footer names only the source still
/// recording.
#[test]
fn a_track_that_didn_t_start_is_named_by_its_warning_but_not_the_footer() {
    let mut warnings = Warnings::default();
    warnings.set_tracks(vec![
        Track {
            id: MIC,
            role: TrackRole::Microphone,
            source: "mic".to_owned(),
            recording: false,
        },
        Track {
            id: SYSTEM,
            role: TrackRole::System,
            source: "system audio".to_owned(),
            recording: true,
        },
    ]);
    raise(
        &mut warnings,
        Cause::StreamFailed("permission refused".into()),
        Some(MIC),
        2,
    );
    assert_eq!(
        warnings.top(secs(2)),
        Some(shown("⚠ mic not recording", Tone::Accent, 0))
    );
    assert_eq!(warnings.sources(), Some("system audio".to_owned()));
}
