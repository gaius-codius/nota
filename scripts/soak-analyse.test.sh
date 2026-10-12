#!/usr/bin/env bash
# Tests soak-analyse.py's lag check against latency logs written here: a
# run whose p99 is within 2 s passes it, and its highest is named (as over
# the bound, not judged, when it is), from a minute's highest when that
# passes the log's 5 s cap; a p99 past 2 s fails, one at 2 s doesn't; and a
# log sampled less than half the run fails.
#
# Requires: bash, python3.
set -euo pipefail

scripts=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT

# -I: nothing from the current or the script's directory is imported; -B:
# no bytecode is written beside the analyser.
python3 -I -B - "$scripts/soak-analyse.py" "$dir" <<'PY'
import contextlib
import importlib.util
import io
import os
import sys

spec = importlib.util.spec_from_file_location("soak_analyse", sys.argv[1])
soak = importlib.util.module_from_spec(spec)
spec.loader.exec_module(soak)
work = sys.argv[2]
failures = []


def run(p99, peak, samples=3000, play_to_stop_s=100, minute_peak=None, minutes=True):
    """The report's lines and whether it failed, for track 0's lag line,
    whose highest minute is `minute_peak` (`peak` unless given), or with
    every minute unsampled unless `minutes`."""
    with open(os.path.join(work, "latency.tsv"), "w") as f:
        f.write(f"lag\t0\tsamples={samples}\tp50_ms=400\tp95_ms=800\tp99_ms={p99}\t"
                f"max_ms={peak}\tqueued_max_ms=40\n")
        if minutes:
            f.write(f"lagmin\t0\t0\t{min(peak, 400)}\nlagmin\t0\t1\t-\n"
                    f"lagmin\t0\t17\t{peak if minute_peak is None else minute_peak}\n")
        else:
            f.write("lagmin\t0\t0\t-\n")
    report = soak.Report()
    with contextlib.redirect_stdout(io.StringIO()):
        soak.lag(report, work, 0, {"latency_log": "1", "play_to_stop_s": str(play_to_stop_s)})
    return report.lines, report.failed


def expect(name, ok):
    if not ok:
        failures.append(name)


# The 3-hour soak's shape: p99 856 ms, one peak of 2,392 ms.
lines, failed = run(856, 2392)
expect(f"a p99 within the bound with a higher peak failed: {lines}", not failed)
expect(f"the lag check didn't pass on its p99: {lines}",
       any(line.startswith("lag (delivered") and "p99 856" in line and line.endswith(" pass") for line in lines))
expect(f"the highest wasn't named as over the bound: {lines}",
       "  highest 2392 ms, over 2000 ms: reported, not judged" in lines)

# The log's highest stops at 5 s; the minute's keeps the whole 8 s.
lines, failed = run(856, 5000, minute_peak=8000)
expect(f"a peak past the log's cap wasn't named whole: {lines}", not failed
       and "  highest 8000 ms, over 2000 ms: reported, not judged" in lines)

# A log whose minutes went unsampled still names its highest.
lines, failed = run(856, 900, minutes=False)
expect(f"a log with no minute sampled lost its highest: {lines}", not failed and "  highest 900 ms" in lines)

lines, failed = run(2001, 2392)
expect(f"a p99 past the bound passed: {lines}", failed)
expect(f"a p99 past the bound wasn't the check that failed: {lines}",
       any(line.startswith("lag (delivered") and line.endswith(" FAIL") for line in lines))

lines, failed = run(2000, 2000)
expect(f"a p99 at the bound failed: {lines}", not failed)
expect(f"a highest at the bound was called over it: {lines}", "  highest 2000 ms" in lines)

# 100 s sampled every 20 ms is 5,000 samples; at least half are wanted.
lines, failed = run(500, 500, samples=2499)
expect(f"a log sampled under half the run passed: {lines}", failed)
lines, failed = run(500, 500, samples=2500)
expect(f"a log sampled for half the run failed: {lines}", not failed)

for failure in failures:
    print(f"soak-analyse.test.sh: {failure}", file=sys.stderr)
sys.exit(1 if failures else 0)
PY
