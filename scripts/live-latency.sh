#!/usr/bin/env bash
# The M1b latency criterion (T1, second half) end to end: speech plays into
# temporary null sinks, `nota record` captures them on a pseudo-terminal
# with the real engine, and each text's latency is measured from the last
# sample of its pause-cut chunk to the moment it's handed to the screen
# (which draws on receipt). Built with the `latency-log` feature, `nota
# record --latency-log FILE` logs, for each text, its track, its chunk's
# first and last sample in session time, and when it was shown.
#
# Speech is the test fixture (four sentences, 17.6 s), looped:
#   ordinary     as it is: sentences with pauses of 0.2-1.2 s between them,
#                so chunks are cut at pauses about 3-4 s in
#   continuous   with every pause over 50 ms removed, so the chunker meets
#                its 10 s cap and cuts at the quietest moment instead
# With --tracks 1, the mic records the speech and the system audio is a
# node that doesn't exist, so it isn't recorded. With --tracks 2, both
# record the same speech from two sinks, the system audio starting
# --offset-ms later (default 0: both tracks' chunks end together, and the
# one engine decodes one while the other waits, the worst case).
#
# Nothing is heard and the user's devices, defaults and volumes are left
# alone: the recorder captures the sinks by name. The sinks, players and
# recorder are stopped on exit by module index and PID.
#
# Pass if, for every track:
#   - ordinary speech: the median latency (shown - chunk end) is at most
#     3 s and the 95th percentile at most 5 s;
#   - both: no text is older than the live cap plus 5 s when shown
#     (shown - chunk start <= 15 s).
# In continuous speech there's no pause to cut at early: the chunker cuts
# once a chunk reaches the cap, at the widest pause within it, so a
# chunk's end is already some seconds old when it's cut. That's what the
# cap-plus-5 s bound is for; the latency is printed there too, but not
# judged.
#
# Not run in CI: it needs a running PipeWire (with pipewire-pulse for
# `pactl`), `pw-play`, `ffmpeg`, `script` (util-linux) and the test models
# (scripts/fetch-test-models.sh). Each run records in real time.
#
# Usage: scripts/live-latency.sh [--speech ordinary|continuous] [--tracks 1|2]
#          [--loops N] [--offset-ms MS] [--keep]
#   --loops N    how many times the fixture plays (default 10: about 3 min)
#   --keep       keep the work directory (the logs) and print its path
#   NOTA_TEST_MODELS   the test models (default ~/.local/share/nota/test-models)

set -euo pipefail

MODELS=${NOTA_TEST_MODELS:-$HOME/.local/share/nota/test-models}
SPEECH=ordinary
TRACKS=1
LOOPS=10
OFFSET_MS=0
KEEP=0

die() { echo "live-latency: $*" >&2; exit 2; }

while [[ $# -gt 0 ]]; do
  case $1 in
    --speech) SPEECH=${2:?}; shift ;;
    --tracks) TRACKS=${2:?}; shift ;;
    --loops) LOOPS=${2:?}; shift ;;
    --offset-ms) OFFSET_MS=${2:?}; shift ;;
    --keep) KEEP=1 ;;
    *) die "unknown option $1" ;;
  esac
  shift
done
[[ $SPEECH == ordinary || $SPEECH == continuous ]] || die "--speech is ordinary or continuous"
[[ $TRACKS == 1 || $TRACKS == 2 ]] || die "--tracks is 1 or 2"
[[ $LOOPS =~ ^[1-9][0-9]*$ ]] || die "--loops takes a positive number"
[[ $OFFSET_MS =~ ^[0-9]+$ ]] || die "--offset-ms takes a number"

FIXTURE=$MODELS/fixtures/invented-lecture.wav
PARAKEET=$MODELS/parakeet-tdt-0.6b-v3-int8
VAD=$MODELS/silero_vad_v6.onnx
for f in "$FIXTURE" "$VAD" "$PARAKEET/encoder.int8.onnx"; do
  [[ -f $f ]] || die "no $f (run scripts/fetch-test-models.sh)"
done
for c in pactl pw-play ffmpeg script; do
  command -v "$c" > /dev/null || die "$c is required"
done

REPO=$(cd "$(dirname "$0")/.." && pwd)
# From the repo, so its .cargo/config.toml points the build at the pinned
# sherpa-onnx libraries (scripts/sherpa-onnx.sh).
(cd "$REPO" && cargo build --release --locked --quiet -p nota --features latency-log)
NOTA=${CARGO_TARGET_DIR:-$REPO/target}/release/nota
[[ -x $NOTA ]] || die "built, but no $NOTA"

WORK=$(mktemp -d)
MODULES=()
PLAYERS=()
TERMINAL=

cleanup() {
  [[ -n $TERMINAL ]] && kill -9 "$TERMINAL" 2> /dev/null || true
  for p in "${PLAYERS[@]}"; do kill "$p" 2> /dev/null || true; done
  for m in "${MODULES[@]}"; do pactl unload-module "$m" || true; done
  if [[ $KEEP -eq 1 ]]; then
    echo "Kept $WORK" >&2
  else
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# The speech.
inputs=()
for _ in $(seq "$LOOPS"); do inputs+=(-i "$FIXTURE"); done
filter=""
for i in $(seq 0 $((LOOPS - 1))); do filter+="[$i:a]"; done
filter+="concat=n=$LOOPS:v=0:a=1"
if [[ $SPEECH == continuous ]]; then
  # Every silence over 50 ms is cut to 50 ms: Silero needs 150 ms for a
  # pause.
  filter+=",silenceremove=stop_periods=-1:stop_duration=0.05:stop_threshold=-45dB:stop_silence=0.05"
fi
SPOKEN=$WORK/speech.wav
ffmpeg -nostdin -loglevel error -y "${inputs[@]}" -filter_complex "$filter" \
  -ar 16000 -ac 1 -c:a pcm_s16le "$SPOKEN"
spoken_s=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$SPOKEN")

default_before=$(pactl get-default-sink)
SINKS=()
for t in $(seq "$TRACKS"); do
  sink="nota_latency_${$}_$t"
  MODULES+=("$(pactl load-module module-null-sink "sink_name=$sink" \
    "sink_properties=node.description=nota-latency-$t")")
  SINKS+=("$sink")
done
MIC=${SINKS[0]}
SYSTEM=${SINKS[1]:-nota_latency_no_such_node_$$}

LOG=$WORK/latency.tsv
OUT=$WORK/terminal.out
# A pseudo-terminal for the screen, sized as an ordinary terminal.
script -qfec "stty cols 120 rows 40; exec '$NOTA' record --data '$WORK/data' \
  --title latency --parakeet '$PARAKEET' --vad '$VAD' --mic '$MIC' \
  --system '$SYSTEM' --latency-log '$LOG'" "$OUT" < /dev/null > /dev/null 2>&1 &
TERMINAL=$!
# The engine loads in a second or two; text before that would measure the
# loading, not the live path.
sleep 5
kill -0 "$TERMINAL" 2> /dev/null || die "nota record stopped at once: $(tail -c 2000 "$OUT")"
RECORDER=$(pgrep -P "$TERMINAL" -x nota || true)
[[ -n $RECORDER ]] || die "no nota under the terminal"

pw-play --target "$MIC" "$SPOKEN" &
PLAYERS+=($!)
if [[ $TRACKS -eq 2 ]]; then
  sleep "$(awk -v ms="$OFFSET_MS" 'BEGIN { printf "%.3f", ms / 1000 }')"
  pw-play --target "${SINKS[1]}" "$SPOKEN" &
  PLAYERS+=($!)
fi
for p in "${PLAYERS[@]}"; do wait "$p" || die "pw-play failed"; done
PLAYERS=()
# The last chunk ends at the trailing pause; give it time to show.
sleep 8
# Stopped as a closing terminal stops it: it must still publish everything.
kill -HUP "$RECORDER"
for _ in $(seq 600); do
  kill -0 "$TERMINAL" 2> /dev/null || break
  sleep 0.1
done
kill -0 "$TERMINAL" 2> /dev/null && die "nota record didn't stop"
stopped=0
wait "$TERMINAL" || stopped=$?
TERMINAL=
[[ -f $LOG ]] || die "no latency log: $(tail -c 2000 "$OUT")"

if [[ "$(pactl get-default-sink)" != "$default_before" ]]; then
  echo "live-latency: the default sink changed during the run (was $default_before)" >&2
fi

# nota's summary, without the terminal's control sequences.
summary=$(sed 's/\x1b\[[0-9;?]*[a-zA-Z]//g' "$OUT" | tr '\r' '\n' |
  awk '/nota: recorded to/ { on = 1; sub(/.*nota: recorded to/, "nota: recorded to") }
    /^Script done/ { on = 0 } on && NF')
echo "$summary"

# Per track: texts, latency median / p95 / max (shown - end), the oldest
# text (shown - start), and chunk lengths, in ms. Percentiles by nearest
# rank.
echo "speech=$SPEECH tracks=$TRACKS offset_ms=$OFFSET_MS spoken_s=$spoken_s"
status=0
for t in $(seq 0 $((TRACKS - 1))); do
  latencies=$(awk -F'\t' -v t="$t" 'NR > 1 && $1 == t { print $4 - $3 }' "$LOG" | sort -n)
  oldest=$(awk -F'\t' -v t="$t" 'NR > 1 && $1 == t { print $4 - $2 }' "$LOG" | sort -n | tail -1)
  chunks=$(awk -F'\t' -v t="$t" 'NR > 1 && $1 == t { print $3 - $2 }' "$LOG" | sort -n)
  n=$(grep -c . <<< "$latencies" || true)
  if [[ $n -eq 0 ]]; then
    echo "track=$t texts=0 FAIL"
    status=1
    continue
  fi
  rank() { awk -v n="$n" -v p="$1" 'BEGIN { r = int(n * p / 100); if (r < n * p / 100) r++; if (r < 1) r = 1; print r }'; }
  median=$(sed -n "$(rank 50)p" <<< "$latencies")
  p95=$(sed -n "$(rank 95)p" <<< "$latencies")
  max=$(tail -1 <<< "$latencies")
  min=$(head -1 <<< "$latencies")
  chunk_median=$(sed -n "$(rank 50)p" <<< "$chunks")
  chunk_max=$(tail -1 <<< "$chunks")
  verdict=pass
  if [[ $oldest -gt 15000 ]] ||
    [[ $SPEECH == ordinary && ($median -gt 3000 || $p95 -gt 5000) ]]; then
    verdict=FAIL
    status=1
  fi
  echo "track=$t texts=$n latency_ms min=$min median=$median p95=$p95 max=$max" \
    "oldest_ms=$oldest chunk_ms median=$chunk_median max=$chunk_max $verdict"
done
if [[ $stopped -ne 0 ]]; then
  echo "nota record exited with $stopped after SIGHUP: something wasn't published"
  status=1
fi
exit $status
