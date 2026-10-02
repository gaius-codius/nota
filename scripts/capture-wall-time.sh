#!/usr/bin/env bash
# Captures a tone through the recorder's PipeWire backend for a fixed time
# and checks that the sample count matches the wall clock (the
# `capture_wall_time` example).
#
# The tone plays into a temporary null sink, and the recorder captures that
# sink by name, so nothing is heard and the user's devices, defaults and
# volumes are left alone. The sink and the player are removed on exit, by
# module index and PID.
#
# Not run in CI: it needs a running PipeWire (with pipewire-pulse for
# `pactl`), `pw-play` and `ffmpeg`. Run it by hand after changing capture.
#
# Usage: scripts/capture-wall-time.sh [SECONDS]   (default 600)
set -euo pipefail

seconds="${1:-600}"
sink="nota_wall_time_$$"
work=$(mktemp -d)
module="" player=""

cleanup() {
  [ -n "$player" ] && kill "$player" 2>/dev/null || true
  [ -n "$module" ] && pactl unload-module "$module" || true
  rm -rf "$work"
}
trap cleanup EXIT

default_before=$(pactl get-default-sink)
# The lowest priority, so the session manager never picks it as the default.
module=$(pactl load-module module-null-sink "sink_name=$sink" \
  "sink_properties=node.description=nota-wall-time priority.session=1 priority.driver=1")

# A 440 Hz tone at -38 dBFS, longer than the capture.
ffmpeg -loglevel error -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=$((seconds + 30))" \
  -af volume=0.0125 -f wav - | pw-play --target "$sink" - &
player=$!

mkdir "$work/session"
status=0
cargo run --quiet --release --locked -p nota-recorder --example capture_wall_time -- \
  "$work/session" "$seconds" device "$sink" || status=$?

if ! kill -0 "$player" 2>/dev/null; then
  echo "capture-wall-time.sh: the tone stopped before the capture did" >&2
  status=1
fi
if [ "$(pactl get-default-sink)" != "$default_before" ]; then
  echo "capture-wall-time.sh: the default sink changed during the run (was $default_before)" >&2
fi
exit "$status"
