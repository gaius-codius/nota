//! The stamps across each route change a stream reported in the
//! `first-epoch` mode (GAI-492).
//!
//! The recorder opens a route change's epoch at the first buffer after the
//! report whose cycle or capture stamp is [`LOSS_MIN`] or more from where
//! the buffer before it puts it, and failing that at the first buffer whose
//! cycle is [`REOPEN_WAIT`] past the report (`Reroutes` in `capture.rs`).
//! Which stamp breaks on a real switch, and by how much, decides whether
//! the old device's last audio stays in its own epoch, and nothing past the
//! sender shows it. So for each report this prints the buffers around it,
//! each with its raw stamps and how far each stamp is from where the buffer
//! before it puts it (`stamp` lines, with a `skip` line where buffers are
//! left out), and one `switch` line: which stamp broke, by how much and how
//! long after the report, or that neither did and the wait reopened, and
//! then whether one broke later. A `switches` line counts them.
//!
//! Only the stream's own reports are read. `PipeWireBackend`'s watch, which
//! this mode's backend leaves out, reports the same change within
//! microseconds as a rule (GAI-494), so the wait is counted from the
//! stream's report here; a change only the watch hears isn't seen. The
//! harness reads the clock for a report a moment before the sender does,
//! so a buffer whose cycle falls in those microseconds can be taken for the
//! wait's one buffer early. Each report is judged on its own, where the
//! recorder waits from the first of two reports it hasn't reopened for yet:
//! a switch reported twice shows as two `switch` lines. The epochs shown
//! for a report are the track's later epochs that start from [`BEFORE`]
//! before it to [`BEFORE`] before the next report.

use std::io;
use std::ops::Range;
use std::time::Duration;

use nota_core::TrackId;
use nota_core::drift::LOSS_MIN;

use super::{RATE, Raw, len_ms, length, millis};

/// How long after a change's report the recorder still waits for a stamp
/// to break before it reopens anyway: a copy of its `REOPEN_WAIT`.
const REOPEN_WAIT: Duration = Duration::from_millis(200); // check-bound

/// How far before a report a break is looked for: the new device's audio
/// may come before the stream hears of the switch, and the recorder then
/// has no break left to wait for.
const BEFORE: Duration = Duration::from_millis(300);

/// How far after a report a break is looked for once the wait has
/// reopened: a stamp that breaks after the wait puts the new device's
/// first audio in the old device's epoch.
const LATE: Duration = Duration::from_secs(1);

/// How many buffers are printed either side of each one a `switch` line
/// names.
const AROUND: usize = 4;

/// A route change the stream reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Report {
    /// When, on the session clock's elapsed time.
    pub(super) at: Duration,
    /// How many buffers the stream had noted by then: the buffer at this
    /// place is the first to come after the report.
    pub(super) next: usize,
}

/// How far a buffer's stamps are from where the buffer before it puts
/// them (its own stamps plus its length), in nanoseconds, later positive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Jump {
    /// The cycle's.
    cycle: i128,
    /// The capture stamp's.
    capture: i128,
}

/// Which of a buffer's stamps broke from the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Broke {
    /// Neither: both are within [`LOSS_MIN`] of where the run puts them.
    Neither,
    /// The cycle alone.
    Cycle,
    /// The capture stamp alone.
    Capture,
    /// Both.
    Both,
}

impl Broke {
    /// The word for it on a `switch` line.
    fn label(self) -> &'static str {
        match self {
            Self::Neither => "neither",
            Self::Cycle => "cycle",
            Self::Capture => "capture",
            Self::Both => "both",
        }
    }
}

impl Jump {
    /// Which stamps moved by [`LOSS_MIN`] or more either way, as the
    /// recorder judges a break.
    fn broke(self) -> Broke {
        let min = i128::try_from(LOSS_MIN.as_nanos()).unwrap_or(i128::MAX);
        match (self.cycle.abs() >= min, self.capture.abs() >= min) {
            (false, false) => Broke::Neither,
            (true, false) => Broke::Cycle,
            (false, true) => Broke::Capture,
            (true, true) => Broke::Both,
        }
    }
}

/// `d` in nanoseconds, signed so two can be taken apart.
fn nanos(d: Duration) -> i128 {
    i128::try_from(d.as_nanos()).unwrap_or(i128::MAX)
}

/// `nanos` in milliseconds, for printing.
fn signed_ms(nanos: i128) -> f64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "a stamp's offset is far below 2^52 nanoseconds"
    )]
    let nanos = nanos as f64;
    nanos / 1e6
}

/// How far the buffer at `index` is from where the one before it puts it;
/// `None` for the first buffer, or where either has no stamp the session
/// clock placed.
fn jump(buffers: &[Raw], index: usize) -> Option<Jump> {
    let before = buffers.get(index.checked_sub(1)?)?;
    let (Some(run), Some(this)) = (before.placed, buffers.get(index)?.placed) else {
        return None;
    };
    let len = nanos(length(before.len, RATE)?);
    let off = |at: Duration, from: Duration| nanos(at) - (nanos(from) + len);
    Some(Jump {
        cycle: off(this.cycle, run.cycle),
        capture: off(this.capture, run.capture),
    })
}

/// The buffer at `index`'s jump, if it broke from the run.
fn broken(buffers: &[Raw], index: usize) -> Option<(usize, Jump)> {
    jump(buffers, index)
        .filter(|jump| jump.broke() != Broke::Neither)
        .map(|jump| (index, jump))
}

/// Where the recorder's rule reopens for a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reopening {
    /// At the buffer at `index`, whose stamps broke from the run.
    Break {
        /// Its place among the buffers.
        index: usize,
        /// How far its stamps moved.
        jump: Jump,
    },
    /// At the buffer at `index`, which, or the one before which, had no
    /// stamp the session clock placed: there's nothing to compare, so it
    /// reopens.
    Unstamped {
        /// Its place among the buffers.
        index: usize,
    },
    /// At the buffer at `index`, the first whose cycle is [`REOPEN_WAIT`]
    /// past the report, none having broken before it.
    Wait {
        /// Its place among the buffers.
        index: usize,
    },
    /// Nowhere: the buffers noted ran out first.
    Ended,
}

impl Reopening {
    /// The buffer it reopens at, if any.
    fn index(self) -> Option<usize> {
        match self {
            Self::Break { index, .. } | Self::Unstamped { index } | Self::Wait { index } => {
                Some(index)
            }
            Self::Ended => None,
        }
    }
}

/// Where the recorder reopens for `report` among `buffers`, by its rule: the
/// first buffer after the report that breaks from the run, or has nothing to
/// compare, or whose cycle is [`REOPEN_WAIT`] past the report.
fn reopening(buffers: &[Raw], report: Report) -> Reopening {
    let due = report.at.saturating_add(REOPEN_WAIT);
    for (index, raw) in buffers.iter().enumerate().skip(report.next) {
        let Some(jump) = jump(buffers, index) else {
            return Reopening::Unstamped { index };
        };
        if jump.broke() != Broke::Neither {
            return Reopening::Break { index, jump };
        }
        if raw.placed.is_some_and(|placed| placed.cycle >= due) {
            return Reopening::Wait { index };
        }
    }
    Reopening::Ended
}

/// The last buffer before `report` that broke from the run, with its jump,
/// among those whose cycle is within [`BEFORE`] of it.
fn break_before(buffers: &[Raw], report: Report) -> Option<(usize, Jump)> {
    let from = report.at.saturating_sub(BEFORE);
    (1..report.next.min(buffers.len()))
        .rev()
        .take_while(|&index| {
            buffers
                .get(index)
                .and_then(|raw| raw.placed)
                .is_some_and(|placed| placed.cycle >= from)
        })
        .find_map(|index| broken(buffers, index))
}

/// After a reopening by the wait, the first later buffer that broke from
/// the run, before the buffer at `until` (the next report's first) and
/// within [`LATE`] of `report`. `None` after any other reopening.
fn break_after_wait(
    buffers: &[Raw],
    report: Report,
    reopening: Reopening,
    until: usize,
) -> Option<(usize, Jump)> {
    let Reopening::Wait { index } = reopening else {
        return None;
    };
    let end = report.at.saturating_add(LATE);
    (index + 1..until.min(buffers.len()))
        .take_while(|&index| {
            buffers
                .get(index)
                .and_then(|raw| raw.placed)
                .is_some_and(|placed| placed.cycle < end)
        })
        .find_map(|index| broken(buffers, index))
}

/// For each of `reports`, those of `epochs` (the track's epochs after its
/// first, their starts in session nanoseconds) that start from [`BEFORE`]
/// before it to [`BEFORE`] before the next report.
fn epochs_for(reports: &[Report], epochs: &[u64]) -> Vec<Vec<u64>> {
    let before = |report: &Report| nanos(report.at.saturating_sub(BEFORE));
    reports
        .iter()
        .enumerate()
        .map(|(k, report)| {
            let from = before(report);
            let to = reports.get(k + 1).map_or(i128::MAX, before);
            epochs
                .iter()
                .copied()
                .filter(|&start| (from..to).contains(&i128::from(start)))
                .collect()
        })
        .collect()
}

/// What one report's lines are made from.
#[derive(Debug, Clone, Copy)]
struct Switch {
    /// The track.
    track: u32,
    /// The report's place among the track's reports, from 1.
    number: usize,
    /// The report.
    report: Report,
    /// Where the recorder's rule reopens for it.
    reopening: Reopening,
    /// A break in the stamps just before it, if any.
    before: Option<(usize, Jump)>,
    /// After a reopening by the wait, a break that came later, if any.
    late: Option<(usize, Jump)>,
}

impl Switch {
    /// `report`, `number` among `track`'s, judged against `buffers`, with
    /// the buffer at `until` the next report's first.
    fn new(track: TrackId, number: usize, report: Report, buffers: &[Raw], until: usize) -> Self {
        let reopening = reopening(buffers, report);
        Self {
            track: track.get(),
            number,
            report,
            reopening,
            before: break_before(buffers, report),
            late: break_after_wait(buffers, report, reopening, until),
        }
    }

    /// How far after the report the cycle of the buffer at `index` is, in
    /// nanoseconds; `None` if it wasn't placed.
    fn after(self, buffers: &[Raw], index: usize) -> Option<i128> {
        let placed = buffers.get(index)?.placed?;
        Some(nanos(placed.cycle) - nanos(self.report.at))
    }

    /// The buffers printed for it, of `count`: [`AROUND`] either side of
    /// the first after the report, the one that reopens, and any break
    /// before or after, in order, those that touch or overlap made one.
    fn printed(self, count: usize) -> Vec<Range<usize>> {
        let mut named = [
            Some(self.report.next),
            self.reopening.index(),
            self.before.map(|(index, _)| index),
            self.late.map(|(index, _)| index),
        ]
        .into_iter()
        .flatten()
        .map(|index| index.saturating_sub(AROUND)..index.saturating_add(AROUND + 1).min(count))
        .filter(|range| !range.is_empty())
        .collect::<Vec<_>>();
        named.sort_by_key(|range| range.start);
        let mut merged: Vec<Range<usize>> = Vec::new();
        for range in named {
            match merged.last_mut() {
                Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
                _ => merged.push(range),
            }
        }
        merged
    }
}

/// How many reports reopened each way, for the `switches` line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Tally {
    /// By the cycle alone breaking.
    cycle: usize,
    /// By the capture stamp alone breaking.
    capture: usize,
    /// By both breaking.
    both: usize,
    /// By the wait.
    wait: usize,
    /// By the wait, with a stamp breaking later.
    late: usize,
    /// By a buffer with no placed stamp.
    unstamped: usize,
    /// Not at all: the buffers ran out.
    ended: usize,
}

impl Tally {
    /// Counts `switch`.
    fn count(&mut self, switch: Switch) {
        let slot = match switch.reopening {
            Reopening::Break { jump, .. } => match jump.broke() {
                Broke::Cycle => &mut self.cycle,
                Broke::Capture => &mut self.capture,
                // A break is never neither; counted as both if it were.
                Broke::Both | Broke::Neither => &mut self.both,
            },
            Reopening::Wait { .. } => &mut self.wait,
            Reopening::Unstamped { .. } => &mut self.unstamped,
            Reopening::Ended => &mut self.ended,
        };
        *slot += 1;
        if switch.late.is_some() {
            self.late += 1;
        }
    }
}

/// Prints the stamps around each of `track`'s `reports`, and where the
/// recorder's rule reopens for each, with the epochs `epochs_for` takes for
/// it from `epochs` (its epochs after its first).
pub(super) fn print(
    out: &mut impl io::Write,
    track: TrackId,
    buffers: &[Raw],
    reports: &[Report],
    epochs: &[u64],
) -> io::Result<()> {
    let mut tally = Tally::default();
    for (k, (&report, epochs)) in reports.iter().zip(epochs_for(reports, epochs)).enumerate() {
        let until = reports.get(k + 1).map_or(buffers.len(), |next| next.next);
        let switch = Switch::new(track, k + 1, report, buffers, until);
        print_stamps(out, switch, buffers)?;
        writeln!(out, "{}", describe(switch, buffers, &epochs))?;
        tally.count(switch);
    }
    let Tally {
        cycle,
        capture,
        both,
        wait,
        late,
        unstamped,
        ended,
    } = tally;
    writeln!(
        out,
        "switches {} {}: cycle {cycle}, capture {capture}, both {both}, wait {wait} (then \
         broke {late}), unstamped {unstamped}, ended {ended}",
        track.get(),
        reports.len()
    )
}

/// Prints `switch`'s `stamp` lines, with a `skip` line for each stretch of
/// buffers left out between them.
fn print_stamps(out: &mut impl io::Write, switch: Switch, buffers: &[Raw]) -> io::Result<()> {
    let mut last: Option<usize> = None;
    for range in switch.printed(buffers.len()) {
        if let Some(end) = last {
            writeln!(
                out,
                "skip {} {} {} buffers",
                switch.track,
                switch.number,
                range.start - end
            )?;
        }
        last = Some(range.end);
        for index in range {
            print_stamp(out, switch, buffers, index)?;
        }
    }
    Ok(())
}

/// Prints the buffer at `index` around `switch`'s report: its length, its
/// stamps placed on the session clock after the report, its delay, its
/// jump from the run, and its raw stamps on `CLOCK_MONOTONIC`.
fn print_stamp(
    out: &mut impl io::Write,
    switch: Switch,
    buffers: &[Raw],
    index: usize,
) -> io::Result<()> {
    let Some(raw) = buffers.get(index) else {
        return Ok(());
    };
    let (t, k) = (switch.track, switch.number);
    let at = nanos(switch.report.at);
    let mut parts = vec![format!("stamp {t} {k} {index} len {:.3}", len_ms(raw.len))];
    parts.push(match (raw.stamp, raw.placed) {
        (Some(stamp), Some(placed)) => format!(
            "cycle {:+.3} capture {:+.3} delay {:.3}",
            signed_ms(nanos(placed.cycle) - at),
            signed_ms(nanos(placed.capture) - at),
            millis(stamp.cycle.saturating_sub(stamp.capture)),
        ),
        (Some(_), None) => "unplaced".to_owned(),
        (None, _) => "unstamped".to_owned(),
    });
    if let Some(jump) = jump(buffers, index) {
        parts.push(format!(
            "jump cycle {:+.3} capture {:+.3}",
            signed_ms(jump.cycle),
            signed_ms(jump.capture)
        ));
    }
    if let Some(stamp) = raw.stamp {
        parts.push(format!(
            "raw {} {}",
            stamp.cycle.as_nanos(),
            stamp.capture.as_nanos()
        ));
    }
    if index == switch.report.next {
        parts.push("<- first after the report".to_owned());
    }
    if switch.reopening.index() == Some(index) {
        parts.push("<- reopens".to_owned());
    }
    writeln!(out, "{}", parts.join(" "))
}

/// `switch`'s `switch` line: which stamp broke and by how much, or why
/// the recorder reopened where it did, with the break before the report
/// and the `epochs` taken for it, if any.
fn describe(switch: Switch, buffers: &[Raw], epochs: &[u64]) -> String {
    let mut parts = vec![format!(
        "switch {} {} at {:.3}: {}",
        switch.track,
        switch.number,
        millis(switch.report.at),
        reopened(switch, buffers)
    )];
    if let Some((index, jump)) = switch.before {
        parts.push(format!(
            "{} broke before the report, {}",
            jump.broke().label(),
            break_at(switch, buffers, index, jump)
        ));
    }
    let from_report = |start: &u64| {
        format!(
            "{:+.3} ms",
            signed_ms(i128::from(*start) - nanos(switch.report.at))
        )
    };
    parts.push(match epochs {
        [] => "no epoch opened before the next report".to_owned(),
        [one] => format!("epoch at {} from the report", from_report(one)),
        _ => format!(
            "epochs at {} from the report",
            epochs
                .iter()
                .map(from_report)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    });
    parts.join("; ")
}

/// Where a break came and how far it moved each stamp, in words.
fn break_at(switch: Switch, buffers: &[Raw], index: usize, jump: Jump) -> String {
    format!(
        "at buffer {index}, {} after the report: cycle {:+.3} ms, capture {:+.3} ms",
        after(switch, buffers, index),
        signed_ms(jump.cycle),
        signed_ms(jump.capture)
    )
}

/// How far after `switch`'s report the buffer at `index` came, by its
/// cycle, in words.
fn after(switch: Switch, buffers: &[Raw], index: usize) -> String {
    switch.after(buffers, index).map_or_else(
        || "unplaced".to_owned(),
        |a| format!("{:+.3} ms", signed_ms(a)),
    )
}

/// Where `switch` reopened, and why there, in words.
fn reopened(switch: Switch, buffers: &[Raw]) -> String {
    let after = |index| after(switch, buffers, index);
    match switch.reopening {
        Reopening::Break { index, jump } => format!(
            "{} broke {}",
            jump.broke().label(),
            break_at(switch, buffers, index, jump)
        ),
        Reopening::Wait { index } => {
            let then = switch.late.map_or_else(
                || format!("neither broke by {} ms", LATE.as_millis()),
                |(late, jump)| {
                    format!(
                        "then {} broke {}",
                        jump.broke().label(),
                        break_at(switch, buffers, late, jump)
                    )
                },
            );
            format!(
                "neither broke within {} ms; the wait reopened at buffer {index}, {} after the \
                 report; {then}",
                REOPEN_WAIT.as_millis(),
                after(index)
            )
        }
        Reopening::Unstamped { index } => format!(
            "buffer {index}, or the one before it, had no placed stamp, so it reopened, {} \
             after the report",
            after(index)
        ),
        Reopening::Ended => "no reopening: the buffers noted ran out first (the recording \
                             ended, or the harness's cap on buffers was reached)"
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use nota_recorder::capture::Delivery;

    use super::*;

    /// 320 samples at 16 kHz: a 20 ms buffer.
    const LEN: usize = 320;

    /// A buffer's length in microseconds.
    const STEP: u64 = 20_000;

    /// A buffer of [`LEN`] samples whose cycle is `cycle_us` and capture
    /// `capture_us` microseconds into the session, placed there.
    fn raw(cycle_us: u64, capture_us: u64) -> Raw {
        let stamp = Delivery {
            cycle: Duration::from_micros(cycle_us),
            capture: Duration::from_micros(capture_us),
        };
        Raw {
            len: LEN,
            stamp: Some(stamp),
            placed: Some(stamp),
        }
    }

    /// The cycle of the buffer at `i` in a run from 1 s, in microseconds.
    fn cycle(i: u64) -> u64 {
        1_000_000 + i * STEP
    }

    /// `count` buffers running on from a cycle at 1 s, with no delay.
    fn run(count: u64) -> Vec<Raw> {
        (0..count).map(|i| raw(cycle(i), cycle(i))).collect()
    }

    /// A report at `at_us` microseconds, before the buffer at `next`.
    fn report(at_us: u64, next: usize) -> Report {
        Report {
            at: Duration::from_micros(at_us),
            next,
        }
    }

    /// A buffer's stamps are measured from where the one before puts them,
    /// both its cycle and its capture stamp, so a delay that steps shows in
    /// the capture stamp alone.
    #[test]
    fn a_jump_is_measured_from_the_buffer_before() {
        let mut buffers = run(3);
        // The cycles run on, but the third buffer was captured 43 ms
        // earlier than the run puts it: a new link's first buffer.
        buffers[2] = raw(cycle(2), cycle(2) - 43_000);
        let found = jump(&buffers, 2);
        assert_eq!(
            found,
            Some(Jump {
                cycle: 0,
                capture: -43_000_000
            })
        );
        assert_eq!(found.map(Jump::broke), Some(Broke::Capture));
    }

    /// A stamp breaks at [`LOSS_MIN`] either way, as the recorder judges
    /// it, and not a nanosecond short of it.
    #[test]
    fn a_stamp_breaks_at_loss_min_either_way() {
        let min = i128::try_from(LOSS_MIN.as_nanos()).unwrap_or(0);
        let broke = |cycle, capture| Jump { cycle, capture }.broke();
        assert_eq!(
            [
                broke(min - 1, 1 - min),
                broke(min, 0),
                broke(0, -min),
                broke(-min, min),
            ],
            [Broke::Neither, Broke::Cycle, Broke::Capture, Broke::Both]
        );
    }

    /// The first buffer after the report whose cycle jumps reopens, and
    /// it's the cycle that broke.
    #[test]
    fn a_cycle_that_jumps_after_the_report_reopens() {
        let mut buffers = run(6);
        // A new driver: from the fifth buffer the cycles are 10 ms later,
        // and the capture stamps run on.
        for (i, buffer) in (4..).zip(&mut buffers[4..]) {
            *buffer = raw(cycle(i) + 10_000, cycle(i));
        }
        let found = reopening(&buffers, report(cycle(3) + 5_000, 4));
        assert_eq!(
            found,
            Reopening::Break {
                index: 4,
                jump: Jump {
                    cycle: 10_000_000,
                    capture: 0
                }
            }
        );
    }

    /// A run that never breaks reopens at the first buffer whose cycle is
    /// 200 ms past the report, not before.
    #[test]
    fn a_run_that_never_breaks_reopens_at_the_wait() {
        let buffers = run(20);
        // 200 ms is ten buffers: the report at buffer 2's cycle is waited
        // out at buffer 12's.
        assert_eq!(
            reopening(&buffers, report(cycle(2), 3)),
            Reopening::Wait { index: 12 }
        );
    }

    /// A buffer after the report with no stamp the clock placed reopens:
    /// there's nothing to compare it with.
    #[test]
    fn an_unplaced_buffer_after_the_report_reopens() {
        let mut buffers = run(6);
        buffers[4].placed = None;
        assert_eq!(
            reopening(&buffers, report(cycle(3), 4)),
            Reopening::Unstamped { index: 4 }
        );
    }

    /// A report whose stream ends before a break or the wait has no
    /// reopening.
    #[test]
    fn a_stream_that_ends_first_has_no_reopening() {
        let buffers = run(5);
        assert_eq!(reopening(&buffers, report(cycle(3), 4)), Reopening::Ended);
    }

    /// A break in the stamps just before the report is found: the
    /// recorder has no break left to wait for after it.
    #[test]
    fn a_break_just_before_the_report_is_found() {
        let mut buffers = run(8);
        // From the fourth buffer on, the capture stamps are 20 ms earlier.
        for (i, buffer) in (3..).zip(&mut buffers[3..]) {
            *buffer = raw(cycle(i), cycle(i) - 20_000);
        }
        let found = break_before(&buffers, report(cycle(5), 6));
        assert_eq!(
            found,
            Some((
                3,
                Jump {
                    cycle: 0,
                    capture: -20_000_000
                }
            ))
        );
    }

    /// A break more than 300 ms before the report isn't taken for the
    /// switch's.
    #[test]
    fn a_break_long_before_the_report_is_not_found() {
        let mut buffers = run(30);
        buffers[1] = raw(cycle(1) + 10_000, cycle(1) + 10_000);
        assert_eq!(break_before(&buffers, report(cycle(25), 26)), None);
        // The same break 260 ms before the report is found.
        assert_eq!(
            break_before(&buffers, report(cycle(15), 16)).map(|(index, _)| index),
            Some(2)
        );
    }

    /// Each report takes the epochs from 300 ms before it to 300 ms before
    /// the next report: an epoch long before isn't taken, one the next
    /// report opened isn't stolen, and a report that opened two shows both.
    #[test]
    fn each_report_takes_the_epochs_up_to_the_next_one() {
        let ms = |ms: u64| ms * 1_000_000;
        let reports = [report(5_000_000, 0), report(9_000_000, 0)];
        assert_eq!(
            epochs_for(&reports, &[ms(1_000), ms(4_950), ms(9_010)]),
            [vec![ms(4_950)], vec![ms(9_010)]]
        );
        assert_eq!(
            epochs_for(&reports, &[ms(4_950), ms(5_180)]),
            [vec![ms(4_950), ms(5_180)], vec![]]
        );
        // Two reports reopened for once, at 9.1 s: the first gets none, and
        // the third keeps its own.
        let three = [
            report(5_000_000, 0),
            report(9_000_000, 0),
            report(13_000_000, 0),
        ];
        assert_eq!(
            epochs_for(&three, &[ms(9_100), ms(13_010)]),
            [vec![], vec![ms(9_100)], vec![ms(13_010)]]
        );
    }

    /// The `switch` line names the stamp that broke, by how much and how
    /// long after the report, and where the epoch starts.
    #[test]
    fn the_switch_line_says_which_stamp_broke() {
        let mut buffers = run(6);
        buffers[4] = raw(cycle(4), cycle(4) - 43_000);
        let at = cycle(3) + 10_000;
        let switch = Switch::new(TrackId::new(0), 2, report(at, 4), &buffers, buffers.len());
        assert_eq!(
            describe(switch, &buffers, &[(at - 32_000) * 1_000]),
            "switch 0 2 at 1070.000: capture broke at buffer 4, +10.000 ms after the \
             report: cycle +0.000 ms, capture -43.000 ms; epoch at -32.000 ms from the report"
        );
    }

    /// A switch whose stamps broke before the report, and that the wait
    /// reopened for, says both, and that nothing broke after the wait.
    #[test]
    fn the_switch_line_says_when_the_break_came_first() {
        let mut buffers = run(20);
        for (i, buffer) in (3..).zip(&mut buffers[3..]) {
            *buffer = raw(cycle(i) + 6_000, cycle(i) + 6_000);
        }
        let switch = Switch::new(TrackId::new(1), 1, report(cycle(4), 5), &buffers, 20);
        assert_eq!(
            describe(switch, &buffers, &[]),
            "switch 1 1 at 1080.000: neither broke within 200 ms; the wait reopened at \
             buffer 14, +206.000 ms after the report; neither broke by 1000 ms; both broke \
             before the report, at buffer 3, -14.000 ms after the report: cycle +6.000 ms, \
             capture +6.000 ms; no epoch opened before the next report"
        );
    }

    /// A stamp that breaks after the wait reopened is found and named, as
    /// the new device's first audio then sits in the old device's epoch.
    #[test]
    fn a_break_after_the_wait_is_named() {
        let mut buffers = run(30);
        // The new driver's cycles, 7 ms on, from 360 ms after the report.
        for (i, buffer) in (20..).zip(&mut buffers[20..]) {
            *buffer = raw(cycle(i) + 7_000, cycle(i));
        }
        let switch = Switch::new(TrackId::new(0), 1, report(cycle(2), 3), &buffers, 30);
        assert!(
            describe(switch, &buffers, &[]).contains(
                "the wait reopened at buffer 12, +200.000 ms after the report; then cycle \
                 broke at buffer 20, +367.000 ms after the report: cycle +7.000 ms, capture \
                 +0.000 ms"
            ),
            "{}",
            describe(switch, &buffers, &[])
        );
        // Past the next report's first buffer, the break is that report's.
        let cut = Switch::new(TrackId::new(0), 1, report(cycle(2), 3), &buffers, 20);
        assert_eq!(cut.late, None);
    }

    /// The `stamp` lines run from before the report to after the buffer
    /// that reopens, and mark both.
    #[test]
    fn the_stamp_lines_mark_the_report_and_the_reopening() {
        let mut buffers = run(12);
        buffers[6] = raw(cycle(6), cycle(6) - 43_000);
        let text = printed(&buffers, &[report(cycle(5), 6)]);
        let marked: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with("stamp"))
            .map(|line| line.split(" <- ").nth(1).unwrap_or(""))
            .collect();
        // Four before the first after the report, it, and four after.
        assert_eq!(
            marked,
            ["", "", "", "", "first after the report", "", "", "", ""]
        );
        assert!(
            text.contains(
                "stamp 0 1 6 len 20.000 cycle +20.000 capture -23.000 delay 43.000 jump \
                 cycle +0.000 capture -43.000 raw 1120000000 1077000000 <- first after the \
                 report <- reopens"
            ),
            "{text}"
        );
    }

    /// Buffers far from any named one are left out, and a `skip` line says
    /// how many, so the report, the reopening and a late break all show
    /// whatever the quantum.
    #[test]
    fn far_apart_buffers_are_skipped_with_a_count() {
        let mut buffers = run(40);
        for (i, buffer) in (30..).zip(&mut buffers[30..]) {
            *buffer = raw(cycle(i) + 7_000, cycle(i));
        }
        let text = printed(&buffers, &[report(cycle(2), 3)]);
        let kinds: Vec<&str> = text
            .lines()
            .filter_map(|line| line.split(' ').next())
            .collect();
        // Buffers 0 to 16 (the report's and the wait's at 12), nine left
        // out, then 26 to 34 around the late break at 30.
        let mut expected = vec!["stamp"; 17];
        expected.push("skip");
        expected.extend(["stamp"; 9]);
        expected.extend(["switch", "switches"]);
        assert_eq!(kinds, expected);
        assert!(text.contains("skip 0 1 9 buffers"), "{text}");
    }

    /// The `switches` line counts each kind of reopening.
    #[test]
    fn the_switches_line_counts_each_kind() {
        let mut buffers = run(40);
        buffers[6] = raw(cycle(6), cycle(6) - 43_000);
        let text = printed(&buffers, &[report(cycle(5), 6), report(cycle(10), 11)]);
        assert_eq!(
            text.lines().last(),
            Some(
                "switches 0 2: cycle 0, capture 1, both 0, wait 1 (then broke 0), unstamped \
                 0, ended 0"
            )
        );
    }

    /// Every kind of reopening goes to its own count, and a late break is
    /// counted on top of its wait.
    #[test]
    fn the_tally_keeps_each_kind_apart() {
        let jump = |cycle, capture| Jump { cycle, capture };
        let switch = |reopening, late| Switch {
            track: 0,
            number: 1,
            report: report(0, 0),
            reopening,
            before: None,
            late,
        };
        let mut tally = Tally::default();
        for (reopening, late) in [
            (
                Reopening::Break {
                    index: 1,
                    jump: jump(5_000_000, 0),
                },
                None,
            ),
            (
                Reopening::Break {
                    index: 1,
                    jump: jump(0, 5_000_000),
                },
                None,
            ),
            (
                Reopening::Break {
                    index: 1,
                    jump: jump(5_000_000, 5_000_000),
                },
                None,
            ),
            (
                Reopening::Break {
                    index: 1,
                    jump: jump(5_000_000, 0),
                },
                None,
            ),
            (Reopening::Wait { index: 1 }, Some((2, jump(5_000_000, 0)))),
            (Reopening::Wait { index: 1 }, None),
            (Reopening::Wait { index: 1 }, None),
            (Reopening::Unstamped { index: 1 }, None),
            (Reopening::Ended, None),
            (Reopening::Ended, None),
        ] {
            tally.count(switch(reopening, late));
        }
        assert_eq!(
            tally,
            Tally {
                cycle: 2,
                capture: 1,
                both: 1,
                wait: 3,
                late: 1,
                unstamped: 1,
                ended: 2,
            }
        );
    }

    /// What [`print`] writes for `reports` over `buffers`, with no epochs.
    fn printed(buffers: &[Raw], reports: &[Report]) -> String {
        let mut out = Vec::new();
        let result = print(&mut out, TrackId::new(0), buffers, reports, &[]);
        assert!(result.is_ok(), "{result:?}");
        String::from_utf8(out).unwrap_or_default()
    }
}
