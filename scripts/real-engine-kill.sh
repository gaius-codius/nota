#!/usr/bin/env bash
# The M1a engine-kill criterion end to end with real capture: speech plays,
# the recorder captures it through PipeWire into journals and feeds each
# frame to the real engine (`nota engine asr`) under its supervisor, and
# the engine is SIGKILLed mid-sentence (the `real_capture engine` example).
# Each run checks that text resumes within 10 s, that no audio was skipped
# and all of it was confirmed, and that the text reads as the fixture.
#
# The fixture plays once into a temporary null sink, and the recorder
# captures that sink by name, so nothing is heard and the user's devices,
# defaults and volumes are left alone. The sink and the player are removed
# on exit, by module index and PID.
#
# Not run in CI (the real-engine kill test in `crates/nota/tests` covers the
# supervisor there, without capture): it needs a running PipeWire (with
# pipewire-pulse for `pactl`), `pw-play` and the test models
# (scripts/fetch-test-models.sh).
#
# Usage: scripts/real-engine-kill.sh [KILL_AFTER_MS ...]
#   one run per delay, from the first confirmed text to the kill
#   (default: 0 250 500 1000 1500 2000 3000)
#   NOTA_TEST_MODELS   the test models (default ~/.local/share/nota/test-models)

set -euo pipefail

MODELS=${NOTA_TEST_MODELS:-$HOME/.local/share/nota/test-models}
DELAYS=("$@")
[[ ${#DELAYS[@]} -gt 0 ]] || DELAYS=(0 250 500 1000 1500 2000 3000)

die() { echo "real-engine-kill: $*" >&2; exit 2; }

FIXTURE=$MODELS/fixtures/invented-lecture.wav
for f in "$FIXTURE" "$MODELS/silero_vad_v6.onnx" "$MODELS/parakeet-tdt-0.6b-v3-int8/encoder.int8.onnx"; do
  [[ -f $f ]] || die "no $f (run scripts/fetch-test-models.sh)"
done

REPO=$(cd "$(dirname "$0")/.." && pwd)
cargo build --manifest-path "$REPO/Cargo.toml" --release --locked --quiet \
  -p nota-recorder --example real_capture
cargo build --manifest-path "$REPO/Cargo.toml" --release --locked --quiet -p nota --bin nota
TARGET=${CARGO_TARGET_DIR:-$REPO/target}/release
BIN=$TARGET/examples/real_capture
ENGINE=$TARGET/nota
[[ -x $BIN && -x $ENGINE ]] || die "built, but no binaries in $TARGET"

WORK=$(mktemp -d)
SINK="nota_real_engine_$$"
MODULE=
PLAYER=
RUNNER=

cleanup() {
  [[ -n $RUNNER ]] && kill -9 "$RUNNER" 2> /dev/null || true
  [[ -n $PLAYER ]] && kill "$PLAYER" 2> /dev/null || true
  [[ -n $MODULE ]] && pactl unload-module "$MODULE" || true
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

default_before=$(pactl get-default-sink)
MODULE=$(pactl load-module module-null-sink "sink_name=$SINK" \
  "sink_properties=node.description=nota-real-engine priority.session=1 priority.driver=1")

FAILED=0
for delay in "${DELAYS[@]}"; do
  run=$WORK/run-$delay
  mkdir -p "$run/session"
  # The fixture is 17.6 s; capture runs on for the pause-cut tail.
  "$BIN" engine "$run/session" --source "$SINK" --seconds 24 \
    --engine "$ENGINE" --parakeet "$MODELS/parakeet-tdt-0.6b-v3-int8" \
    --vad "$MODELS/silero_vad_v6.onnx" \
    --said "$REPO/crates/nota-engine/tests/fixtures/invented-lecture.txt" \
    --kill-after-ms "$delay" --ready "$run/ready" > "$run/out" 2>&1 &
  RUNNER=$!
  tries=3000
  while [[ ! -e $run/ready ]] && kill -0 "$RUNNER" 2> /dev/null; do
    tries=$((tries - 1))
    [[ $tries -gt 0 ]] || die "the engine never came up"
    sleep 0.02
  done
  pw-play --target "$SINK" "$FIXTURE" &
  PLAYER=$!
  status=0
  wait "$RUNNER" || status=$?
  RUNNER=
  wait "$PLAYER" 2> /dev/null || true
  PLAYER=
  echo "kill_after=${delay}ms $(sed -n 's/^result //p' "$run/out")"
  if [[ $status -ne 0 ]]; then
    FAILED=$((FAILED + 1))
    sed 's/^/  /' "$run/out" >&2
  fi
done

if [[ "$(pactl get-default-sink)" != "$default_before" ]]; then
  echo "real-engine-kill: the default sink changed during the run (was $default_before)" >&2
fi
echo "${#DELAYS[@]} runs, $FAILED failed"
[[ $FAILED -eq 0 ]]
