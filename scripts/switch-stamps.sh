#!/usr/bin/env bash
# The raw stamps of the system audio's stream across switches of the
# default output (GAI-492): which of a buffer's two stamps, its cycle or
# its capture stamp, breaks from the old device's run when the default
# moves, and by how much. The recorder opens a route change's epoch at the
# first buffer after the report where one does by 4 ms or more, and failing
# that 200 ms after the report (GAI-434); this shows which it was.
#
# It runs the `first-epoch` mode of the `real_capture` example on the
# default output's monitor (`sink_default`), switches the default output
# --switches times between two sinks, --gap seconds apart, and prints, for
# each route change the stream reported:
#   stamp    the buffers around the report, one per line: the length, the
#            cycle and capture stamps in ms after the report, the delay,
#            `jump cycle C capture P` (how far each stamp is from where the
#            buffer before it puts it, in ms), and `raw` (both stamps on
#            CLOCK_MONOTONIC, in ns). `<- first after the report` and
#            `<- reopens` mark those buffers.
#   switch   which stamp broke (cycle, capture or both), at which buffer,
#            how long after the report and by how much; or that neither
#            broke within 200 ms and the wait reopened; and where the epoch
#            the recorder opened starts, from the report.
#   switches the count of each.
# The terminal shows the `switch` and `switches` lines; the whole output
# (with each buffer's delay profile, the epochs and the notices) is in the
# file it names. The `result` line about first epochs is the first-epoch
# check, which a hardware monitor fails for a reason of its own (GAI-489):
# it is not this measurement and doesn't set the exit status.
#
# Two modes:
#   private  (the default) Runs on a private PipeWire, WirePlumber and
#            pipewire-pulse with no hardware, in a temporary runtime
#            directory of its own, as scripts/real-capture-first-epoch.sh
#            does: nothing of the user's is touched. The two sinks are null
#            sinks made in that instance, each with a tone playing into it.
#            It shows that the harness works, not how hardware behaves.
#   real     Runs on the user's own PipeWire, with the user at the
#            terminal, and says what it will do and waits for Enter first.
#            It changes only these, and puts each back on exit, whatever
#            happens (an error, Ctrl-C):
#              - the default output, switched between the current default
#                and --other-sink, only with --allow-default-switch. Anything
#                playing moves with it, as when you switch it yourself.
#                `pactl set-default-sink` also writes WirePlumber's
#                configured default (`default.configured.audio.sink`), so
#                that is saved first and put back as it was, or deleted if
#                there was none. The script says so if it had to put either
#                back, and fails if it couldn't.
#                WirePlumber also adds --other-sink to its list of outputs
#                chosen before (`default.configured.audio.sink.0` in
#                ~/.local/state/wireplumber/default-nodes), as a switch by
#                hand does, and this can't be undone from outside it. The
#                list is read only when the chosen output is missing:
#                WirePlumber then falls back to the latest one there.
#              - with --tone, a quiet tone (about -35 dBFS before the
#                sink's volume, and any volume WirePlumber restores for
#                `pw-play`'s streams: 500 Hz into the current default,
#                1500 Hz into the other) plays for the run, so both outputs carry
#                audio as in GAI-492's first hardware run. Without it, play
#                something yourself (music will do): a monitor with nothing
#                playing shows little of the delay.
#            No volume is changed, and no sink or source is made. The
#            players and the recorder are stopped by their PIDs. A run killed
#            with SIGKILL can't put the default back: the saved one is
#            printed at the start, to set by hand.
#
# Exit status: 0 when the stream reported at least --switches route
# changes and each was measured; 1 when it reported fewer, or a report's
# buffers ran out before its reopening (the run ended, or the harness's
# cap of 60,000 buffers was reached), or a default couldn't be put back;
# 2 for a usage or set-up error. More reports than switches is said, not
# failed: a switch reported twice shows as two `switch` lines.
#
# Not run in CI: it needs PipeWire 1.x, WirePlumber 0.5 (private),
# pipewire-pulse, `pactl`, `pw-play`, `pw-link`, `pw-metadata` (real) and
# python3 (to write the tones).
#
# Usage: scripts/switch-stamps.sh [private|real] [--switches N] [--gap S]
#          [--other-sink NODE] [--allow-default-switch] [--tone] [--out DIR]
#   --switches N   default-output switches (default 8; even ends where it
#                  started)
#   --gap S        seconds between switches (default 3, at least 2, so each
#                  switch's reopening (200 ms at most) and the hold of the
#                  new device's first buffers (500 ms at most) are over
#                  before the next)
#   --other-sink   real: the sink to switch to (a name from
#                  `pactl list short sinks`); the current default is the
#                  other one
#   --allow-default-switch   real: the user's OK to switch the default output
#   --tone         real: play the quiet tones into both outputs
#   --out DIR      where the output file goes
#                  (default ~/.cache/nota/switch-stamps)
set -euo pipefail

mode=private
if [[ ${1:-} == private || ${1:-} == real ]]; then
  mode=$1
  shift
fi
switches=8
gap=3
other=
allow=0
tone=0
out=${XDG_CACHE_HOME:-$HOME/.cache}/nota/switch-stamps
while [[ $# -gt 0 ]]; do
  case $1 in
    --switches) switches=${2:?}; shift 2 ;;
    --gap) gap=${2:?}; shift 2 ;;
    --other-sink) other=${2:?}; shift 2 ;;
    --allow-default-switch) allow=1; shift ;;
    --tone) tone=1; shift ;;
    --out) out=${2:?}; shift 2 ;;
    -h | --help) sed -n '2,/^set -/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

die() { echo "switch-stamps: $*" >&2; exit 2; }

[[ $switches =~ ^[0-9]+$ && $switches -ge 1 ]] || die "--switches needs a number, at least 1"
[[ $gap =~ ^[0-9]+$ && $gap -ge 2 ]] || die "--gap needs a number, at least 2"
if [[ $mode == real ]]; then
  [[ $allow -eq 1 ]] || die "real mode switches the default output: add --allow-default-switch"
  [[ -n $other ]] || die "real mode needs --other-sink (a name from 'pactl list short sinks')"
fi

# pactl's words are translated; this parses the English ones.
export LC_ALL=C

for c in python3 pactl pw-play pw-link; do
  command -v "$c" >/dev/null || die "$c is required"
done
if [[ $mode == private ]]; then
  for c in pipewire wireplumber pipewire-pulse; do
    command -v "$c" >/dev/null || die "$c is required"
  done
else
  command -v pw-metadata >/dev/null || die "pw-metadata is required in real mode (it keeps the configured default)"
fi

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# The interpreter itself, found before a private instance's environment is
# set: a version manager's shim reads the user's config and state from the
# directories that environment moves.
python=$(python3 -I -c 'import sys; print(sys.executable)')
# Built before a private instance's environment is set, so the build sees
# the user's own.
cargo build --manifest-path "$repo/Cargo.toml" --release --locked --quiet \
  -p nota-recorder --example real_capture
bin=${CARGO_TARGET_DIR:-$repo/target}/release/examples/real_capture
[[ -x $bin ]] || die "built, but no binary at $bin"

mkdir -p "$out"
file=$out/$mode-$(date +%Y%m%d-%H%M%S).txt
work=$(mktemp -d "${TMPDIR:-/tmp}/nota-switch-stamps.XXXXXX")
pids=()
players=()
recorder=
saved=0
saved_sink=
saved_conf_sink=

# WirePlumber keeps the default the user chose (`configured`) apart from the
# one in effect. `pactl set-default-sink` writes the configured one, so it's
# saved and put back as it was, or deleted if there was none (as
# scripts/device-changes.sh does).
configured() { pw-metadata -n default 2>/dev/null | sed -n "s/.*key:'$1' value:'\(.*\)' type:'\(.*\)'.*/\1\t\2/p"; }

restore_configured() {
  local key=$1 saved=$2 now value type
  now=$(configured "$key")
  [[ $now == "$saved" ]] && return 0
  if [[ -z $saved ]]; then
    pw-metadata -n default -d 0 "$key" >/dev/null || return 1
  else
    value=${saved%%$'\t'*}
    type=${saved#*$'\t'}
    pw-metadata -n default 0 "$key" "$value" "$type" >/dev/null || return 1
  fi
  echo "switch-stamps: $key was changed; put back" >&2
}

# Puts the user's default output back, and the configured one after it,
# since `set-default-sink` writes that too. Fails if either can't be.
restore_default() {
  local sink failed=0
  sink=$(pactl get-default-sink 2>/dev/null || true)
  if [[ -n $saved_sink && $sink != "$saved_sink" ]]; then
    if pactl set-default-sink "$saved_sink" 2>/dev/null; then
      echo "switch-stamps: the default output was $sink; put back to $saved_sink" >&2
    else
      echo "switch-stamps: the default output is $sink and could NOT be put back to $saved_sink: set it yourself" >&2
      failed=1
    fi
  fi
  restore_configured default.configured.audio.sink "$saved_conf_sink" || failed=1
  return "$failed"
}

stop_players() {
  for pid in ${players[@]+"${players[@]}"}; do
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  done
  players=()
}

failed_restore=0
cleanup() {
  local rc=$?
  # The recorder and the players are stopped by their PIDs, then the
  # default is put back, then any private instance is stopped.
  [[ -z $recorder ]] || kill "$recorder" 2>/dev/null || true
  stop_players
  # Only once both were saved: an empty saved value would delete the
  # user's configured default.
  if [[ $saved -eq 1 ]]; then
    restore_default || failed_restore=1
  fi
  # Written so an empty list is no error under `set -u` in bash 3.2.
  for pid in ${pids[@]+"${pids[@]}"}; do
    kill "$pid" 2>/dev/null || true
  done
  for pid in ${pids[@]+"${pids[@]}"}; do
    wait "$pid" 2>/dev/null || true
  done
  rm -rf "$work"
  if [[ $failed_restore -ne 0 ]]; then
    echo "switch-stamps: the default output could not be put back" >&2
    [[ $rc -ne 0 ]] || exit 1
  fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# start_private: a PipeWire, WirePlumber and pipewire-pulse of the script's
# own, with no hardware, in $work; pactl then talks to it alone.
start_private() {
  # The no-hardware config below is WirePlumber 0.5's; 0.4 would ignore it
  # and open the user's real devices.
  local wp_version wp_major wp_minor server
  wp_version=$(wireplumber --version | sed -n 's/.*libwireplumber \([0-9]*\)\.\([0-9]*\).*/\1 \2/p')
  read -r wp_major wp_minor <<<"${wp_version:-0 0}"
  if [ "$wp_major" -eq 0 ] && [ "$wp_minor" -lt 5 ]; then
    die "needs WirePlumber 0.5 or later"
  fi
  export XDG_RUNTIME_DIR=$work/run XDG_CONFIG_HOME=$work/config \
    XDG_STATE_HOME=$work/state XDG_DATA_HOME=$work/data
  # Nothing may point at the user's servers or session bus, nor start a bus
  # of its own that would outlive the script (D-Bus autolaunches one from a
  # display).
  unset PIPEWIRE_REMOTE PIPEWIRE_RUNTIME_DIR PULSE_SERVER PULSE_RUNTIME_PATH \
    DISPLAY WAYLAND_DISPLAY
  # Nor may a config directory set elsewhere take the place of the
  # no-hardware config written below.
  unset WIREPLUMBER_CONFIG_DIR WIREPLUMBER_DATA_DIR PIPEWIRE_CONFIG_DIR \
    PIPEWIRE_CONFIG_PREFIX PIPEWIRE_CONFIG_NAME
  export DBUS_SESSION_BUS_ADDRESS=disabled:
  mkdir -p -m 700 "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d" \
    "$XDG_STATE_HOME" "$XDG_DATA_HOME"
  cat >"$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d/90-no-hardware.conf" <<'EOF'
wireplumber.profiles = {
  main = {
    monitor.alsa = disabled
    monitor.alsa-midi = disabled
    monitor.bluez = disabled
    monitor.bluez-midi = disabled
    monitor.v4l2 = disabled
    monitor.libcamera = disabled
  }
}
EOF
  pipewire >"$work/pipewire.log" 2>&1 &
  pids+=($!)
  for _ in $(seq 50); do
    [ -S "$XDG_RUNTIME_DIR/pipewire-0" ] && break
    sleep 0.1
  done
  [ -S "$XDG_RUNTIME_DIR/pipewire-0" ] || die "the private PipeWire didn't start"
  wireplumber >"$work/wireplumber.log" 2>&1 &
  pids+=($!)
  pipewire-pulse >"$work/pipewire-pulse.log" 2>&1 &
  pids+=($!)
  for _ in $(seq 50); do
    pactl info >/dev/null 2>&1 && break
    sleep 0.1
  done
  # pactl must be talking to this instance, which has no hardware.
  server=$(pactl info | sed -n 's/^Server String: //p')
  case $server in
    "$XDG_RUNTIME_DIR"/*) ;;
    *) die "pactl reached $server, not the private instance" ;;
  esac
  if pactl list short sinks | grep -v auto_null | grep -q . ||
    pactl list short sources | grep -v '\.monitor' | grep -q .; then
    die "the private instance has devices of its own: $(pactl list short sinks)"
  fi
  pactl load-module module-null-sink sink_name=switch_a >/dev/null
  pactl load-module module-null-sink sink_name=switch_b >/dev/null
  pactl set-default-sink switch_a
}

# wait_for_default SINK: waits for the default WirePlumber keeps to show
# SINK.
wait_for_default() {
  for _ in $(seq 50); do
    [[ $(pactl get-default-sink) == "$1" ]] && return 0
    sleep 0.1
  done
  return 1
}

# sink_names: the sinks the server lists, one per line.
sink_names() { pactl list short sinks | cut -f2; }

# play_tone HZ SINK SECONDS: a quiet tone into SINK, linked by hand: a
# stream the session manager places follows the default when it moves,
# and this one must stay on its sink. The player's PID is kept to stop it
# by.
play_tone() {
  local hz=$1 sink=$2 seconds=$3 wav name ports
  wav=$work/tone-$hz.wav
  "$python" -I - "$wav" "$hz" "$seconds" <<'EOF'
import math, struct, sys, wave
path, hz, seconds = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
rate, amplitude = 16000, 600  # about -35 dBFS
second = struct.pack(f"<{rate}h", *(int(amplitude * math.sin(2 * math.pi * hz * i / rate)) for i in range(rate)))
with wave.open(path, "wb") as w:
    w.setnchannels(1)
    w.setsampwidth(2)
    w.setframerate(rate)
    w.writeframes(second * seconds)
EOF
  name="switch_stamps_tone_$hz"
  pw-play --target 0 -P "node.name=$name" "$wav" >/dev/null 2>&1 &
  players+=($!)
  for _ in $(seq 100); do
    pw-link -o | grep -x "$name:output_MONO" >/dev/null && break
    sleep 0.05
  done
  # The sink's name is matched as it is: ALSA names hold dots.
  ports=$(pw-link -i | awk -v p="$sink:playback_" 'index($0, p) == 1')
  [[ -n $ports ]] || die "$sink has no playback ports to play a tone into"
  while read -r port; do
    pw-link "$name:output_MONO" "$port"
  done <<<"$ports"
}

if [[ $mode == private ]]; then
  start_private
  first=switch_a
  other=switch_b
  wait_for_default "$first" || die "the default sink never became $first"
  # The null sinks carry audio only if something plays into them.
  tone=1
else
  pactl info >/dev/null 2>&1 || die "no PipeWire (pipewire-pulse) to talk to"
  [[ -t 0 ]] || die "real mode needs a terminal: it asks before it changes anything"
  first=$(pactl get-default-sink)
  [[ -n $first ]] || die "there is no default output to switch from"
  sink_names | grep -x "$other" >/dev/null || die "no sink named $other: see 'pactl list short sinks'"
  [[ $other != "$first" ]] || die "$other is already the default output: name the other sink"
  saved_sink=$first
  saved_conf_sink=$(configured default.configured.audio.sink)
  saved=1
  echo "switch-stamps: default output saved: $saved_sink (if this run is killed, put it back with: pactl set-default-sink $saved_sink)"
  echo "switch-stamps: configured default saved: ${saved_conf_sink%%$'\t'*}"
  echo
  echo "This switches the default output $switches times, $gap s apart, between"
  echo "  $first and $other,"
  echo "records the default output's monitor for the run, and puts the default back."
  if [[ $tone -eq 1 ]]; then
    echo "A quiet tone plays into both outputs for the run (no volume is changed)."
  else
    echo "Play something to the default output now (music will do) and leave it playing."
  fi
  read -r -p "Press Enter to start, or Ctrl-C to stop: " _
fi

lead=3
seconds=$((lead + switches * gap + 3))
if [[ $tone -eq 1 ]]; then
  play_tone 500 "$first" $((seconds + 10))
  play_tone 1500 "$other" $((seconds + 10))
fi

{
  echo "# switch-stamps $mode, $(date -Iseconds), $switches switches $gap s apart"
  echo "# first $first, other $other, tone $tone"
} >"$file"

mkdir "$work/recording"
"$bin" first-epoch "$work/recording" --seconds "$seconds" --source sink_default \
  --tracks 1 --ready "$work/ready" >"$work/out.txt" &
recorder=$!
for _ in $(seq 1000); do
  [[ -e $work/ready ]] && break
  kill -0 "$recorder" 2>/dev/null || break
  sleep 0.01
done
[[ -e $work/ready ]] || die "the recorder never said it was running: $(cat "$work/out.txt")"

sleep "$lead"
from=$first
to=$other
for k in $(seq "$switches"); do
  pactl set-default-sink "$to"
  echo "# switch $k: $from to $to at $(date +%T.%N)" >>"$file"
  echo "switch-stamps: switch $k of $switches: $to"
  read -r from to <<<"$to $from"
  sleep "$gap"
done

status=0
wait "$recorder" || status=$?
recorder=
cat "$work/out.txt" >>"$file"
echo
grep -E '^switch(es)? ' "$work/out.txt" || true
echo "Output: $file"
# 0 and 1 are a finished run (1 is the first-epoch check, not this one).
[[ $status -le 1 ]] || { echo "switch-stamps: the recorder failed (exit $status)" >&2; exit 1; }
reported=$(grep -c '^switch 0 ' "$work/out.txt" || true)
unmeasured=$(grep '^switch 0 ' "$work/out.txt" | grep -c 'no reopening' || true)
if [[ $reported -lt $switches ]]; then
  echo "switch-stamps: the stream reported $reported route changes for $switches switches" >&2
  exit 1
fi
if [[ $unmeasured -gt 0 ]]; then
  echo "switch-stamps: $unmeasured of the reports ran out of buffers before their reopening" >&2
  exit 1
fi
[[ $reported -eq $switches ]] ||
  echo "switch-stamps: more reports than switches: a switch was reported twice (see the switch lines in $file)" >&2
echo "switch-stamps: $reported route changes reported for $switches switches"
