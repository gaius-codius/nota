#!/usr/bin/env bash
# A track's first epoch against the stamps PipeWire gave its first buffers
# (the `first-epoch` mode of the `real_capture` example): the recorder holds
# a stream's first buffers until a buffer's delay is at least its length and
# dates them back from it, so the first epoch must start at the first
# settled buffer's stamp less the samples before it, within a sample.
#
# Runs on a private PipeWire, WirePlumber and pipewire-pulse with no
# hardware, in a temporary runtime directory of its own, with its own
# config and state, as scripts/pipewire-devices.sh does: the user's devices,
# defaults and volumes, and any recording on the user's own PipeWire, are
# never touched. Two null sinks, A and B, live only in that instance.
# Everything is stopped on exit, by PID, and the directory removed.
#
# Three runs, each printing the first buffers and the whole delay profile
# (`buffer` and `profile` lines) and one `result` line:
#   one      one track on sink A's monitor.
#   two      two tracks on it, the second joining as the first records.
#   move     one track following the default sink, which is switched from A
#            to B partway through: the profile shows what PipeWire's delay
#            does after a move.
# The first epoch is checked in each. Exit status 1 if any fails.
#
# A hardware microphone, and a suspend and resume, can't be run on a private
# instance (see scripts/first-epoch-probes.sh).
#
# Not run in CI: it needs PipeWire 1.x, WirePlumber 0.5, pipewire-pulse and
# `pactl`. Run it by hand after changing capture.
#
# Usage: scripts/real-capture-first-epoch.sh [--seconds S]
#   --seconds S   recording length per run (default 8; the move run
#                 switches the default 3 s in)
set -euo pipefail

seconds=8
while [[ $# -gt 0 ]]; do
  case $1 in
    --seconds) seconds=$2; shift 2 ;;
    -h | --help) sed -n '2,/^set -/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ $seconds =~ ^[0-9]+$ && $seconds -ge 6 ]] || {
  echo "real-capture-first-epoch: --seconds needs a number, at least 6" >&2
  exit 2
}

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/nota-first-epoch.XXXXXX")
pids=()
recorder=
players=()

cleanup() {
  # The recorder and the players are stopped by their PIDs while they run.
  [[ -z $recorder ]] || kill "$recorder" 2>/dev/null || true
  stop_players
  # Written so an empty list is no error under `set -u` in bash 3.2.
  for pid in ${pids[@]+"${pids[@]}"}; do
    kill "$pid" 2>/dev/null || true
  done
  for pid in ${pids[@]+"${pids[@]}"}; do
    wait "$pid" 2>/dev/null || true
  done
  rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# The no-hardware config below is WirePlumber 0.5's; 0.4 would ignore it
# and open the user's real devices.
wp_version=$(wireplumber --version | sed -n 's/.*libwireplumber \([0-9]*\)\.\([0-9]*\).*/\1 \2/p')
read -r wp_major wp_minor <<<"${wp_version:-0 0}"
if [ "$wp_major" -eq 0 ] && [ "$wp_minor" -lt 5 ]; then
  echo "real-capture-first-epoch: needs WirePlumber 0.5 or later" >&2
  exit 1
fi

# Built before the private instance's environment is set, so the build sees
# the user's own.
cargo build --manifest-path "$repo/Cargo.toml" --release --locked --quiet \
  -p nota-recorder --example real_capture
bin=${CARGO_TARGET_DIR:-$repo/target}/release/examples/real_capture
[[ -x $bin ]] || { echo "real-capture-first-epoch: built, but no binary at $bin" >&2; exit 2; }

export XDG_RUNTIME_DIR=$work/run XDG_CONFIG_HOME=$work/config \
  XDG_STATE_HOME=$work/state XDG_DATA_HOME=$work/data
# Nothing may point at the user's servers or session bus, nor start a bus
# of its own that would outlive the script (D-Bus autolaunches one from a
# display).
unset PIPEWIRE_REMOTE PIPEWIRE_RUNTIME_DIR PULSE_SERVER PULSE_RUNTIME_PATH \
  DISPLAY WAYLAND_DISPLAY
export DBUS_SESSION_BUS_ADDRESS=disabled:
mkdir -p -m 700 "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d" \
  "$XDG_STATE_HOME" "$XDG_DATA_HOME"
# No hardware: the instance sees only the null sinks made below.
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
[ -S "$XDG_RUNTIME_DIR/pipewire-0" ] || { echo "real-capture-first-epoch: the private PipeWire didn't start" >&2; exit 1; }
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
  *) echo "real-capture-first-epoch: pactl reached $server, not the private instance" >&2; exit 1 ;;
esac
if pactl list short sinks | grep -v auto_null | grep -q . ||
  pactl list short sources | grep -v '\.monitor' | grep -q .; then
  echo "real-capture-first-epoch: the private instance has devices of its own:" >&2
  pactl list short sinks >&2
  exit 1
fi

pactl load-module module-null-sink sink_name=probe_a >/dev/null
pactl load-module module-null-sink sink_name=probe_b >/dev/null
pactl set-default-sink probe_a
# The default is the metadata WirePlumber keeps; wait for it to show.
for _ in $(seq 50); do
  [[ $(pactl get-default-sink) == probe_a ]] && break
  sleep 0.1
done
[[ $(pactl get-default-sink) == probe_a ]] || {
  echo "real-capture-first-epoch: the default sink never became probe_a" >&2
  exit 1
}

failed=0

# Silence played into a sink as a client does, started the moment the
# recorder's streams are running. The graph's delay for a capture stream
# shows only once a client is playing, and a stream that opens as one
# starts gets its first buffers with no delay: the case the recorder's hold
# is for. The player's PID is kept to stop it by.
play_into() {
  pw-cat --playback --target "$1" --rate 16000 --channels 1 --format s16 \
    --raw /dev/zero >/dev/null 2>&1 &
  players+=($!)
}

stop_players() {
  for pid in ${players[@]+"${players[@]}"}; do
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  done
  players=()
}

# run LABEL [--switch] RECORDER-ARGS...: records, starts the players once the
# streams run, and with --switch moves the default sink to B 3 s in.
run() {
  local label=$1 switch=0
  shift
  if [[ ${1:-} == --switch ]]; then
    switch=1
    shift
  fi
  echo "== $label"
  mkdir -p "$work/$label"
  "$bin" first-epoch "$work/$label" --seconds "$seconds" \
    --ready "$work/$label.ready" "$@" >"$work/$label.out" &
  recorder=$!
  for _ in $(seq 600); do
    [[ -e $work/$label.ready ]] && break
    kill -0 "$recorder" 2>/dev/null || break
    sleep 0.01
  done
  play_into probe_a
  if ((switch)); then
    play_into probe_b
    sleep 3
    pactl set-default-sink probe_b
  fi
  wait "$recorder" || failed=1
  recorder=
  stop_players
  cat "$work/$label.out"
}

# The move run must have been followed: a stream that stayed put probed
# nothing.
followed() {
  grep -q '^notice 0 RouteChanged' "$1" || {
    echo "real-capture-first-epoch: the move wasn't followed (no RouteChanged)" >&2
    failed=1
  }
}

run one --source probe_a --tracks 1
run two --source probe_a --tracks 2
# The move follows the default sink, which switches from A to B.
run move --switch --source sink_default --tracks 1
followed "$work/move.out"

exit "$failed"
