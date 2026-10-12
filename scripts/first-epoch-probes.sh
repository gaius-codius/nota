#!/usr/bin/env bash
# The probes of a track's first epoch that need the user at the machine, for
# the T4 sitting (GAI-327): a hardware microphone, a suspend and a resume,
# and a default output moved on real hardware. Each runs the `first-epoch`
# mode of the `real_capture` example on the user's own PipeWire, which
# prints the delay profile PipeWire gave the stream (`buffer` and `profile`
# lines), what the recorder did (`epoch` and `notice` lines) and whether the
# first epoch sits where the first settled buffer puts it (`result`).
#
# This script changes no device, default or volume. It only records from
# the default input or the default output's monitor; you make the move, the
# suspend and the speech yourself, when it says. Record from the default
# output's monitor only with something playing (music in a browser will do).
#
#   mic      records the default microphone for S seconds (default 10,
#            at least 6).
#            Speak, or tap the microphone, from the start. Reports whether
#            the delay ever reaches a buffer's length, which is what the
#            recorder's hold waits for: a microphone whose delay never does
#            gives its first epoch from the zero-delay rule after 500 ms.
#   resume   records for S seconds (default 60, at least 40). Suspend the machine
#            (`systemctl suspend`) for 10 seconds or more, between 10 s and
#            S - 20 s in, and wake it. Reports the profile after the resume:
#            whether the delay goes back to zero, and the epoch the resume
#            opens.
#   move     records the default output's monitor for S seconds (default
#            30, at least 30), with something playing. Once, between 10 s and S - 10 s in,
#            switch the default output (to the headphones, say) the way you
#            usually do, and back afterwards if you like. Reports whether
#            the delay goes back to zero after the stream follows it, and
#            the epoch it opens.
#
# What to bring back: the file each run writes (the path is printed), pasted
# on GAI-477. The lines that matter are `profile` (runs of buffers with the
# same length and delay, with when each began), `epoch` and `notice`.
#
# Not run in CI: it needs a running PipeWire and the audio hardware.
#
# Usage: scripts/first-epoch-probes.sh mic|resume|move [--seconds S]
#          [--source NODE] [--out DIR]
#   --source NODE  the cpal node to record (default: input_default for
#                  mic and resume, sink_default for move)
#   --out DIR      where the output goes (default ~/.cache/nota/first-epoch-probes)
set -euo pipefail

mode=${1:-}
[[ $mode =~ ^(mic|resume|move)$ ]] || {
  sed -n '2,/^set -/p' "$0" | sed '$d; s/^# \{0,1\}//' >&2
  exit 2
}
shift

case $mode in
  mic) seconds=10 source=input_default ;;
  resume) seconds=60 source=input_default ;;
  move) seconds=30 source=sink_default ;;
esac
out=${XDG_CACHE_HOME:-$HOME/.cache}/nota/first-epoch-probes
while [[ $# -gt 0 ]]; do
  case $1 in
    --seconds) seconds=$2; shift 2 ;;
    --source) source=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
# The least that leaves room for what each probe asks of the user.
case $mode in
  mic) least=6 ;;
  move) least=30 ;;
  resume) least=40 ;;
esac
[[ $seconds =~ ^[0-9]+$ && $seconds -ge $least ]] || {
  echo "first-epoch-probes: --seconds needs a number, at least $least for $mode" >&2
  exit 2
}

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cargo build --manifest-path "$repo/Cargo.toml" --release --locked --quiet \
  -p nota-recorder --example real_capture
bin=${CARGO_TARGET_DIR:-$repo/target}/release/examples/real_capture
[[ -x $bin ]] || { echo "first-epoch-probes: built, but no binary at $bin" >&2; exit 2; }

mkdir -p "$out"
work=$(mktemp -d "${TMPDIR:-/tmp}/nota-first-epoch-probe.XXXXXX")
trap 'rm -rf "$work"' EXIT
file=$out/$mode-$(date +%Y%m%d-%H%M%S).txt

case $mode in
  mic) echo "Recording $source for $seconds s. Speak now." ;;
  resume) echo "Recording $source for $seconds s. Suspend the machine for 10 s or more, between 10 s and $((seconds - 20)) s from now, and wake it." ;;
  move) echo "Recording $source for $seconds s. Play something, and between 10 s and $((seconds - 10)) s from now switch the default output." ;;
esac

status=0
"$bin" first-epoch "$work" --seconds "$seconds" --source "$source" | tee "$file" || status=$?
echo "Output: $file (exit $status; 0 means the first epoch sits where the settled stamps put it)"
exit "$status"
