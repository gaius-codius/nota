#!/usr/bin/env python3
"""Measures a soak run's session (see scripts/soak.sh, which calls this).

    soak-analyse.py WORKDIR

WORKDIR holds what soak.sh left: run.env (the conditions), samples.tsv
(the sampled memory and load), and data/ (the library and the session). The
speech that was played has a 40 ms burst of a 6 kHz tone every 10 s from its
first sample. Each track's audio is decoded from the session's FLAC segments,
each burst's first sample found, and the epochs in the library map it to
session time. Stdlib only. Prints the report, writes it to WORKDIR/report.txt
and the bursts to WORKDIR/bursts-<track>.tsv, and exits 1 if a criterion
fails.
"""

import array
import bisect
import cmath
import itertools
import math
import os
import re
import sqlite3
import statistics
import subprocess
import sys

RATE = 16000
# The bursts: 6 kHz is three eighths of the sample rate, so the carrier
# repeats every eight samples.
PERIOD_S = 10
WINDOW = 64
CARRIER = [cmath.exp(-2j * math.pi * 3 * i / 8) for i in range(8)]
# A burst's 0.3 full-scale tone gives the sliding sum about this size; a
# burst is any peak above half of it.
PLATEAU = 0.3 * 32768 * WINDOW / 2
# Where to look for the next burst: this far either side of where the last
# one puts it, widening only when it isn't there.
RADII = (RATE // 2, 3 * RATE, 8 * RATE)

MEMORY_LIMIT_MB = 50
SETTLED_AFTER_S = 600
DRIFT_LIMIT_MS = 20
# Beyond ordinary jitter in a burst's measured place.
STEP_MS = 5
# A gap the timeline names is matched to its epochs within this.
MATCH_NS = 50_000_000
TAIL_LIMIT_S = 0.5
# Silence the source never has (it has a noise floor throughout): a run of
# exactly zero samples this long inside the played span is audio that
# wasn't captured, whatever the epochs say.
ZERO_RUN_S = 0.02
ALIGNMENT_LIMIT_MS = 20


class Report:
    def __init__(self):
        self.lines = []
        self.failed = False

    def say(self, line=""):
        self.lines.append(line)
        print(line)

    def check(self, ok, line):
        if not ok:
            self.failed = True
        self.say(f"{line} {'pass' if ok else 'FAIL'}")


def read_env(work):
    env = {}
    with open(os.path.join(work, "run.env")) as f:
        for line in f:
            key, _, value = line.strip().partition("=")
            env[key] = value
    return env


def read_samples(work):
    with open(os.path.join(work, "samples.tsv")) as f:
        header = f.readline().strip().split("\t")
        return [dict(zip(header, map(float, line.split("\t")))) for line in f if line.strip()]


def memory(report, samples):
    """Growth after the first ten minutes: the highest later reading, and
    the last ten minutes' median, against the median of the minute from ten
    minutes in."""
    report.say("== memory (resident, MB)")
    kinds = (
        ("recorder", lambda s: s["nota_rss_kb"]),
        ("engine", lambda s: s["engine_rss_kb"]),
        ("both", lambda s: s["nota_rss_kb"] + s["engine_rss_kb"]),
    )
    end = samples[-1]["elapsed_s"]
    base_window = [s for s in samples if SETTLED_AFTER_S <= s["elapsed_s"] < SETTLED_AFTER_S + 60]
    later = [s for s in samples if s["elapsed_s"] >= SETTLED_AFTER_S]
    if not base_window or len(later) < 2:
        report.check(False, f"run of {end:.0f} s is too short to judge growth after {SETTLED_AFTER_S} s")
        return
    last = [s for s in later if s["elapsed_s"] >= end - 600]
    for name, of in kinds:
        base = statistics.median(of(s) for s in base_window) / 1024
        peak = max(of(s) for s in later) / 1024
        final = statistics.median(of(s) for s in last) / 1024
        early = max(of(s) for s in samples if s["elapsed_s"] < SETTLED_AFTER_S) / 1024
        xs = [s["elapsed_s"] / 3600 for s in later]
        ys = [of(s) / 1024 for s in later]
        slope = least_squares_slope(xs, ys)
        line = (
            f"{name}: at 10 min {base:.1f}, highest after {peak:.1f} (+{peak - base:.1f}), "
            f"last 10 min median {final:.1f} (+{final - base:.1f}), "
            f"slope {slope:+.2f} MB/h, highest in the first 10 min {early:.1f}"
        )
        if name == "both":
            report.check(peak - base < MEMORY_LIMIT_MB, f"{line}; growth under {MEMORY_LIMIT_MB} MB:")
        else:
            report.say(line)
    hwm = max(s["nota_hwm_kb"] for s in samples) / 1024
    report.say(f"recorder peak resident (VmHWM) {hwm:.1f} MB; engines seen: "
               f"{sorted({int(s['engines']) for s in samples})}")
    cpu = samples[-1]["nota_cpu_ticks"] - samples[0]["nota_cpu_ticks"]
    span = samples[-1]["elapsed_s"] - samples[0]["elapsed_s"]
    if span > 0:
        report.say(f"recorder CPU {cpu / os.sysconf('SC_CLK_TCK') / span * 100:.1f}% of one core "
                   f"on average, {int(max(s['threads'] for s in samples))} threads at most")
    loads = [s["load1"] for s in samples]
    report.say(f"load average (1 min): median {statistics.median(loads):.1f}, "
               f"highest {max(loads):.1f}; memory available at the lowest "
               f"{min(s['mem_avail_kb'] for s in samples) / 1048576:.1f} GB; "
               f"session on disk {samples[-1]['session_kb'] / 1024:.0f} MB")
    report.check(all(s["default_sink_changed"] == 0 for s in samples),
                 "the default sink was unchanged at every sample:")
    report.say("")
    report.say("memory by 10-minute block (median of both, MB)")
    blocks = {}
    for s in samples:
        blocks.setdefault(int(s["elapsed_s"] // 600), []).append(s["nota_rss_kb"] + s["engine_rss_kb"])
    report.say("  " + "  ".join(f"{b * 10}m:{statistics.median(v) / 1024:.0f}" for b, v in sorted(blocks.items())))


def least_squares_slope(xs, ys):
    n = len(xs)
    if n < 2:
        return 0.0
    mx, my = sum(xs) / n, sum(ys) / n
    den = sum((x - mx) ** 2 for x in xs)
    return sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / den if den else 0.0


class Epochs:
    """A track's epochs: a sample's session time is the epoch's start plus
    the samples since its first, at its rate times one plus its drift."""

    def __init__(self, rows):
        self.rows = sorted(rows)  # (first_sample, number, rate, anchor_ns, drift_ppb)
        self.firsts = [r[0] for r in self.rows]

    def row_of(self, sample):
        return self.rows[max(bisect.bisect_right(self.firsts, sample) - 1, 0)]

    def time(self, sample, corrected=True):
        first, _, rate, anchor_ns, drift_ppb = self.row_of(sample)
        factor = 1 + drift_ppb / 1e9 if corrected else 1
        return anchor_ns / 1e9 + (sample - first) / (rate * factor)

    def end_of(self, i):
        """When epoch `i`'s audio ends by its own mapping, which is where
        the next one's first sample would play without a gap."""
        first, _, rate, anchor_ns, drift_ppb = self.rows[i]
        nxt = self.rows[i + 1][0]
        return anchor_ns / 1e9 + (nxt - first) / (rate * (1 + drift_ppb / 1e9))


def load_audio(work, session, track, segments):
    """The track's samples, in order, decoded from its FLAC segments."""
    samples = array.array("h")
    expected = 0
    for start, end in sorted(segments):
        path = os.path.join(work, "data", "sessions", str(session), "audio", f"seg-t{track}-{start:012d}.flac")
        if not os.path.isfile(path):
            raise SystemExit(f"soak-analyse: no segment for track {track} from sample {start}")
        raw = subprocess.run(
            ["ffmpeg", "-nostdin", "-loglevel", "error", "-i", path, "-f", "s16le",
             "-ar", str(RATE), "-ac", "1", "-"],
            check=True, capture_output=True).stdout
        part = array.array("h")
        part.frombytes(raw)
        if sys.byteorder == "big":
            part.byteswap()
        if start != expected or len(part) != end - start:
            raise SystemExit(f"soak-analyse: track {track}'s segment from {start} holds {len(part)} "
                             f"samples, the library says {start}..{end} (expected from {expected})")
        samples.extend(part)
        expected = end
    return samples


def find_burst(x, lo, hi):
    """The first sample of the burst whose tone lies in x[lo:hi], or None.
    The sliding sum of the signal against the carrier rises over WINDOW
    samples; the burst starts half a window before it passes half its peak."""
    lo, hi = max(lo, 0), min(hi, len(x))
    if hi - lo < 2 * WINDOW:
        return None
    z = [x[i] * CARRIER[i & 7] for i in range(lo, hi)]
    total = [0j, *itertools.accumulate(z)]
    mags = [abs(total[i] - total[i - WINDOW]) for i in range(WINDOW, len(total))]
    peak = max(mags)
    if peak < PLATEAU / 2:
        return None
    first = next(i for i, m in enumerate(mags) if m >= peak / 2)
    return lo + first + WINDOW - WINDOW // 2


def find_bursts(x, count):
    """The first sample of each burst found, in order, looking for `count`
    (the bursts played), a period apart from the last found. Which burst
    each is comes from its session time (see `label`)."""
    first = find_burst(x, 0, min(len(x), 60 * RATE))
    if first is None:
        return []
    found = [first]
    k, last_k, last = 1, 0, first
    while k < count:
        pred = last + (k - last_k) * PERIOD_S * RATE
        if pred - RADII[0] >= len(x) - 2 * WINDOW:
            break
        for r in RADII:
            hit = find_burst(x, pred - r, pred + r)
            if hit is not None:
                found.append(hit)
                last_k, last = k, hit
                break
        k += 1
    return found


def tracks_of(db, session):
    rows = db.execute("SELECT track, kind, source FROM track WHERE session_id = ? ORDER BY track",
                      (session,)).fetchall()
    return rows


def analyse_track(report, work, db, session, track, env, outputs):
    name = {0: "mic", 1: "system"}.get(track, str(track))
    report.say(f"== track {track} ({name})")
    segments = db.execute("SELECT start_sample, end_sample FROM segment WHERE session_id = ? AND track = ?",
                          (session, track)).fetchall()
    epoch_rows = [(f, e, r, a, d) for e, f, r, a, d in db.execute(
        "SELECT epoch, first_sample, rate, anchor_ns, drift_ppb FROM epoch "
        "WHERE session_id = ? AND track = ?", (session, track))]
    if not segments or not epoch_rows:
        report.check(False, "no audio or no epochs published:")
        return None
    x = load_audio(work, session, track, segments)
    epochs = Epochs(epoch_rows)
    end_t = epochs.time(len(x))
    report.say(f"audio: {len(x)} samples = {len(x) / RATE:.1f} s, {len(segments)} segments, "
               f"session time {epochs.time(0):.3f} s to {end_t:.3f} s "
               f"(spans {end_t - epochs.time(0):.1f} s)")

    # Epochs and gaps.
    report.say(f"epochs: {len(epochs.rows)}; drift (ppb) {sorted({r[4] for r in epochs.rows})}")
    events = db.execute("SELECT at_ns, kind, detail FROM event WHERE session_id = ? AND track = ? "
                        "ORDER BY at_ns", (session, track)).fetchall()
    gap_events = [(at, int(detail)) for at, kind, detail in events if kind == "gap"]
    implied = []
    for i in range(len(epochs.rows) - 1):
        gap = epochs.rows[i + 1][3] / 1e9 - epochs.end_of(i)
        if abs(gap) > 0.001:
            implied.append((epochs.end_of(i), epochs.rows[i + 1][3] / 1e9, gap))
    unlogged_gaps = []
    logged = []
    unused = list(gap_events)
    for start, until, gap in implied:
        # One event for one gap, naming both its ends.
        match = next((e for e in unused if abs(e[0] - start * 1e9) <= MATCH_NS
                      and abs(e[1] - until * 1e9) <= MATCH_NS), None)
        if match is not None:
            unused.remove(match)
            logged.append((start, until))
        report.say(f"  gap at {start:.3f} s of {gap * 1000:+.1f} ms: "
                   f"{'on the timeline' if match else 'NOT on the timeline'}")
        if match is None:
            unlogged_gaps.append((start, gap))
    report.check(not unlogged_gaps and not unused,
                 f"gaps implied by the epochs: {len(implied)}, timeline gap events: {len(gap_events)} "
                 f"({len(unused)} matching no gap); every gap logged, once:")
    kinds = {}
    for _, kind, _ in events:
        kinds[kind] = kinds.get(kind, 0) + 1
    report.say(f"timeline events on this track: {kinds or 'none'}")

    # Bursts. Each is labelled by its place in session time, so a gap, or a
    # first burst the recording missed, doesn't shift the labels after it.
    played = round(float(env["minutes"]) * 6)
    offset = float(env.get("burst_offset_s", 0))
    found = find_bursts(x, played)
    if not found:
        report.check(False, "no burst found in the audio:")
        return None
    guess = float(env.get("play_started_after_s", 0)) + offset - 0.5
    bursts = [(round((epochs.time(sample) - guess) / PERIOD_S), sample) for sample in found]
    labels = [k for k, _ in bursts]
    if labels != sorted(set(labels)) or labels[0] < 0 or labels[-1] >= played:
        report.check(False, f"bursts can't be told apart by their place in session time: labels {labels[:6]}...:")
        return None
    k_last = labels[-1]
    reference = statistics.median(epochs.time(sample) - k * PERIOD_S for k, sample in bursts)
    nominal = statistics.median(epochs.time(sample, corrected=False) - k * PERIOD_S for k, sample in bursts)
    # A missing burst is excused only where a logged gap covers its place.
    absent = [k for k in range(played) if k not in set(labels)]
    covered = [k for k in absent
               if any(start - 0.05 <= reference + k * PERIOD_S <= until + 0.05 for start, until in logged)]
    uncovered = [k for k in absent if k not in covered]
    report.check(not uncovered and labels[0] == 0 and k_last == played - 1,
                 f"bursts: {len(bursts)} found of {played} played; missing {len(absent)} "
                 f"({len(covered)} inside a logged gap, the rest: {uncovered[:6]}); the first found is index "
                 f"{labels[0]}, the last {k_last}; none missing outside a logged gap:")
    rows = []
    for k, sample in bursts:
        t = epochs.time(sample)
        dev = (t - reference - k * PERIOD_S) * 1000
        dev_nominal = (epochs.time(sample, corrected=False) - nominal - k * PERIOD_S) * 1000
        rows.append((k, sample, t, dev, dev_nominal))
    with open(os.path.join(work, f"bursts-{track}.tsv"), "w") as f:
        f.write("k\tsample\tsession_s\tdev_ms\tdev_without_drift_ms\n")
        for k, sample, t, dev, dev_nominal in rows:
            f.write(f"{k}\t{sample}\t{t:.6f}\t{dev:.3f}\t{dev_nominal:.3f}\n")
    devs = [r[3] for r in rows]
    start = epochs.time(0)
    after = [r[3] for r in rows if r[2] - start >= 30] or devs
    spread = max(devs) - min(devs)
    report.check(spread < DRIFT_LIMIT_MS,
                 f"drift after correction: bursts' place against a steady 10 s varies by {spread:.2f} ms "
                 f"({min(devs):+.2f} to {max(devs):+.2f}) over the run, under {DRIFT_LIMIT_MS} ms:")
    slope = least_squares_slope([r[2] for r in rows], devs) * 1000
    report.say(f"  after the first 30 s: {max(after) - min(after):.2f} ms; trend {slope:+.3f} ppm; without the "
               f"epochs' drift the spread would be {max(r[4] for r in rows) - min(r[4] for r in rows):.2f} ms")
    retimes = sum(1 for i in range(len(epochs.rows) - 1)
                  if epochs.rows[i][4] != epochs.rows[i + 1][4] and abs(epochs.rows[i + 1][3] / 1e9 - epochs.end_of(i)) <= 0.001)
    if not implied and not retimes:
        report.say("  note: no gap and no drift correction happened in this track, so the drift and gap checks "
                   "had nothing to catch (null sinks run on the system's clock, the session clock's own)")
    # Any step between neighbouring bursts: a logged gap sized right leaves
    # none, so one across an epoch boundary is a gap logged too small, or a
    # loss absorbed into a drift correction.
    steps = []
    for (k1, s1, _, d1, _), (k2, s2, _, d2, _) in zip(rows, rows[1:]):
        if abs(d2 - d1) > STEP_MS:
            crossed = any(s1 < f <= s2 for f in epochs.firsts[1:])
            steps.append((k2, round(d2 - d1, 1), "across an epoch boundary" if crossed else "no epoch boundary"))
    report.check(not steps, f"steps over {STEP_MS} ms between neighbouring bursts: {len(steps)}"
                            + (f" {steps[:5]}" if steps else "") + "; none:")

    # How much of the end is missing: the audio after the last burst, in
    # session time, against the time from it to the stop.
    if "play_to_stop_s" in env:
        tail_have = end_t - epochs.time(bursts[-1][1])
        tail_should = float(env["play_to_stop_s"]) - offset - k_last * PERIOD_S
        report.check(tail_should - tail_have < TAIL_LIMIT_S,
                     f"end of the audio: {tail_have:.2f} s of session time after the last burst; the stop came "
                     f"{tail_should:.2f} s after it ({tail_should - tail_have:+.2f} s short), under {TAIL_LIMIT_S} s:")
    else:
        report.say("end of the audio: not checked (the run didn't get as far as the stop)")
    zero_runs(report, x, epochs, events, bursts[0][1], bursts[-1][1] + round(9.5 * RATE))
    # And the speech: texts the engine returned.
    count, first_at, last_at = db.execute(
        "SELECT count(*), min(start_ns), max(end_ns) FROM utterance WHERE session_id = ? AND track = ?",
        (session, track)).fetchone()
    report.say(f"live text: {count} utterances"
               + (f", from {first_at / 1e9:.1f} s to {last_at / 1e9:.1f} s" if count else ""))
    report.say("")
    outputs[track] = {r[0]: r[2] for r in rows}
    return epochs


def zero_runs(report, x, epochs, events, lo, hi):
    """Runs of exactly zero samples inside the played span [lo, hi): the
    source has a noise floor throughout, so each is audio that wasn't
    captured. Each is named with whether a digital-zeros warning covers it
    on the timeline."""
    raw = x[lo:hi].tobytes() if sys.byteorder == "little" else None
    if raw is None:
        report.check(False, "zero runs can only be checked on a little-endian host:")
        return
    minimum = round(ZERO_RUN_S * RATE) * 2
    runs = [(lo + m.start() // 2, (m.end() - m.start()) // 2)
            for m in re.finditer(b"\x00{%d,}" % minimum, raw)]
    raised = [at for at, kind, _ in events if kind == "raised:digital-zeros"]
    cleared = [at for at, kind, _ in events if kind == "cleared:digital-zeros"]
    unlogged = []
    for first, length in runs:
        t = epochs.time(first)
        covered = any(r / 1e9 <= t + 0.1 and (not [c for c in cleared if c > r] or
                                              min(c for c in cleared if c > r) / 1e9 >= t)
                      for r in raised)
        report.say(f"  {length / RATE * 1000:.0f} ms of exact zeros at {t:.3f} s: "
                   f"{'a digital-zeros warning covers it' if covered else 'NOT on the timeline'}")
        if not covered:
            unlogged.append((t, length))
    report.check(not unlogged, f"runs of {ZERO_RUN_S * 1000:.0f} ms or more of exact zeros inside the "
                               f"played span: {len(runs)}, unlogged {len(unlogged)}; none unlogged:")


def alignment(report, bursts):
    """The same burst on the two tracks."""
    report.say("== cross-track alignment (system minus mic, same burst, ms)")
    a, b = bursts[0], bursts[1]
    common = sorted(set(a) & set(b))
    if len(common) < 2:
        report.check(False, "fewer than two bursts on both tracks:")
        return
    diffs = [(b[k] - a[k]) * 1000 for k in common]
    head = statistics.mean(diffs[:10])
    tail = statistics.mean(diffs[-10:])
    report.say(f"{len(common)} bursts on both: mean {statistics.mean(diffs):+.2f}, "
               f"min {min(diffs):+.2f}, max {max(diffs):+.2f}, spread {max(diffs) - min(diffs):.2f}; "
               f"first ten {head:+.2f}, last ten {tail:+.2f} (moved {tail - head:+.2f})")
    report.say(f"trend {least_squares_slope([float(k) * PERIOD_S for k in common], diffs) * 1000:+.3f} ppm")
    report.check(max(diffs) - min(diffs) < ALIGNMENT_LIMIT_MS,
                 f"the offset between the tracks varies by {max(diffs) - min(diffs):.2f} ms over the run, "
                 f"under {ALIGNMENT_LIMIT_MS} ms (its constant part, {statistics.mean(diffs):+.2f} ms, "
                 f"is reported, not judged):")
    report.say("both tracks heard the same samples at the same moment (one player linked to both "
               "sinks), so this is nota's own: the capture stamps and the mapping to session time")
    report.say("")


def main(work):
    env = read_env(work)
    report = Report()
    report.say(f"soak: {env['tracks']} track(s), {env['minutes']} min played, {env['host']}, "
               f"{env['cpus']} CPUs, nota exit {env.get('nota_exit', '?')}, "
               f"stopping took {env.get('stop_took_s', '?')} s")
    report.check(env.get("nota_exit") == "0", "nota exited 0 after the SIGHUP (everything published):")
    report.say("")
    memory(report, read_samples(work))
    report.say("")
    db = sqlite3.connect(f"file:{os.path.join(work, 'data', 'library.db')}?mode=ro", uri=True)
    session = db.execute("SELECT max(id) FROM session").fetchone()[0]
    state = db.execute("SELECT state FROM session WHERE id = ?", (session,)).fetchone()[0]
    tracks = [t[0] for t in tracks_of(db, session)]
    report.check(len(tracks) == int(env["tracks"]), f"session {session} ({state}) has {len(tracks)} track(s):")
    other = [e for e in db.execute("SELECT at_ns, track, kind, detail FROM event WHERE session_id = ? "
                                   "AND track IS NULL ORDER BY at_ns", (session,))]
    report.say(f"session-wide timeline events: {[(round(a / 1e9, 1), k) for a, _, k, _ in other] or 'none'}")
    report.say("")
    outputs = {}
    for track in tracks:
        analyse_track(report, work, db, session, track, env, outputs)
    if len(outputs) == 2:
        alignment(report, [outputs[t] for t in sorted(outputs)])
    report.say("RESULT " + ("FAIL" if report.failed else "pass"))
    with open(os.path.join(work, "report.txt"), "w") as f:
        f.write("\n".join(report.lines) + "\n")
    return 1 if report.failed else 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    sys.exit(main(sys.argv[1]))
