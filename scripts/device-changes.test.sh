#!/usr/bin/env bash
# Tests device-changes.py's parts that need no PipeWire: the terminal
# emulator that rebuilds nota's screen from the cells it redraws, the reading
# of the top border's warnings, the tone classifier that says which device an
# epoch recorded, and the matching of a timeline event to a change. Each
# check fails on the wrong answer (a warning missed, a tone mistaken, an
# event more than 2 s late counted), so the runs against PipeWire have
# something to trust.
#
# Requires: bash, python3.
set -euo pipefail

scripts=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT

# -I: nothing from the current or the script's directory is imported; -B:
# no bytecode is written beside the driver.
python3 -I -B - "$scripts/device-changes.py" "$dir" <<'PY'
import array
import importlib.util
import math
import os
import sqlite3
import sys

spec = importlib.util.spec_from_file_location("device_changes", sys.argv[1])
dc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(dc)
work = sys.argv[2]
failures = []


def expect(name, ok):
    if not ok:
        failures.append(name)


# --- the screen: only changed cells are written, so a warning is never in
# one piece in the output.
s = dc.Screen(5, 40)
s.feed("\x1b[2J\x1b[1;1H╭─ nota ─────── ● REC 00:00:00 ─╮".encode())
expect(f"a drawn border wasn't read back: {s.line(0)!r}", s.line(0) == "╭─ nota ─────── ● REC 00:00:00 ─╮")
# The cells of "● REC" become "⚠ mic lost" with the unchanged ones skipped:
# moves between the runs that differ, and a repeat for the dashes.
s.feed("\x1b[1;17H\x1b[1m⚠ mic lost\x1b[0m".encode())
expect(f"a cursor move and a styled print weren't followed: {s.line(0)!r}", "⚠ mic lost" in s.line(0))
s.feed("\x1b[2;3Hab\x1b[3;1H-\x1b[3;2H\x1b[5b".encode())
expect(f"a repeat wasn't followed: {s.line(2)!r}", s.line(2) == "------")
s.feed("\x1b[2;1H\x1b[K".encode())
expect(f"an erase to the end of the line left text: {s.line(1)!r}", s.line(1) == "")
before = s.line(0)
s.feed("\x1b[1;3H\x1b[4P".encode())
expect(f"a delete of 4 characters wasn't followed: {s.line(0)!r}", s.line(0) == before[:2] + before[6:])
s.feed("\x1b[1;3H\x1b[2@".encode())
expect(f"an insert of 2 characters wasn't followed: {s.line(0)!r}", s.line(0).startswith("╭─  ") and s.line(0)[4:].startswith(before[6:12]))
s2 = dc.Screen(2, 10)
wide = "a\u4e2dx".encode()
s2.feed(wide[:2])  # a wide character split across two reads
s2.feed(wide[2:])
expect(f"a character split across reads, or a wide one, was wrong: {s2.line(0)!r}", s2.line(0) == "a\u4e2dx")
expect(f"a wide character took one cell: the cursor is at {s2.c}", s2.c == 4)

# --- the warnings, as the top border shows them.
top = "╭─ ≈ nota · x ───────────────── ⚠ mic lost +1 · 00:00:04 ─╮  ╭─ marks & notes ─╮"
expect(f"a lost mic wasn't read: {dc.warning(top)}", dc.warning(top) == ("lost", "mic", ""))
top = "╭─ ≈ nota · x ───────────────── ↪ system: ndc_probe_b Audio/Sink sink +1 · 00:00:08 ─╮"
expect(f"a moved system wasn't read (its `+1` is the border's): {dc.warning(top)}", dc.warning(top) == ("changed", "system", "ndc_probe_b Audio/Sink sink"))
top = "╭─ ≈ nota · x ───────────────── ↪ system: ndc_probe_a ─╮"
expect(f"a moved system's name wasn't cut at the border: {dc.warning(top)}", dc.warning(top) == ("changed", "system", "ndc_probe_a"))
for quiet in ("╭─ ≈ nota · x ──────── ● REC 00:00:04 ─╮", "╭─ ⚠ mic not recording · 00:00:04 ─╮", ""):
    expect(f"{quiet!r} was read as a device warning: {dc.warning(quiet)}", dc.warning(quiet) is None)

# --- tones: which device an epoch recorded.
tones = {"a": 500, "b": 1500}


def tone(freq, n=dc.WINDOW, amp=8000):
    return [int(amp * math.sin(2 * math.pi * freq * i / dc.RATE)) for i in range(n)]


expect("a 500 Hz window wasn't a", dc.classify(tone(500), tones) == "a")
expect("a 1500 Hz window wasn't b", dc.classify(tone(1500), tones) == "b")
expect("a quiet 500 Hz window (a real device's level) wasn't a", dc.classify(tone(500, amp=1000), tones) == "a")
expect("zeros weren't silence", dc.classify([0] * dc.WINDOW, tones) == "silence")
mix = [x + y for x, y in zip(tone(500), tone(1500))]
expect("both tones at once weren't other (a stream on the wrong sink hears both)", dc.classify(mix, tones) == "other")

# Two epochs of 2 s: a, then b, the second opening with a 40 ms of zeros and
# then 20 ms of the old tone (a late buffer of the old device).
first = tone(500, 2 * dc.RATE)
second = [0] * (dc.WINDOW * 2) + tone(500, dc.WINDOW) + tone(1500, 2 * dc.RATE - dc.WINDOW * 3)
samples = array.array("h", first + second)
summary = dc.epoch_tones(samples, [0, 2 * dc.RATE], tones)
expect(f"the epochs' tones were {[x[0] for x in summary]}", [x[0] for x in summary] == ["a", "b"])
expect(f"pure epochs weren't pure: {[x[3] for x in summary]}", all(x[3] > 0.99 for x in summary))

# An epoch of 1 s of the old device's tone and then 2 s of the new one has the
# new one for its majority but isn't one device's audio.
mixed = array.array("h", tone(1500, 3 * dc.RATE)[: 0] + tone(500, dc.RATE) + tone(1500, 2 * dc.RATE))
(only,) = dc.epoch_tones(mixed, [0], tones)
expect(f"a third of an epoch from the other device wasn't seen: {only[0]} {only[3]:.2f}",
       only[0] == "b" and only[3] < dc.PURITY)
class Quiet:
    failed = []
    said = []
    def check(self, ok, text):
        self.said.append((ok, text))
    def say(self, text=""):
        pass
quiet = Quiet()
dc.tone_checks(quiet, mixed, [(0, 0, 0)], tones, [["b"]], "x")
expect(f"a mixed epoch passed the sequence check: {quiet.said}", quiet.said and quiet.said[-1][0] is False and "mixed(b)" in quiet.said[-1][1])
quiet.said.clear()
dc.tone_checks(quiet, samples, [(0, 0, 0), (1, 2 * dc.RATE, 0)], tones, [["a", "b"]], "x")
expect(f"a right sequence failed: {quiet.said}", quiet.said and quiet.said[-1][0] is True)
expect(f"the second epoch's opening was {summary[1][1]}",
       summary[1][1][:4] == ["silence", "silence", "a", "b"])
expect("merged didn't drop repeats", dc.merged(["a", "a", "b", "b", "a"]) == ["a", "b", "a"])

# --- the screen's name and the timeline's, and the events of one track.
expect("a name cut short by the border wasn't the device", dc.same_device("ndc_probe_b Audio", "ndc_probe_b Audio/Sink sink"))
expect("another device was taken for the same one", not dc.same_device("ndc_probe_a", "ndc_probe_b Audio/Sink sink"))
expect("an empty name was taken for a device", not dc.same_device("", "x") and not dc.same_device("x", None))
expect("a name inside the logged one counted both ways", dc.names_it("USB Audio Analog", "analog stereo") is False
       and dc.names_it("usb audio analog stereo", "Analog Stereo") is True)
expect("a detail inside the expected name counted as naming it", dc.names_it("analog", "analog stereo") is False)
expect("an empty detail named a device", dc.names_it("", "x") is False)
evs = [(1, 1, "device:changed", "b"), (2, 0, "device:lost", None), (3, 1, "device:lost", None), (4, 1, "gap", "9")]
expect("the last device event of a track wasn't found", dc.last_device_kind(evs, 1) == "device:lost" and dc.last_device_kind(evs, 0) == "device:lost"
       and dc.last_device_kind(evs, 2) is None)

# What the screen said is an event of the same kind, on the track, for the
# same device, near the change.
evs = [(5_000_000_000, 1, "device:changed", "sink b"), (5_010_000_000, 1, "device:changed", "sink c"),
       (5_000_000_000, 0, "device:lost", None), (50_000_000_000, 1, "device:changed", "sink b")]
said = dc.events_saying(evs, ("changed", "system", "sink c"), 1, 105.0, 100.0)
expect(f"the screen's destination wasn't matched to its event: {said}", said == [evs[1]])
expect("a move was matched to a loss or to another track", dc.events_saying(evs, ("changed", "system", "sink b"), 0, 105.0, 100.0) == []
       and dc.events_saying(evs, ("lost", "mic", ""), 0, 105.0, 100.0) == [evs[2]])
expect("an event far from the change was matched", dc.events_saying(evs, ("changed", "system", "sink b"), 1, 105.0, 100.0) == [evs[0]])
tab = "69\tusb_mic\tPipeWire\n74\tusb_mic_other\tPipeWire\n"
expect("a device was found by part of its name", dc.index_in(tab, "usb_mic") == "69" and dc.index_in(tab, "usb") is None
       and dc.index_in(tab, "usb_mic_other") == "74")

# --- the server's own events: only the watched device's going, or a default
# or card changing, counts.
server = dc.Server.__new__(dc.Server)
server.events = [(1.0, "Event 'remove' on sink #7"), (2.0, "Event 'remove' on source #9"), (3.0, "Event 'change' on server #1"),
                 (4.0, "Event 'change' on sink #9"), (5.0, "Event 'change' on card #3")]
expect("an unrelated sink's removal was the change", server.first_change(0.0, [("source", "9")])[0] == 2.0)
expect("a watched source 19 was taken for 9", server.first_change(0.0, [("source", "1")])[0] == 3.0)
expect("a default moving wasn't the change", server.first_change(2.5, [("source", "9")])[0] == 3.0)
expect("no watch took the first sign", server.first_change(0.0)[0] == 1.0)
expect("a sink's state change (idle) was taken for a change, or a card's profile wasn't", server.first_change(3.5, [("sink", "9")])[0] == 5.0)

# --- the timeline: an event is the change's if it is of the right kind and
# track and comes within the time.
events = [
    (1_000_000_000, 0, "device:lost", None),          # session 1.0 s, track 0
    (5_000_000_000, 1, "device:changed", "sink b"),   # 5.0 s, track 1
    (9_100_000_000, 1, "device:changed", "sink a"),   # 9.1 s, track 1
]
anchor = 100.0  # session zero on the monotonic clock
hit = dc.judge_timeline(events, {"device:changed"}, 1, 105.0, anchor)
expect(f"the change's event wasn't found: {hit}", hit and hit[0] == 1 and abs(hit[1]) < 1e-9 and hit[3] == "sink b")
hit = dc.judge_timeline(events, {"device:changed"}, 1, 109.0, anchor, used={1})
expect(f"the next change reused the last one's event: {hit}", hit and hit[0] == 2)
hit = dc.judge_timeline(events, {"device:changed"}, 1, 105.0, anchor, used={1, 2})
expect(f"an event already used was found again: {hit}", hit is None)
hit = dc.judge_timeline(events, {"device:lost"}, 1, 101.0, anchor)
expect(f"an event of another track was taken: {hit}", hit is None)
hit = dc.judge_timeline(events, {"device:changed"}, 1, 102.0, anchor)
expect(f"an event 3 s after the change should be returned as 3 s, for the caller to fail: {hit}", hit and abs(hit[1] - 3.0) < 1e-9)
hit = dc.judge_timeline(events, {"device:changed"}, 1, 105.6, anchor, used={2})
expect(f"an event 0.6 s before the change was taken as its: {hit}", hit is None)
hit = dc.judge_timeline(events, {"device:changed"}, 1, 105.3, anchor, used={2})
expect(f"an event 0.3 s before the change (the anchor's slack) wasn't taken: {hit}", hit and hit[0] == 1)

# --- the session as the library keeps it.
path = os.path.join(work, "library.db")
db = sqlite3.connect(path)
db.executescript("""
CREATE TABLE session (id INTEGER PRIMARY KEY);
CREATE TABLE mark (id INTEGER PRIMARY KEY, session_id INTEGER, at_ns INTEGER);
INSERT INTO mark (session_id, at_ns) VALUES (2, 4500000000), (2, 9000000000);
CREATE TABLE event (id INTEGER PRIMARY KEY, session_id INTEGER, track INTEGER, at_ns INTEGER, kind TEXT, detail TEXT);
INSERT INTO session VALUES (1), (2);
INSERT INTO event (session_id, track, at_ns, kind, detail) VALUES
  (1, 0, 10, 'device:lost', NULL), (2, 1, 30, 'device:changed', 'b'), (2, NULL, 20, 'gap', '40');
""")
db.commit()
db.close()
session, rows = dc.timeline(path)
expect(f"the newest session's timeline, in time order, wasn't read: {session} {rows}",
       session == 2 and rows == [(20, None, "gap", "40"), (30, 1, "device:changed", "b")])
expect("the first mark wasn't read", dc.first_mark_ns(path) == 4_500_000_000)
expect("audio_end_s", abs(dc.audio_end_s([0] * 32000, [(0, 0, 0), (1, 16000, 3_000_000_000)]) - 4.0) < 1e-9)

# --- report.txt: whole runs against stubbed cases and server, passing and
# failing, and one that dies. The file must hold every line printed.
import contextlib
import io


class NoServer:
    def close(self):
        pass


class NoFixtures:
    def __init__(self, *_):
        pass

    def close(self):
        pass


def full_run(name, cases, argv_cases):
    out_dir = os.path.join(work, name)
    os.mkdir(out_dir)
    dc.CASES = cases
    dc.Server, dc.Fixtures, dc.server_name = NoServer, NoFixtures, lambda: "stub"
    buf = io.StringIO()
    status = error = None
    with contextlib.redirect_stdout(buf):
        try:
            status = dc.main(["private", out_dir, "nota", "--cases", argv_cases])
        except RuntimeError as e:
            error = e
    with open(os.path.join(out_dir, "report.txt"), encoding="utf-8") as f:
        return status, error, buf.getvalue(), f.read()


def case_pass(ctx):
    ctx.report.say("== pass")
    ctx.report.check(True, "fine")


def case_fail(ctx):
    ctx.report.say("== fail")
    ctx.report.check(False, "broken")


def case_dies(ctx):
    ctx.report.say("== dies")
    raise RuntimeError("boom")


status, _, printed, filed = full_run("passing", {"p": case_pass}, "p")
expect(f"a passing run's exit status was {status}", status == 0)
expect(f"a passing run's report.txt wasn't what was printed: {filed!r} vs {printed!r}", filed == printed and "PASSED" in filed and "  ok   fine" in filed)
status, _, printed, filed = full_run("failing", {"p": case_pass, "f": case_fail}, "p,f")
expect(f"a failing run's exit status was {status}", status == 1)
expect(f"a failing run's report.txt wasn't what was printed: {filed!r} vs {printed!r}", filed == printed and "FAILED (1):" in filed and "  - broken" in filed)
status, error, printed, filed = full_run("dying", {"p": case_pass, "d": case_dies}, "p,d")
expect(f"a run that died lost its error: {error}", isinstance(error, RuntimeError))
expect(f"a run that died left report.txt short: {filed!r} vs {printed!r}", filed == printed and "== dies" in filed)

# An open report holds every line before it is closed (a run killed past
# its clean-up still has them).
r = dc.Report(os.path.join(work, "open.txt"))
with contextlib.redirect_stdout(io.StringIO()):
    r.say("one")
    r.check(False, "two")
with open(os.path.join(work, "open.txt"), encoding="utf-8") as f:
    held = f.read()
r.close()
expect(f"an unclosed report didn't hold its lines: {held!r}", held == "one\n  FAIL two\n")

# --- a device that comes back under the same name (a Bluetooth profile
# change) is a new default when the server's index for it is new.
expect("the same name on a new node wasn't a new default", dc.is_new_default("bz.1", "70", "bz.1", "75") is True)
expect("the same name on the same node was a new default", dc.is_new_default("bz.1", "70", "bz.1", "70") is False)
expect("the same name with no indexes was a new default", dc.is_new_default("bz.1", None, "bz.1", None) is False)
expect("another name wasn't a new default", dc.is_new_default("a", "1", "b", "1") is True)
expect("no default was a new default", dc.is_new_default("a", "1", "", None) is False)


class Ctx:
    report = None


def follow(events, old, now, listing_ids):
    quiet = Quiet()
    quiet.said = []
    ctx = Ctx()
    ctx.report = quiet
    real_index, real_desc = dc.device_index, dc.description
    dc.device_index = lambda listing, node: listing_ids.get(node)
    dc.description = lambda node, listing="sinks": "Melomania P100 SE"
    try:
        dc.follow_through(ctx, events, 1, old, now, "sinks", "70")
    finally:
        dc.device_index, dc.description = real_index, real_desc
    return quiet.said


moved = [(1, 1, "device:changed", "Melomania P100 SE")]
said = follow(moved, "bz.1", "bz.1", {"bz.1": "75"})
expect(f"a same-name node after a move failed the follow check: {said}", said and said[-1][0] is False)
said = follow(moved, "bz.1", "bz.1", {"bz.1": "70"})
expect(f"the old node as the default passed a move's follow check: {said}", said and said[-1][0] is False)
lost = [(1, 1, "device:lost", None)]
said = follow(lost, "bz.1", "bz.1", {"bz.1": "75"})
expect(f"a lost track with a new same-name default passed: {said}", said and said[-1][0] is False)

for failure in failures:
    print(f"device-changes.test.sh: {failure}", file=sys.stderr)
sys.exit(1 if failures else 0)
PY
