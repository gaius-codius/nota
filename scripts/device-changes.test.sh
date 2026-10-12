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
s.feed("\x1b[1;3H\x1b[4Pxx".encode())
expect(f"a delete of characters wasn't followed: {s.line(0)!r}", s.line(0).startswith("╭─xx"))
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
expect(f"a moved system wasn't read: {dc.warning(top)}", dc.warning(top) == ("changed", "system", "ndc_probe_b Audio/Sink sink +1"))
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
expect(f"the second epoch's opening was {summary[1][1]}",
       summary[1][1][:4] == ["silence", "silence", "a", "b"])
expect("merged didn't drop repeats", dc.merged(["a", "a", "b", "b", "a"]) == ["a", "b", "a"])

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

for failure in failures:
    print(f"device-changes.test.sh: {failure}", file=sys.stderr)
sys.exit(1 if failures else 0)
PY
