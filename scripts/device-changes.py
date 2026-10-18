#!/usr/bin/env python3
"""The driver of scripts/device-changes.sh (read that script's header first).

For each case it starts `nota record` in a pseudo-terminal, changes a
device, and times how long the Recording screen takes to say so. The screen
is rebuilt from nota's output by a small terminal emulator (the screen draws
only the cells that changed, so no single write holds a whole warning), and
each write is stamped as it is read. Then it stops nota and reads what the
session kept: the timeline's events, and (where the script played tones into
the devices) the tone in each epoch's audio, which says which device was
recorded.

Run through the shell script, which owns the PipeWire instance, the saved
defaults and the clean-up. Standard library only, plus pactl, pw-play,
pw-link and ffmpeg on PATH.
"""

import argparse
import array
import codecs
import fcntl
import math
import operator
import os
import pty
import re
import select
import signal
import sqlite3
import struct
import subprocess
import sys
import termios
import threading
import time
import unicodedata
import wave

RATE = 16000
# The names of everything this run makes on the server start with this, which
# is unique to the run (the shell script passes it and unloads by it).
PREFIX = "ndc_"
# The exit criterion: a change shows, and is logged, within this.
WITHIN_S = 2.0
# The timeline's clock starts when nota starts; the first draw anchors it.
# An event may be stamped this long before the change it names (the
# anchor's uncertainty), never after WITHIN_S.
ANCHOR_SLACK_S = 0.5
# How long an event shows in the top border (the UI spec's "about 10 s").
EVENT_SHOWN_S = 10.0
# A tone window: 20 ms, a whole number of cycles of every tone below.
WINDOW = 320
# The part of a window's energy a tone must hold to be named: a stream
# hearing two devices at once holds half in each.
TONE_SHARE = 0.7
# Audio at the start and end of an epoch is not judged for its tone, and an
# epoch must be this pure in the rest: a device's audio under another's name
# shows as the share of windows that aren't the epoch's own tone.
EDGE_S = 0.3
PURITY = 0.95
# The amplitude of the tones played into the user's own outputs: about -35
# dBFS, before the sink's volume.
REAL_TONE = 600
# Windows at an epoch's start, where a late buffer of the old device would
# show (GAI-434).
OPENING_WINDOWS = 10
# The recording may end this long after the Lost event: the buffers that
# were already on their way.
LOST_END_SLACK_S = 0.3

# Warnings in the top border. `mic` and `system` are the track names.
LOST_RE = re.compile(r"⚠ (?P<track>mic|system) lost")
CHANGED_RE = re.compile(r"↪ (?P<track>mic|system): (?P<name>.*?)\s*(?:·|─|╮|$)")
TRACK_OF = {"mic": 0, "system": 1}

# What the server reports when a device appears or goes, a card changes
# (a Bluetooth profile), or a default moves.
SERVER_CHANGE_RE = re.compile(
    r"Event '(?:remove|new)' on (?:sink|source|card) #|Event 'change' on (?:card|server)"
)


def fixture_name(name):
    """The server name of the fixture `name`, unique to this run."""
    return f"{PREFIX}{name}"


# ---------------------------------------------------------------- the screen


class Screen:
    """The cells nota's output has drawn, enough of a terminal to follow
    ratatui's output: printing, cursor moves, erases, repeats and the
    inserts and deletes that shift a line. Colours and modes are skipped.
    Scrolling isn't followed; the Recording screen never scrolls."""

    def __init__(self, rows=40, cols=120):
        self.rows, self.cols = rows, cols
        self.grid = [[" "] * cols for _ in range(rows)]
        self.r = self.c = 0
        self.saved = (0, 0)
        self.last = " "
        self.state = "text"
        self.params = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def feed(self, data):
        for ch in self.decoder.decode(data):
            self.char(ch)

    def char(self, ch):
        state = self.state
        if state == "text":
            if ch == "\x1b":
                self.state = "esc"
            elif ch == "\r":
                self.c = 0
            elif ch == "\n":
                self.r = min(self.r + 1, self.rows - 1)
            elif ch == "\b":
                self.c = max(self.c - 1, 0)
            elif ch == "\t":
                self.c = min((self.c // 8 + 1) * 8, self.cols - 1)
            elif ch >= " ":
                self.put(ch)
        elif state == "esc":
            self.state = "text"
            if ch == "[":
                self.state, self.params = "csi", ""
            elif ch in "]PX^_":
                self.state = "string"
            elif ch in "()":
                self.state = "skip"
            elif ch == "7":
                self.saved = (self.r, self.c)
            elif ch == "8":
                self.r, self.c = self.saved
        elif state == "csi":
            if "@" <= ch <= "~":
                self.state = "text"
                self.csi(self.params, ch)
            else:
                self.params += ch
        elif state == "string":
            if ch == "\x07":
                self.state = "text"
            elif ch == "\x1b":
                self.state = "esc"
        else:
            self.state = "text"

    def put(self, ch):
        if unicodedata.combining(ch) or unicodedata.category(ch) in ("Mn", "Cf"):
            return
        width = 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1
        if self.c + width <= self.cols:
            self.grid[self.r][self.c] = ch
            if width == 2:
                self.grid[self.r][self.c + 1] = ""
        self.c = min(self.c + width, self.cols)
        self.last = ch

    def csi(self, params, final):
        if final in "hlm":
            return
        try:
            nums = [int(p) if p else 0 for p in params.split(";")]
        except ValueError:
            return
        n = nums[0] or 1
        if final in "Hf":
            self.r = min(max((nums[0] or 1) - 1, 0), self.rows - 1)
            col = nums[1] if len(nums) > 1 else 1
            self.c = min(max((col or 1) - 1, 0), self.cols - 1)
        elif final == "A":
            self.r = max(self.r - n, 0)
        elif final == "B":
            self.r = min(self.r + n, self.rows - 1)
        elif final == "C":
            self.c = min(self.c + n, self.cols - 1)
        elif final == "D":
            self.c = max(self.c - n, 0)
        elif final == "G":
            self.c = min(n - 1, self.cols - 1)
        elif final == "d":
            self.r = min(n - 1, self.rows - 1)
        elif final == "J":
            self.erase_display(nums[0])
        elif final == "K":
            self.erase_line(nums[0])
        elif final == "X":
            for i in range(self.c, min(self.c + n, self.cols)):
                self.grid[self.r][i] = " "
        elif final == "b":
            for _ in range(n):
                self.put(self.last)
        elif final == "P":
            row = self.grid[self.r]
            row[self.c:] = row[self.c + n:] + [" "] * min(n, self.cols - self.c)
        elif final == "@":
            row = self.grid[self.r]
            row[self.c:] = ([" "] * n + row[self.c:])[: self.cols - self.c]
        elif final == "s":
            self.saved = (self.r, self.c)
        elif final == "u":
            self.r, self.c = self.saved

    def erase_display(self, mode):
        if mode in (2, 3):
            self.grid = [[" "] * self.cols for _ in range(self.rows)]
        elif mode == 0:
            self.erase_line(0)
            for r in range(self.r + 1, self.rows):
                self.grid[r] = [" "] * self.cols
        elif mode == 1:
            self.erase_line(1)
            for r in range(self.r):
                self.grid[r] = [" "] * self.cols

    def erase_line(self, mode):
        row = self.grid[self.r]
        if mode == 0:
            row[self.c:] = [" "] * (self.cols - self.c)
        elif mode == 1:
            row[: self.c + 1] = [" "] * (self.c + 1)
        else:
            self.grid[self.r] = [" "] * self.cols

    def line(self, r):
        return "".join(self.grid[r]).rstrip()


def warning(row):
    """What the top border's row says about a device: `("lost", "mic", "")`
    or `("changed", "system", "<device>")`, or None."""
    if m := LOST_RE.search(row):
        return ("lost", m["track"], "")
    if m := CHANGED_RE.search(row):
        # The border adds `+N` for the other conditions that hold.
        return ("changed", m["track"], re.sub(r"\s*\+\d+$", "", m["name"]))
    return None


# ---------------------------------------------------------------------- nota


class Nota:
    """`nota record` in a pseudo-terminal of its own, its screen followed."""

    def __init__(self, argv, log, rows=40, cols=120):
        self.screen = Screen(rows, cols)
        self.log = log
        self.started = time.monotonic()
        self.first_draw = None
        self.status = None
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.environ["TERM"] = "xterm-256color"
            # The size is set before nota reads it.
            os.execvp("sh", ["sh", "-c", f'stty cols {cols} rows {rows}; exec "$0" "$@"', *argv])
        self.eof = False
        self.tail = ""

    def pump(self, seconds, until=None):
        """Reads nota's output for `seconds`, or until `until()` holds."""
        end = time.monotonic() + seconds
        while time.monotonic() < end and not self.eof:
            if until and until():
                return
            ready, _, _ = select.select([self.fd], [], [], 0.02)
            if not ready:
                continue
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                self.eof = True
                return
            if not data:
                self.eof = True
                return
            now = time.monotonic()
            self.log.write(b"%.3f " % (now - self.started) + data.hex().encode() + b"\n")
            self.screen.feed(data)
            row = self.screen.line(0)
            if self.first_draw is None and row:
                self.first_draw = now
            self.tail = (self.tail + data.decode("utf-8", "replace"))[-4000:]

    def row0(self):
        return self.screen.line(0)

    def key(self, text):
        os.write(self.fd, text.encode())

    def stop(self, patience_s=120):
        """Stops the recording as the screen does (`s`, then `y`) and waits
        for nota to publish and exit."""
        self.key("s")
        self.pump(0.5)
        self.key("y")
        self.pump(patience_s, until=lambda: self.eof)
        self.reap(patience_s=10)

    def reap(self, patience_s=0):
        """Waits for nota's exit status; a nota that won't exit is hung up
        on (by its PID), as a closing terminal does, so it still publishes."""
        if self.status is not None:
            return
        end = time.monotonic() + patience_s
        while True:
            done, status = os.waitpid(self.pid, os.WNOHANG)
            if done:
                self.status = os.waitstatus_to_exitcode(status)
                return
            if time.monotonic() >= end:
                break
            time.sleep(0.05)
        os.kill(self.pid, signal.SIGHUP)
        _, status = os.waitpid(self.pid, 0)
        self.status = os.waitstatus_to_exitcode(status)


# -------------------------------------------------------------------- server


# pactl's words are translated; the parsing here is of the English ones.
C_LOCALE = {**os.environ, "LC_ALL": "C"}


def pactl(*args, check=True):
    done = subprocess.run(["pactl", *args], capture_output=True, text=True, check=False, env=C_LOCALE)
    if check and done.returncode:
        raise SystemExit(f"device-changes: pactl {' '.join(args)}: {done.stderr.strip()}")
    return done.stdout.strip()


class Server:
    """The server's own events (`pactl subscribe`), each stamped as read:
    the earliest sign that a device changed, whoever changed it."""

    def __init__(self):
        self.events = []
        self.proc = subprocess.Popen(
            ["pactl", "subscribe"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1, env=C_LOCALE
        )
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        for line in self.proc.stdout:
            self.events.append((time.monotonic(), line.strip()))

    def first_change(self, since, watch=None):
        """The first sign of a change after `since`: with `watch`, a list of
        `(kind, index)` of the devices that matter, only the coming or going
        of one of them, a default moving or a card changing."""
        if watch:
            devices = "|".join(f"{kind} #{index}" for kind, index in watch)
            wanted = re.compile(rf"Event '(?:remove|new)' on (?:{devices})$|Event 'change' on (?:card|server)")
        else:
            wanted = SERVER_CHANGE_RE
        for t, line in list(self.events):
            if t >= since and wanted.search(line):
                return t, line
        return None

    def close(self):
        self.proc.terminate()
        self.proc.wait()


# ------------------------------------------------------------------- fixtures


class Fixtures:
    """What a case makes on the server (null sinks, virtual sources, tone
    players), all removed by the case's end, by module index and PID."""

    def __init__(self, work):
        self.work = work
        self.modules = {}
        self.players = []

    def null_sink(self, name):
        props = f"sink_properties=node.description={name} priority.session=1 priority.driver=1"
        self.modules[name] = pactl("load-module", "module-null-sink", f"sink_name={name}", props)
        wait_for(lambda: listed("sinks", name), f"{name} to appear")

    def usb_mic(self, name):
        """A microphone (an `Audio/Source`, as a USB one is) that plays what
        is played into its feed sink, `<name>_feed`."""
        feed = f"{name}_feed"
        self.null_sink(feed)
        self.modules[name] = pactl(
            "load-module", "module-remap-source", f"master={feed}.monitor", f"source_name={name}",
            f"source_properties=node.description={name}",
        )
        wait_for(lambda: listed("sources", name), f"{name} to appear")

    def remove(self, name):
        """Takes the device away, and its feed if it has one."""
        pactl("unload-module", self.modules.pop(name))
        if f"{name}_feed" in self.modules:
            pactl("unload-module", self.modules.pop(f"{name}_feed"))

    def play(self, freq, node, amplitude, seconds=120):
        """A tone of `freq` Hz into the sink `node` for `seconds` (more than
        the case lasts). The
        player is linked by hand: a stream the session manager places
        follows the default sink when it moves."""
        path = tone_file(self.work, freq, amplitude, seconds)
        name = f"tone_{freq}_{node}"
        proc = subprocess.Popen(
            ["pw-play", "--target", "0", "-P", f"node.name={name}", path],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        self.players.append(proc)

        def ported():
            return f"{name}:output_MONO" in subprocess.run(["pw-link", "-o"], capture_output=True, text=True).stdout

        wait_for(ported, f"{name}'s ports")
        inputs = subprocess.run(["pw-link", "-i"], capture_output=True, text=True).stdout.split("\n")
        ports = [line.strip() for line in inputs if line.startswith(f"{node}:playback_")]
        if not ports:
            raise SystemExit(f"device-changes: {node} has no playback ports to play a tone into")
        for port in ports:
            subprocess.run(["pw-link", f"{name}:output_MONO", port], check=True)

    def close(self):
        for proc in self.players:
            proc.terminate()
        for proc in self.players:
            proc.wait()
        self.players = []
        for name in list(self.modules):
            pactl("unload-module", self.modules.pop(name), check=False)


def listed(listing, node):
    """Whether `pactl list short <listing>` has a device named `node`."""
    return node in device_names(listing)


def device_names(listing):
    return [f[1] for f in (line.split("\t") for line in pactl("list", "short", listing).splitlines()) if len(f) > 1]


def device_index(listing, node):
    """The server's index of the sink or source `node`, or None."""
    return index_in(pactl("list", "short", listing), node)


def index_in(short_listing, node):
    """The index of the device named exactly `node` in `pactl list short`."""
    for line in short_listing.splitlines():
        f = line.split("\t")
        if len(f) > 1 and f[1] == node:
            return f[0]
    return None


def wait_for(condition, what, seconds=10):
    end = time.monotonic() + seconds
    while not condition():
        if time.monotonic() > end:
            raise SystemExit(f"device-changes: gave up waiting for {what}")
        time.sleep(0.05)


def tone_file(work, freq, amplitude, seconds):
    """A mono 16 kHz WAV of `seconds` of a sine of `freq` Hz."""
    path = os.path.join(work, f"tone-{freq}-{int(amplitude)}-{seconds}.wav")
    if not os.path.exists(path):
        cycle = [int(amplitude * math.sin(2 * math.pi * freq * i / RATE)) for i in range(RATE)]
        with wave.open(path, "wb") as w:
            w.setnchannels(1)
            w.setsampwidth(2)
            w.setframerate(RATE)
            one_second = struct.pack(f"<{RATE}h", *cycle)
            w.writeframes(one_second * seconds)
    return path


# ------------------------------------------------------------- the recording


def timeline(db_path):
    """The session's id and its timeline: `(at_ns, track, kind, detail)`."""
    db = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    try:
        row = db.execute("SELECT id FROM session ORDER BY id DESC LIMIT 1").fetchone()
        if row is None:
            return None, []
        events = db.execute(
            "SELECT at_ns, track, kind, detail FROM event WHERE session_id = ? ORDER BY at_ns, id",
            (row[0],),
        ).fetchall()
        return row[0], events
    finally:
        db.close()


def first_mark_ns(db_path):
    """The session time of the first mark, or None."""
    db = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    try:
        row = db.execute(
            "SELECT at_ns FROM mark WHERE session_id = (SELECT MAX(id) FROM session) ORDER BY id LIMIT 1"
        ).fetchone()
        return row[0] if row else None
    finally:
        db.close()


def track_audio(data_dir, session, track):
    """The track's samples and its epochs' first samples, in order."""
    db = sqlite3.connect(f"file:{os.path.join(data_dir, 'library.db')}?mode=ro", uri=True)
    try:
        segments = db.execute(
            "SELECT start_sample, end_sample FROM segment WHERE session_id = ? AND track = ? ORDER BY start_sample",
            (session, track),
        ).fetchall()
        epochs = db.execute(
            "SELECT epoch, first_sample, anchor_ns FROM epoch WHERE session_id = ? AND track = ? ORDER BY epoch",
            (session, track),
        ).fetchall()
    finally:
        db.close()
    samples = array.array("h")
    for start, end in segments:
        path = os.path.join(data_dir, "sessions", str(session), "audio", f"seg-t{track}-{start:012d}.flac")
        raw = subprocess.run(
            ["ffmpeg", "-nostdin", "-loglevel", "error", "-i", path, "-f", "s16le", "-ar", str(RATE), "-ac", "1", "-"],
            check=True, capture_output=True,
        ).stdout
        part = array.array("h")
        part.frombytes(raw)
        if sys.byteorder == "big":
            part.byteswap()
        if len(part) != end - start or len(samples) != start:
            raise SystemExit(f"device-changes: track {track}'s segment from {start} isn't {end - start} samples after {len(samples)}")
        samples.extend(part)
    return samples, epochs


_BASIS = {}


def basis(freq):
    if freq not in _BASIS:
        _BASIS[freq] = (
            [math.cos(2 * math.pi * freq * i / RATE) for i in range(WINDOW)],
            [math.sin(2 * math.pi * freq * i / RATE) for i in range(WINDOW)],
        )
    return _BASIS[freq]


def classify(window, tones):
    """Which of `tones` ({name: Hz}) a window holds: its name, `silence`, or
    `other` when no tone has most of the window's energy."""
    energy = sum(s * s for s in window)
    if energy / len(window) < 100:  # an RMS under 10 of 32768
        return "silence"
    best, share = "other", 0.0
    for name, freq in tones.items():
        cos, sin = basis(freq)
        a = sum(map(operator.mul, window, cos))
        b = sum(map(operator.mul, window, sin))
        part = 2 * (a * a + b * b) / len(window) / energy
        if part > share:
            best, share = name, part
    return best if share >= TONE_SHARE else "other"


def epoch_tones(samples, firsts, tones):
    """Each epoch's tones: `(dominant, opening, length, purity)`. The
    dominant tone is that of most of its body windows (the first and last
    EDGE_S aside, unless the epoch is shorter than that); purity is the share
    of body windows that are the dominant one. `opening` is the classes of
    its first OPENING_WINDOWS windows."""
    bounds = [*firsts, len(samples)]
    out = []
    edge = int(EDGE_S * RATE)
    for first, end in zip(bounds, bounds[1:]):
        lo, hi = first + edge, end - edge
        if hi - lo < WINDOW:
            lo, hi = first, end
        classes = [classify(samples[at:at + WINDOW], tones) for at in range(lo, hi - WINDOW + 1, WINDOW)]
        counts = {name: classes.count(name) for name in set(classes)}
        dominant = max(counts, key=counts.get) if counts else None
        purity = counts[dominant] / len(classes) if counts else 0.0
        opening = [classify(samples[first + i * WINDOW:first + (i + 1) * WINDOW], tones)
                   for i in range(OPENING_WINDOWS) if first + (i + 1) * WINDOW <= end]
        out.append((dominant, opening, end - first, purity))
    return out


def merged(names):
    """`names` without consecutive repeats."""
    return [n for i, n in enumerate(names) if i == 0 or n != names[i - 1]]


# ------------------------------------------------------------------- results


class Report:
    def __init__(self, path):
        self.file = open(path, "w", encoding="utf-8")
        self.failed = []

    def say(self, text=""):
        print(text, flush=True)
        # Flushed per line: the file holds every line printed even if the run
        # dies before `close`.
        self.file.write(text + "\n")
        self.file.flush()

    def close(self):
        self.file.close()

    def check(self, ok, text):
        self.say(f"  {'ok  ' if ok else 'FAIL'} {text}")
        if not ok:
            self.failed.append(text)
        return ok




def judge_timeline(events, kinds, track, change_s, anchor_s, used=()):
    """The first unused event of one of `kinds` on `track` (any track if
    None) at or just before the change: `(index, seconds after the change,
    kind, detail)`, or None. Event times are session time; `anchor_s` is the
    session's zero on the monotonic clock."""
    for i, (at_ns, ev_track, kind, detail) in enumerate(events):
        if i in used or kind not in kinds or (track is not None and ev_track != track):
            continue
        after = anchor_s + at_ns / 1e9 - change_s
        if after >= -ANCHOR_SLACK_S:
            return i, after, kind, detail
    return None


def events_saying(events, warning_, track, change_s, anchor_s):
    """The timeline events near the change that are what the screen said: of
    the warning's kind, on the track, and for a move, of the same device."""
    kind, _, name = warning_
    return [
        e for e in events
        if e[2] == f"device:{kind}" and (track is None or e[1] == track)
        and (kind == "lost" or same_device(name, e[3]))
        and -ANCHOR_SLACK_S <= anchor_s + e[0] / 1e9 - change_s <= WITHIN_S + 1.5
    ]


def audio_end_s(samples, epochs):
    """Where the last sample is in session time, in seconds."""
    _, first, anchor_ns = epochs[-1]
    return anchor_ns / 1e9 + (len(samples) - first) / RATE


# --------------------------------------------------------------------- a case


class Run:
    """One case's recording: nota in a pseudo-terminal, and the changes made
    to the devices while it records."""

    def __init__(self, ctx, name, argv):
        self.ctx, self.name = ctx, name
        self.armed = 0.0
        self.marked = None
        self.dir = os.path.join(ctx.args.work, name)
        os.makedirs(self.dir)
        self.data = os.path.join(self.dir, "data")
        self.log = open(os.path.join(self.dir, "screen.log"), "wb")
        argv = [ctx.args.nota, "record", "--data", self.data, "--title", name, *argv]
        self.nota = Nota(argv, self.log)
        ctx.runs.append(self)
        self.changes = []

    def settle(self, seconds=4):
        self.nota.pump(seconds)
        if self.nota.eof:
            tail = re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", self.nota.tail)
            raise SystemExit(f"device-changes: nota stopped at once:\n{tail[-1500:]}")
        # A mark, pressed at a moment noted here, says where the session's
        # time zero is on this clock: the timeline's events are in session
        # time.
        before = time.monotonic()
        self.nota.key("m")
        self.marked = (before + time.monotonic()) / 2
        self.nota.pump(0.3)

    def ask(self, text):
        """Tells the user what to do and waits for Enter, with nota's output
        read meanwhile (a full pipe would stall its screen)."""
        if not sys.stdin.isatty():
            raise SystemExit("device-changes: this case needs the user at a terminal")
        print(f"\n>>> {text}\n>>> Press Enter when ready; the change comes after.", flush=True)
        while not select.select([sys.stdin], [], [], 0.1)[0]:
            self.nota.pump(0.1)
            if self.nota.eof:
                raise SystemExit("device-changes: nota stopped while waiting for the user")
        sys.stdin.readline()
        self.armed = time.monotonic()

    def change(self, what, act, expect, watch=None):
        """Runs `act()` and reads the screen until `expect(warning)` holds.
        `act` returns the moment of the change, or None for a change the
        user makes: then the moment is the server's first sign of it, of the
        devices in `watch`."""
        # A warning from before has gone from the border; the next is new.
        self.nota.pump(EVENT_SHOWN_S + 1, until=lambda: not warning(self.nota.row0()))
        self.ctx.report.check(not warning(self.nota.row0()), f"the border was clear before: {what}")
        before = time.monotonic()
        t = act()
        if t is None:
            # The user's change: only what the server says from here counts.
            before = max(before, self.armed)
        seen = {}

        def shown():
            w = warning(self.nota.row0())
            if w and expect(w) and "at" not in seen:
                seen["at"], seen["w"] = time.monotonic(), w
            return "at" in seen

        self.nota.pump(WITHIN_S + 1.5 if t is not None else self.ctx.args.manual_wait, until=shown)
        sign = self.ctx.server.first_change(before, watch if t is None else None)
        if t is None:
            t = sign[0] if sign else before
        # The rest of the change arrives (a stream following a move).
        self.nota.pump(2.0)
        self.changes.append({"what": what, "t": t, "screen": seen.get("at"), "warning": seen.get("w"), "server": sign})

    def finish(self):
        """Stops nota, and reads what the session kept."""
        self.nota.stop()
        self.log.close()
        db = os.path.join(self.data, "library.db")
        session, events = timeline(db)
        tail = re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", self.nota.tail)
        mark = first_mark_ns(db) if self.marked else None
        if mark is None:
            self.ctx.report.say("  (no mark was kept, so the timeline's clock is placed at nota's first draw; its times may be late by up to a second)")
            return session, events, self.nota.first_draw, tail
        return session, events, self.marked - mark / 1e9, tail

    def check_changes(self, events, anchor, kinds, track, expect_text):
        """Each change was said on the screen and logged on the timeline
        within WITHIN_S. Returns the timeline events that matched."""
        report, used, hits = self.ctx.report, set(), []
        for change in self.changes:
            t, screen = change["t"], change["screen"]
            report.say(f"- {change['what']}")
            if change["server"] and change["server"][0] != t:
                report.say(f"    the server's first sign: {1000 * (change['server'][0] - t):+.0f} ms ({change['server'][1]})")
            elif change["server"]:
                report.say(f"    the change is timed from the server's first sign: {change['server'][1]}")
            shown = ""
            if screen is not None:
                w = change["warning"]
                shown = f" {1000 * (screen - t):.0f} ms after the change: {w[0]} {w[1]} {w[2]}".rstrip()
            report.check(screen is not None and screen - t <= WITHIN_S, f"the screen said {expect_text}" + (shown or ": it never did"))
            hit = judge_timeline(events, kinds, track, t, anchor, used)
            if hit:
                used.add(hit[0])
            report.check(
                hit is not None and hit[1] <= WITHIN_S,
                "the timeline logged " + (f"{hit[2]} {hit[3] or ''} {1000 * hit[1]:+.0f} ms from the change".replace("  ", " ") if hit else f"none of {sorted(kinds)}"),
            )
            if screen is not None:
                # Moves close together are drawn as the last one: the
                # warning is some event of the timeline's near the change.
                w = change["warning"]
                said = events_saying(events, w, track, t, anchor)
                report.check(bool(said), f"the screen's {w[0]} {w[2]!r} is an event on the timeline near the change")
            hits.append(hit)
        self.list_events(events, anchor)
        return hits

    def list_events(self, events, anchor):
        """Every timeline event around the changes, as the report's record
        of what was logged."""
        if not self.changes:
            return
        low = min(c["t"] for c in self.changes) - ANCHOR_SLACK_S
        high = max(c["t"] for c in self.changes) + 6.0
        self.ctx.report.say("  timeline around the changes (ms from the first change):")
        first = min(c["t"] for c in self.changes)
        for at_ns, track, kind, detail in events:
            wall = anchor + at_ns / 1e9
            if low <= wall <= high:
                self.ctx.report.say(f"    {1000 * (wall - first):+7.0f}  track {track}  {kind}  {detail or ''}".rstrip())

    def check_exit(self):
        self.ctx.report.check(self.nota.status == 0, f"nota exited with status {self.nota.status}")


class Context:
    def __init__(self, args, report, server, fixtures):
        self.args, self.report, self.server, self.fixtures = args, report, server, fixtures
        self.runs = []
        self.private = args.mode == "private"

    def default_sink(self):
        return pactl("get-default-sink")


def await_present(node, listing):
    """Asks the user to plug `node` back in if it isn't listed (an earlier
    case unplugged it), and waits for it."""
    if listed(listing, node):
        return
    if not sys.stdin.isatty():
        raise SystemExit("device-changes: this case needs the user at a terminal")
    input(f"\n>>> {node} isn't there. Plug it in, then press Enter. ")
    wait_for(lambda: listed(listing, node), f"{node} to come back", seconds=60)
    time.sleep(2)  # the session manager's own reaction to it


def description(node, listing="sinks"):
    """The description `pactl` gives a sink (or source): what the screen
    shows after a move. Empty if there is none."""
    text = pactl("list", listing, check=False)
    for block in text.split("\n" + listing[:-1].capitalize() + " #"):
        if f"Name: {node}\n" in block:
            for line in block.splitlines():
                if line.strip().startswith("Description:"):
                    return line.split(":", 1)[1].strip()
    return ""


def names_it(detail, *names):
    """Whether a device's detail on the timeline names one of `names`."""
    d = (detail or "").lower()
    return bool(d) and any(n and n.lower() in d for n in names)


def tone_checks(report, samples, epochs, tones, expected, label):
    """The epochs' tones, in order, are one of the `expected` sequences, and
    each epoch is the one tone throughout: the recorded device was the one the
    timeline names, and only it. Returns the epochs' tone summaries."""
    summary = epoch_tones(samples, [first for _, first, _ in epochs], tones)
    names = merged([
        name if purity >= PURITY else f"mixed({name})"
        for name, _, length, purity in summary
        if name and length >= WINDOW
    ])
    report.check(names in expected, f"{label}: the epochs' audio was {names} (wanted one of {expected})")
    return summary


def report_opening(report, summary, tones):
    """GAI-434: whether the old device's audio opened a new epoch."""
    old_in = 0
    for k, (_, opening, _, _) in enumerate(summary[1:], 1):
        previous = summary[k - 1][0]
        old = [i for i, name in enumerate(opening) if name == previous and previous in tones]
        old_in += bool(old)
        report.say(f"    epoch {k} opening, 20 ms windows: {opening}" + (f"  <- the old device's tone in window(s) {old}" if old else ""))
    report.say(f"  GAI-434: the old device's audio opened {old_in} of {max(len(summary) - 1, 0)} new epochs")
    return old_in


def same_device(screen, logged):
    """Whether the name the screen shows is the device the timeline logged:
    the screen may cut a long name short."""
    a, b = (screen or "").strip().lower(), (logged or "").strip().lower()
    return bool(a) and bool(b) and (b.startswith(a) or a.startswith(b))


def last_device_kind(events, track):
    """The kind of the track's last device event on the timeline, or None."""
    kinds = [kind for _, ev_track, kind, _ in events if ev_track == track and kind.startswith("device:")]
    return kinds[-1] if kinds else None


def confirm(text):
    """Tells the user what is about to happen and waits for Enter, before
    anything is made or played."""
    if not sys.stdin.isatty():
        raise SystemExit("device-changes: this case needs the user at a terminal")
    input(f"\n>>> {text}\n>>> Press Enter to go on. ")


def check_audio(ctx, run, session, events, track, anchor, tones, expected, lost=None):
    """What the track kept: audio, in the tones of the devices recorded, in
    order, each epoch the one tone throughout; and after a loss nothing past
    it. `lost` is whether the track was lost for good (from the timeline, if
    None: its last device event is a loss)."""
    report = ctx.report
    samples, epochs = track_audio(run.data, session, track)
    report.check(bool(epochs) and len(samples) > 0, f"track {track} kept audio ({len(samples) / RATE:.1f} s)")
    if lost is None:
        lost = last_device_kind(events, track) == "device:lost"
    if lost and epochs:
        after = anchor + audio_end_s(samples, epochs) - run.changes[0]["t"]
        report.check(after <= LOST_END_SLACK_S, f"no audio after the loss: the track ends {1000 * after:+.0f} ms from the change")
    if tones and epochs:
        return tone_checks(report, samples, epochs, tones, expected, "no wrong device")
    if not tones:
        report.say("  (no tone was played into the device, so what it recorded is known only from the timeline's name for it)")
    return []


def case_default_output(ctx):
    args, report, fx = ctx.args, ctx.report, ctx.fixtures
    report.say(f"== default-output: the system audio follows the default output, switched {args.switches} times")
    if ctx.private:
        a, b = fixture_name("probe_a"), fixture_name("probe_b")
        tones = {a: 500, b: 1500}
        amplitude = 8000
    else:
        a, b = ctx.default_sink(), args.other_sink
        if a == b or not listed("sinks", b):
            raise SystemExit(f"device-changes: --other-sink {b} must be a sink other than the default ({a})")
        tones = {a: 500, b: 1500} if args.tone else {}
        amplitude = REAL_TONE
        report.say(f"  from {a} to {b} and back; the defaults are put back at the end")
        confirm(f"The default output will be switched from {a} to {b} and back {args.switches} times, then put back."
                + (" A quiet tone plays into each of the two for about as long." if tones else ""))
    if ctx.private:
        for name in (fixture_name("quiet_mic"), a, b):
            fx.null_sink(name)
        pactl("set-default-sink", a)
        wait_for(lambda: ctx.default_sink() == a, "the default to become probe_a")
    else:
        fx.null_sink(fixture_name("quiet_mic"))
    named = {a: description(a) or a, b: description(b) or b}
    # Each switch waits for the last one's warning to leave the border.
    lasts = 60 + (int(EVENT_SHOWN_S) + 10) * args.switches
    for node, freq in tones.items():
        fx.play(freq, node, amplitude, lasts)
    run = Run(ctx, "default-output", ["--mic", fixture_name("quiet_mic")])
    run.settle()
    target = b
    for i in range(args.switches):
        node = target

        def act(node=node):
            t = time.monotonic()
            pactl("set-default-sink", node)
            return t

        run.change(f"switch {i + 1}: default output -> {node}", act,
                   lambda w, node=node: w[0] == "changed" and w[1] == "system" and same_device(w[2], named[node]))
        target = a if target == b else b
    if ctx.default_sink() != a:
        pactl("set-default-sink", a)
    session, events, anchor, tail = run.finish()
    hits = run.check_changes(events, anchor, {"device:changed"}, TRACK_OF["system"], "the system audio moved")
    nodes = [b, a] * args.switches
    for hit, node in zip(hits, nodes):
        if hit:
            report.check(names_it(hit[3], node, named[node]), f"the logged device {hit[3]!r} names {node}")
    if tones:
        sequence = [a]
        for _ in range(args.switches):
            sequence.append(b if sequence[-1] == a else a)
        samples, epochs = track_audio(run.data, session, TRACK_OF["system"])
        summary = tone_checks(report, samples, epochs, tones, [sequence], "no wrong device")
        report_opening(report, summary, tones)
    else:
        report.say("  (no tone was played, so the audio isn't checked: the old device's buffer after a move can't be told; run with --tone)")
    run.check_exit()


def case_pinned_removed(ctx):
    report, fx = ctx.report, ctx.fixtures
    report.say("== pinned-removed: the microphone track pinned to a device that is removed")
    pinned = fixture_name("pin_sink")
    for name in (pinned, fixture_name("quiet_sys")):
        fx.null_sink(name)
    fx.play(500, pinned, 8000)
    run = Run(ctx, "pinned-removed", ["--mic", pinned, "--system", fixture_name("quiet_sys")])
    run.settle()

    def act():
        t = time.monotonic()
        fx.remove(pinned)
        return t

    run.change("pinned device removed", act, lambda w: w[0] == "lost" and w[1] == "mic")
    session, events, anchor, tail = run.finish()
    run.check_changes(events, anchor, {"device:lost"}, TRACK_OF["mic"], "the microphone was lost")
    check_audio(ctx, run, session, events, TRACK_OF["mic"], anchor, {pinned: 500}, [[pinned]], lost=True)
    run.check_exit()


def user_change(run, text, listing, node):
    """A change the user makes: they press Enter first, with `node` still
    there (so an early change doesn't read as a late warning)."""
    run.ask(text)
    present = listed(listing, node)
    run.ctx.report.check(present, f"{node} was still there when the user pressed Enter")
    print(">>> Now.", flush=True)


def case_usb_mic_pinned(ctx):
    args, report, fx = ctx.args, ctx.report, ctx.fixtures
    report.say("== usb-mic-pinned: the microphone track pinned to a USB microphone that is unplugged")
    fx.null_sink(fixture_name("quiet_sys"))
    tones = {}
    if ctx.private:
        node = fixture_name("usb_mic")
        fx.usb_mic(node)
        fx.play(500, f"{node}_feed", 8000)
        tones = {node: 500}
    else:
        node = args.usb_mic
        await_present(node, "sources")
    watch = [("source", device_index("sources", node))]
    run = Run(ctx, "usb-mic-pinned", ["--mic", node, "--system", fixture_name("quiet_sys")])
    run.settle()

    def act():
        if ctx.private:
            t = time.monotonic()
            fx.remove(node)
            return t
        user_change(run, f"Press Enter, then UNPLUG the USB microphone ({node}); nota is recording from it.", "sources", node)
        return None

    run.change("USB microphone unplugged", act, lambda w: w[0] == "lost" and w[1] == "mic", watch)
    session, events, anchor, tail = run.finish()
    run.check_changes(events, anchor, {"device:lost"}, TRACK_OF["mic"], "the microphone was lost")
    check_audio(ctx, run, session, events, TRACK_OF["mic"], anchor, tones, [[node]], lost=True)
    run.check_exit()
    if not ctx.private:
        await_present(node, "sources")


def case_usb_mic_default(ctx):
    args, report, fx = ctx.args, ctx.report, ctx.fixtures
    report.say("== usb-mic-default: the microphone track follows the default input, a USB microphone that is unplugged")
    fx.null_sink(fixture_name("quiet_sys"))
    tones = {}
    other = None
    if ctx.private:
        node, other = fixture_name("usb_mic"), fixture_name("builtin_mic")
        for name in (node, other):
            fx.usb_mic(name)
        pactl("set-default-source", node)
        wait_for(lambda: pactl("get-default-source") == node, "the default input to become usb_mic")
        fx.play(500, f"{node}_feed", 8000)
        fx.play(1500, f"{other}_feed", 8000)
        tones = {node: 500, other: 1500}
    else:
        node = args.usb_mic
        await_present(node, "sources")
        have = pactl("get-default-source")
        if have != node:
            report.say(f"  not run: the default input is {have}, not {node}; make the USB microphone the default (this script changes no default input) and run it again")
            ctx.skipped.append("usb-mic-default")
            return
    watch = [("source", device_index("sources", node))]
    run = Run(ctx, "usb-mic-default", ["--system", fixture_name("quiet_sys")])
    run.settle()

    def act():
        if ctx.private:
            t = time.monotonic()
            fx.remove(node)
            return t
        user_change(run, f"Press Enter, then UNPLUG the USB microphone ({node}), the default input nota records.", "sources", node)
        return None

    run.change("default USB microphone unplugged", act, lambda w: w[1] == "mic", watch)
    now_default = pactl("get-default-source", check=False)
    session, events, anchor, tail = run.finish()
    run.check_changes(events, anchor, {"device:lost", "device:changed"}, TRACK_OF["mic"], "the microphone was lost or moved")
    follow_through(ctx, events, TRACK_OF["mic"], node, now_default, "sources")
    if last_device_kind(events, TRACK_OF["mic"]) == "device:changed":
        expected = [[node, other]]
    else:
        expected = [[node]]
    check_audio(ctx, run, session, events, TRACK_OF["mic"], anchor, tones, expected)
    run.check_exit()
    if not ctx.private:
        await_present(node, "sources")


def is_new_default(old, old_id, now_default, now_id):
    """Whether the default after the change is another device than `old`: a
    different name, or the same name on a new node (a Bluetooth profile change
    brings the device back under its old name), told by the server's index."""
    if not now_default:
        return False
    return now_default != old or (old_id is not None and now_id is not None and now_id != old_id)


def follow_through(ctx, events, track, old, now_default, listing, old_id=None):
    """Where the track ended up is where the server's default is: the last
    device the timeline logged for it is the default after the change, or,
    if it was lost for good, no default is left (or only the old one).
    `old_id` is the old device's index on the server, for a device that
    comes back under the same name."""
    report = ctx.report
    last = [e for e in events if e[1] == track and e[2] in ("device:changed", "device:lost")]
    if not last:
        return
    if last[-1][2] == "device:changed":
        new = description(now_default, listing)
        report.check(
            is_new_default(old, old_id, now_default, device_index(listing, now_default) if now_default else None)
            and names_it(last[-1][3], now_default, new),
            f"the last device logged, {last[-1][3]!r}, is the default after the change ({now_default or 'none'})",
        )
    else:
        report.check(
            not now_default or not is_new_default(old, old_id, now_default, device_index(listing, now_default)),
            f"the track was lost and the server's default afterwards is {now_default or 'none'}, not another device the track missed",
        )


def case_bluetooth(ctx):
    args, report, fx = ctx.args, ctx.report, ctx.fixtures
    report.say("== bluetooth: the system audio follows a default output whose profile changes")
    fx.null_sink(fixture_name("quiet_mic"))
    tones = {}
    new = None
    if ctx.private:
        # A profile change takes the sink away and brings a new one: here,
        # two sinks of different names, the second made the default.
        node, new = fixture_name("bt_a2dp"), fixture_name("bt_hfp")
        fx.null_sink(node)
        pactl("set-default-sink", node)
        wait_for(lambda: ctx.default_sink() == node, "the default to become bt_a2dp")
        fx.play(500, node, 8000)
        tones = {node: 500, new: 1900}
    else:
        node = args.bt_sink
        have = ctx.default_sink()
        if have != node:
            report.say(f"  not run: the default output is {have}, not {node}; make the Bluetooth device the default output (this script changes no default unless asked) and run it again")
            ctx.skipped.append("bluetooth")
            return
    old_id = device_index("sinks", node)
    watch = [("sink", old_id)]
    run = Run(ctx, "bluetooth", ["--mic", fixture_name("quiet_mic")])
    run.settle()

    def act():
        if ctx.private:
            t = time.monotonic()
            fx.remove(node)
            fx.null_sink(new)
            pactl("set-default-sink", new)
            fx.play(1900, new, 8000)
            return t
        user_change(run, f"Press Enter, then CHANGE the Bluetooth profile of {node} (headset / handsfree, or back).", "sinks", node)
        return None

    run.change("Bluetooth profile changed", act, lambda w: w[1] == "system", watch)
    now_default = ctx.default_sink()
    session, events, anchor, tail = run.finish()
    run.check_changes(events, anchor, {"device:lost", "device:changed"}, TRACK_OF["system"], "the system audio was lost or moved")
    follow_through(ctx, events, TRACK_OF["system"], node, now_default, "sinks", old_id)
    # The track may be moved to the sink the session manager falls back to
    # (silent here) before the new node: both are recorded, in order.
    if last_device_kind(events, TRACK_OF["system"]) == "device:changed":
        expected = [[node, new], [node, "silence", new]]
    else:
        expected = [[node], [node, "silence"]]
    check_audio(ctx, run, session, events, TRACK_OF["system"], anchor, tones, expected)
    run.check_exit()
    if not ctx.private:
        confirm(f"Change the Bluetooth profile of {node} back to what it was (this script never changes it).")


CASES = {
    "default-output": case_default_output,
    "pinned-removed": case_pinned_removed,
    "usb-mic-pinned": case_usb_mic_pinned,
    "usb-mic-default": case_usb_mic_default,
    "bluetooth": case_bluetooth,
}


def why_not(args, case):
    """Why a case can't run with these options, or None."""
    if args.mode == "private":
        return None
    if case == "default-output":
        if not args.allow_default_switch or not args.other_sink:
            return "it switches the default output: pass --other-sink NODE and --allow-default-switch"
    if case.startswith("usb-mic") and not args.usb_mic:
        return "it needs the USB microphone's node: pass --usb-mic NODE"
    if case == "bluetooth" and not args.bt_sink:
        return "it needs the Bluetooth device's sink: pass --bt-sink NODE"
    return None


def main(argv):
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("mode", choices=["private", "real"])
    p.add_argument("work")
    p.add_argument("nota")
    p.add_argument("--cases", default=",".join(CASES))
    p.add_argument("--prefix", default="ndc_")
    p.add_argument("--manual-wait", type=float, default=120.0)
    p.add_argument("--switches", type=int, default=4)
    p.add_argument("--other-sink")
    p.add_argument("--usb-mic")
    p.add_argument("--bt-sink")
    p.add_argument("--allow-default-switch", action="store_true")
    p.add_argument("--tone", action="store_true")
    args = p.parse_args(argv)
    global PREFIX
    PREFIX = args.prefix
    wanted = [c for c in args.cases.split(",") if c]
    unknown = [c for c in wanted if c not in CASES]
    if unknown:
        raise SystemExit(f"device-changes: unknown case {unknown}; the cases are {list(CASES)}")
    if args.switches < 1:
        raise SystemExit("device-changes: --switches is at least 1")
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))

    report = Report(os.path.join(args.work, "report.txt"))
    try:
        return run_cases(args, report, wanted)
    finally:
        report.close()


def run_cases(args, report, wanted):
    """Runs the cases, writes the verdict to `report` and returns the exit
    status."""
    server = Server()
    fixtures = Fixtures(args.work)
    ctx = Context(args, report, server, fixtures)
    ctx.skipped = []
    report.say(f"device-changes ({args.mode}): {server_name()}")
    try:
        for case in wanted:
            reason = why_not(args, case)
            if reason:
                report.say(f"== {case}: not run: {reason}")
                ctx.skipped.append(case)
                continue
            try:
                CASES[case](ctx)
            finally:
                fixtures.close()
    finally:
        for run in ctx.runs:
            run.nota.reap()
        fixtures.close()
        server.close()
    report.say()
    if ctx.skipped:
        report.say(f"NOT RUN: {', '.join(ctx.skipped)}")
    if report.failed:
        report.say(f"FAILED ({len(report.failed)}):")
        for text in report.failed:
            report.say(f"  - {text}")
        return 1
    ran = len(wanted) - len(ctx.skipped)
    if ran == 0 or ctx.skipped:
        report.say(f"INCOMPLETE: {ran} case(s) passed, {len(ctx.skipped)} not run")
        return 3
    report.say(f"PASSED: {ran} case(s) shown within {WITHIN_S:g} s and logged")
    return 0


def server_name():
    for line in pactl("info", check=False).splitlines():
        if line.startswith("Server String:"):
            return line.split(":", 1)[1].strip()
    return "?"


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
