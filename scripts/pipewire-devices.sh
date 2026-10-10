#!/usr/bin/env bash
# The recorder's device events against real PipeWire (the
# `pipewire_devices` test in nota-recorder): a followed default output
# switched is Changed, a pinned sink removed is Lost and ends its track,
# and a route change's epoch holds only the new device's audio.
#
# Switching a default can't be tested on the user's own PipeWire without
# changing their defaults (AGENTS.md section 8), so this starts a private
# PipeWire, WirePlumber and pipewire-pulse with no hardware, in a temporary
# runtime directory of its own, with its own config and state (WirePlumber
# saves the defaults it's given there, not in the user's state). The test's
# null sinks and players live only in that instance; the user's devices,
# defaults and volumes are never touched. Everything is stopped on exit,
# by PID, and the directory removed.
#
# Not run in CI: it needs PipeWire 1.x, WirePlumber, pipewire-pulse,
# `pactl` and `pw-play` installed. Run it by hand after changing capture.
#
# Usage: scripts/pipewire-devices.sh
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/nota-pipewire.XXXXXX")
pids=()

cleanup() {
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

# The no-hardware config below is WirePlumber 0.5's; 0.4 would ignore it
# and open the user's real devices.
wp_version=$(wireplumber --version | sed -n 's/.*libwireplumber \([0-9]*\)\.\([0-9]*\).*/\1 \2/p')
read -r wp_major wp_minor <<<"${wp_version:-0 0}"
if [ "$wp_major" -eq 0 ] && [ "$wp_minor" -lt 5 ]; then
  echo "pipewire-devices.sh: needs WirePlumber 0.5 or later" >&2
  exit 1
fi

# Built before the private instance's environment is set, so the build sees
# the user's own.
cd "$repo"
cargo nextest run --locked -p nota-recorder --test pipewire_devices --no-run

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
# No hardware: the instance sees only the test's null sinks.
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
[ -S "$XDG_RUNTIME_DIR/pipewire-0" ] || { echo "pipewire-devices.sh: the private PipeWire didn't start" >&2; exit 1; }
wireplumber >"$work/wireplumber.log" 2>&1 &
pids+=($!)
pipewire-pulse >"$work/pipewire-pulse.log" 2>&1 &
pids+=($!)
for _ in $(seq 50); do
  pactl info >/dev/null 2>&1 && break
  sleep 0.1
done

# The guard the test checks too: pactl must be talking to this instance.
server=$(pactl info | sed -n 's/^Server String: //p')
case $server in
  "$XDG_RUNTIME_DIR"/*) ;;
  *) echo "pipewire-devices.sh: pactl reached $server, not the private instance" >&2; exit 1 ;;
esac

# And it has no hardware: only the test's own devices may come and go, beside
# WirePlumber's fallback sink (auto_null), which stands in when there's none.
if pactl list short sinks | grep -v auto_null | grep -q . ||
  pactl list short sources | grep -v '\.monitor' | grep -q .; then
  echo "pipewire-devices.sh: the private instance has devices before the test:" >&2
  pactl list short sinks >&2
  pactl list short sources >&2
  exit 1
fi

NOTA_PRIVATE_PIPEWIRE=$XDG_RUNTIME_DIR \
  cargo nextest run --locked -p nota-recorder --test pipewire_devices --no-capture
