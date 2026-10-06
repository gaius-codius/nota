#!/usr/bin/env bash
# The M1b latency criterion (T1, second half) end to end: speech plays into
# temporary null sinks, `nota record` captures them on a pseudo-terminal
# with the real engine, and each text's latency is measured from the last
# sample of its pause-cut chunk to the end of the screen's draw that shows
# it. Built with the `latency-log` feature, `nota record --latency-log FILE`
# logs, for each text, its track, its chunk's first and last sample in
# session time, when it was handed to the screen and when it was drawn,
# and anything that kept text from the screen (see crates/nota/src/latency.rs).
#
# Speech is the test fixture (four sentences, 17.6 s), looped:
#   ordinary     as it is: sentences with pauses of 0.2-1.0 s between them,
#                so chunks are cut at pauses about 3-4 s in
#   continuous   with every pause over 50 ms cut to 50 ms (the detector
#                needs 150 ms for a pause), so the chunker reaches its 10 s
#                cap and cuts at the widest short pause in it
# With --tracks 1, the mic records the speech and the system audio is a
# node that doesn't exist, so it isn't recorded. With --tracks 2, both
# record the same speech from two sinks, the system audio starting
# --offset-ms later (default 0: both tracks' chunks end together, and the
# one engine decodes one while the other waits, the worst case).
#
# Nothing is heard and the user's devices, defaults and volumes are left
# alone: the recorder captures the sinks by name. The recording is stopped
# with SIGHUP, as a closing terminal stops it. The sinks, players, recorder
# and terminal are stopped on exit by module index and PID.
#
# Pass if, for every track:
#   - ordinary speech: the median latency (drawn - chunk end) is at most
#     3 s and the 95th percentile at most 5 s;
#   - both: no text is older than the live cap plus 5 s when drawn
#     (drawn - chunk start <= 15 s);
#   - every text was drawn, nothing kept text from the screen (a dropped
#     transcript, text after the screen closed, skipped audio, the engine
#     offline, a new epoch, a failed stream), no gap over 1 s between texts once speech has started (a
#     chunk with speech that never showed), and the last text reaches to
#     within 2 s of the end of its speech;
#   - nota exits 0 after the SIGHUP (everything published), and the default
#     sink is unchanged (the sinks are created at the lowest priority).
# In continuous speech there's no pause to cut at early: the chunker cuts
# once a chunk reaches the cap, so a chunk's end is already some seconds old
# when it's cut. That's what the cap-plus-5 s bound is for; the latency is
# printed there too, but not judged.
#
# Percentiles are by nearest rank; the median of an even count is the upper
# of the two middle values.
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
for c in pactl pw-play ffmpeg ffprobe script pgrep; do
  command -v "$c" > /dev/null || die "$c is required"
done

REPO=$(cd "$(dirname "$0")/.." && pwd)
# Absolute, since the build runs from the repo.
[[ -z ${CARGO_TARGET_DIR:-} || $CARGO_TARGET_DIR == /* ]] || export CARGO_TARGET_DIR=$PWD/$CARGO_TARGET_DIR
# From the repo, so its .cargo/config.toml points the build at the pinned
# sherpa-onnx libraries (scripts/sherpa-onnx.sh).
(cd "$REPO" && cargo build --release --locked --quiet -p nota --features latency-log)
NOTA=${CARGO_TARGET_DIR:-$REPO/target}/release/nota
[[ -x $NOTA ]] || die "built, but no $NOTA"

WORK=$(mktemp -d)
MODULES=()
PLAYERS=()
TERMINAL=
RECORDER=

cleanup() {
  # The recorder first, by its PID: on a hangup it publishes into $WORK, so
  # it must be gone before $WORK is.
  if [[ -n $RECORDER ]] && kill -HUP "$RECORDER" 2> /dev/null; then
    for _ in $(seq 100); do
      kill -0 "$RECORDER" 2> /dev/null || break
      sleep 0.1
    done
    kill -9 "$RECORDER" 2> /dev/null || true
  fi
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
    "sink_properties=node.description=nota-latency-$t priority.session=1 priority.driver=1")")
  SINKS+=("$sink")
done
MIC=${SINKS[0]}
SYSTEM=${SINKS[1]:-nota_latency_no_such_node_$$}

LOG=$WORK/latency.tsv
OUT=$WORK/terminal.out
# Milliseconds since nota started, close to its session clock (which starts
# a little later, so these overstate session times slightly).
now_ms() { echo $(( ($(date +%s%N) - STARTED_NS) / 1000000 )); }
command=$(printf '%q ' "$NOTA" record --data "$WORK/data" --title latency \
  --parakeet "$PARAKEET" --vad "$VAD" --mic "$MIC" --system "$SYSTEM" \
  --latency-log "$LOG")
# A pseudo-terminal for the screen, sized as an ordinary terminal.
STARTED_NS=$(date +%s%N)
SHELL=/bin/bash script -qfec "stty cols 120 rows 40; exec $command" "$OUT" \
  < /dev/null > /dev/null 2>&1 &
TERMINAL=$!
# The engine loads in a second or two; text before that would measure the
# loading, not the live path.
sleep 5
kill -0 "$TERMINAL" 2> /dev/null || die "nota record stopped at once: $(tail -c 2000 "$OUT")"
RECORDER=$(pgrep -P "$TERMINAL" -x nota || true)
[[ -n $RECORDER ]] || die "no nota under the terminal"

# Each track's speech ends at the end of its playback (in ms since start).
ENDED=()
pw-play --target "$MIC" "$SPOKEN" &
PLAYERS+=($!)
if [[ $TRACKS -eq 2 ]]; then
  sleep "$(awk -v ms="$OFFSET_MS" 'BEGIN { printf "%.3f", ms / 1000 }')"
  pw-play --target "${SINKS[1]}" "$SPOKEN" &
  PLAYERS+=($!)
fi
for p in "${PLAYERS[@]}"; do
  wait "$p" || die "pw-play failed"
  ENDED+=("$(now_ms)")
done
PLAYERS=()
# The last chunk ends at the trailing pause; give it time to show.
sleep 8
# Still the nota this script started, under its terminal.
[[ $(pgrep -P "$TERMINAL" -x nota || true) == "$RECORDER" ]] ||
  die "nota record stopped early: $(tail -c 2000 "$OUT")"
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
RECORDER=
[[ -f $LOG ]] || die "no latency log: $(tail -c 2000 "$OUT")"

# Not changed back: the user may have changed it themselves meanwhile.
default_changed=0
[[ "$(pactl get-default-sink)" == "$default_before" ]] || default_changed=1

# nota's summary, without the terminal's control sequences.
sed 's/\x1b\[[0-9;?]*[a-zA-Z]//g' "$OUT" | tr '\r' '\n' |
  awk '/nota: recorded to/ { on = 1; sub(/.*nota: recorded to/, "nota: recorded to") }
    /^Script done/ { on = 0 } on && NF'
echo "speech=$SPEECH tracks=$TRACKS offset_ms=$OFFSET_MS spoken_s=$spoken_s"
status=0
if [[ $stopped -ne 0 ]]; then
  echo "nota record exited with $stopped after SIGHUP: something wasn't published FAIL"
  status=1
fi
if [[ $default_changed -eq 1 ]]; then
  echo "the default sink changed during the run (was $default_before) FAIL"
  status=1
fi
problems=$(awk -F'\t' 'NR > 1 && $1 != "text"' "$LOG")
if [[ -n $problems ]]; then
  echo "kept from the screen FAIL:"
  while IFS= read -r line; do echo "  $line"; done <<< "$problems"
  status=1
fi

# Per track: texts, latency median / p95 / max (drawn - end), the oldest
# text (drawn - start), chunk lengths, gaps between texts, and how far the
# last text falls short of the end of the speech, in ms.
for t in $(seq 0 $((TRACKS - 1))); do
  texts=$(awk -F'\t' -v t="$t" 'NR > 1 && $1 == "text" && $2 == t' "$LOG")
  n=$(grep -c . <<< "$texts" || true)
  if [[ $n -eq 0 ]]; then
    echo "track=$t texts=0 FAIL"
    status=1
    continue
  fi
  undrawn=$(awk -F'\t' '$6 == "-"' <<< "$texts" | grep -c . || true)
  latencies=$(awk -F'\t' '$6 != "-" { print $6 - $4 }' <<< "$texts" | sort -n)
  oldest=$(awk -F'\t' '$6 != "-" { print $6 - $3 }' <<< "$texts" | sort -n | tail -1)
  chunks=$(awk -F'\t' '{ print $4 - $3 }' <<< "$texts" | sort -n)
  # Gaps between one text's chunk and the next (silent chunks aren't
  # transcribed), once speech has started.
  gap_max=$(sort -t$'\t' -k3,3n <<< "$texts" |
    awk -F'\t' 'NR > 1 { g = $3 - end; if (g > max) max = g } { end = $4 } END { print max + 0 }')
  last_end=$(awk -F'\t' '{ if ($4 > max) max = $4 } END { print max + 0 }' <<< "$texts")
  short=$((ENDED[t] - last_end))
  m=$(grep -c . <<< "$latencies" || true)
  at() { sed -n "$1p" <<< "$2"; }
  if [[ $m -gt 0 ]]; then
    median=$(at $((m / 2 + 1)) "$latencies")
    p95=$(at "$(awk -v n="$m" 'BEGIN { r = n * 95 / 100; print (r == int(r)) ? r : int(r) + 1 }')" "$latencies")
    max=$(tail -1 <<< "$latencies")
    min=$(head -1 <<< "$latencies")
  else
    median=- p95=- max=- min=- oldest=0
  fi
  chunk_median=$(at $((n / 2 + 1)) "$chunks")
  chunk_max=$(tail -1 <<< "$chunks")
  verdict=pass
  if [[ $undrawn -gt 0 || $m -eq 0 || $oldest -gt 15000 || $gap_max -gt 1000 || $short -gt 2000 ]] ||
    [[ $SPEECH == ordinary && ($median -gt 3000 || $p95 -gt 5000) ]]; then
    verdict=FAIL
    status=1
  fi
  echo "track=$t texts=$n undrawn=$undrawn latency_ms min=$min median=$median p95=$p95" \
    "max=$max oldest_ms=$oldest chunk_ms median=$chunk_median max=$chunk_max" \
    "gap_max_ms=$gap_max short_of_end_ms=$short $verdict"
done
exit $status
