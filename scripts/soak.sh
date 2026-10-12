#!/usr/bin/env bash
# The M2 early soak (synthesis test 6) end to end: `nota record` captures
# looped speech from temporary null sinks with real PipeWire for hours, and
# the session it leaves is measured against what was played.
#
# What is played (a 60 s unit, looped): the speech fixture three times
# (3 x 17.6 s), a low noise floor throughout, so the pauses are never digital
# zeros, and a 40 ms burst of a 6 kHz tone every 10 s from the unit's first
# second (the first second has none, so the player's links are up before the
# first one). The bursts are the ground truth. Before the player starts and
# after it ends the sinks are silent, and nota's digital-zeros warning
# rightly fires there. Both tracks hear the same player: it is linked, sample for
# sample, to both sinks (`pw-link`), so any difference between the tracks is
# nota's, not the players'. With --tracks 1 only the mic records and the
# system audio is a node that doesn't exist.
#
# What is sampled, every 30 s while it runs (samples.tsv): the recorder's
# and the engine's resident memory, the recorder's CPU time, the load
# average, free memory, the size of the session on disk, and the default
# sink (which must not change). The busiest processes are noted every five
# minutes (others.txt), since other work on the machine moves every figure.
#
# What is measured afterwards from the session (scripts/soak-analyse.py):
#   - memory: growth after the first 10 minutes, for the recorder, the
#     engine and both together;
#   - drift after correction: each burst's time in the session, which the
#     epochs map from the recorded samples, against the 10 s the player
#     spaced them by; the spread of that difference over the run, for each
#     track (and what the same would be without the epochs' drift);
#   - gaps: each gap the epochs imply against the timeline's gap events,
#     any step in the bursts' times that no epoch or event explains, any
#     burst missing, and how much of the end of the audio is missing;
#   - cross-track alignment: the same burst's time on the two tracks.
# The delivered-to-durable lag isn't here: `nota record` doesn't expose the
# durable position, so it can't be read from outside (see the report).
#
# Nothing is heard and the user's devices, defaults and volumes are left
# alone: the recorder captures the sinks by name. Everything started is
# stopped by exact PID and module index on exit. The work directory is
# removed after a passing run unless --keep; a failed or interrupted run
# keeps it (it is the only thing to re-analyse).
#
# Pass if (the exit criterion):
#   - resident memory (recorder plus engine) grows less than 50 MB after
#     the first 10 minutes;
#   - each track's drift after correction is under 20 ms;
#   - no unlogged gaps: every gap in the audio is a timeline event, no burst
#     is missing, no step goes unexplained, and no track ends early;
#   - nota exits 0 after the SIGHUP (everything published) and the default
#     sink is unchanged.
#
# Not run in CI: it needs a running PipeWire (with pipewire-pulse for
# `pactl`), `pw-play`, `pw-link`, `ffmpeg`, `script` (util-linux), python3 and
# the test models (scripts/fetch-test-models.sh), and it takes as long as it
# records. The recording goes under ~/.cache (a few hundred MB an hour), not
# /tmp, which is memory.
#
# Usage: scripts/soak.sh [--tracks 1|2] [--minutes N] [--keep] [--out DIR]
#        scripts/soak.sh --analyse DIR
#   --tracks N     how many tracks record (default 2)
#   --minutes N    how long the speech plays (default 180), a multiple of 10
#   --keep         keep the work directory and print its path
#   --out DIR      the work directory (default ~/.cache/nota/soak/<date>)
#   --analyse DIR  measure a kept run again; records nothing
#   NOTA_TEST_MODELS   the test models (default ~/.local/share/nota/test-models)

set -euo pipefail

MODELS=${NOTA_TEST_MODELS:-$HOME/.local/share/nota/test-models}
TRACKS=2
MINUTES=180
KEEP=0
OUT=
ANALYSE=
SAMPLE_EVERY=30
OTHERS_EVERY=300

die() { echo "soak: $*" >&2; exit 2; }

while [[ $# -gt 0 ]]; do
  case $1 in
    --tracks) TRACKS=${2:?}; shift ;;
    --minutes) MINUTES=${2:?}; shift ;;
    --keep) KEEP=1 ;;
    --out) OUT=${2:?}; shift ;;
    --analyse) ANALYSE=${2:?}; shift ;;
    *) die "unknown option $1" ;;
  esac
  shift
done
[[ $TRACKS == 1 || $TRACKS == 2 ]] || die "--tracks is 1 or 2"
[[ $MINUTES =~ ^[1-9][0-9]*0$ ]] || die "--minutes takes a multiple of 10"

REPO=$(cd "$(dirname "$0")/.." && pwd)
ANALYSER=$REPO/scripts/soak-analyse.py
command -v python3 > /dev/null || die "python3 is required"

if [[ -n $ANALYSE ]]; then
  exec python3 -I "$ANALYSER" "$ANALYSE"
fi

FIXTURE=$MODELS/fixtures/invented-lecture.wav
PARAKEET=$MODELS/parakeet-tdt-0.6b-v3-int8
VAD=$MODELS/silero_vad_v6.onnx
for f in "$FIXTURE" "$VAD" "$PARAKEET/encoder.int8.onnx"; do
  [[ -f $f ]] || die "no $f (run scripts/fetch-test-models.sh)"
done
for c in pactl pw-play pw-link ffmpeg script pgrep; do
  command -v "$c" > /dev/null || die "$c is required"
done

# Absolute, since the build runs from the repo.
[[ -z ${CARGO_TARGET_DIR:-} || $CARGO_TARGET_DIR == /* ]] || export CARGO_TARGET_DIR=$PWD/$CARGO_TARGET_DIR
# From the repo, so its .cargo/config.toml points the build at the pinned
# sherpa-onnx libraries (scripts/sherpa-onnx.sh). The release build as it
# ships: no measuring features.
(cd "$REPO" && cargo build --release --locked --quiet -p nota)
NOTA=${CARGO_TARGET_DIR:-$REPO/target}/release/nota
[[ -x $NOTA ]] || die "built, but no $NOTA"

WORK=${OUT:-$HOME/.cache/nota/soak/$(date +%Y%m%d-%H%M%S)}
[[ ! -e $WORK ]] || die "$WORK exists"
mkdir -p "$WORK"
MODULES=()
PLAYER=
TERMINAL=
RECORDER=
FINISHED=0

cleanup() {
  # The recorder first, by its PID: on a hangup it publishes into $WORK.
  if [[ -n $RECORDER ]] && kill -HUP "$RECORDER" 2> /dev/null; then
    for _ in $(seq 600); do
      kill -0 "$RECORDER" 2> /dev/null || break
      sleep 0.1
    done
    kill -9 "$RECORDER" 2> /dev/null || true
  fi
  [[ -n $TERMINAL ]] && kill -9 "$TERMINAL" 2> /dev/null || true
  [[ -n $PLAYER ]] && kill "$PLAYER" 2> /dev/null || true
  for m in ${MODULES[@]+"${MODULES[@]}"}; do pactl unload-module "$m" || true; done
  if [[ $KEEP -eq 1 || $FINISHED -eq 0 ]]; then
    echo "Kept $WORK" >&2
  else
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# The speech and the bursts, one 60 s unit, then looped to the whole run.
ffmpeg -nostdin -loglevel error -y -i "$FIXTURE" -i "$FIXTURE" -i "$FIXTURE" \
  -filter_complex "[0:a][1:a][2:a]concat=n=3:v=0:a=1,apad=whole_dur=60[speech];
    aevalsrc='0.3*sin(2*PI*6000*t)*gte(t,1)*lt(mod(t-1,10),0.04)':s=16000:d=60[burst];
    anoisesrc=r=16000:d=60:a=0.002:c=white[floor];
    [speech][burst][floor]amix=inputs=3:duration=first:normalize=0" \
  -ar 16000 -ac 1 -c:a pcm_s16le "$WORK/unit.wav"
SECONDS_PLAYED=$((MINUTES * 60))
ffmpeg -nostdin -loglevel error -y -stream_loop -1 -i "$WORK/unit.wav" \
  -t "$SECONDS_PLAYED" -c:a pcm_s16le "$WORK/speech.wav"

default_before=$(pactl get-default-sink)
SINKS=()
for t in $(seq "$TRACKS"); do
  sink="nota_soak_${$}_$t"
  MODULES+=("$(pactl load-module module-null-sink "sink_name=$sink" \
    "sink_properties=node.description=nota-soak-$t priority.session=1 priority.driver=1")")
  SINKS+=("$sink")
done
MIC=${SINKS[0]}
SYSTEM=${SINKS[1]:-nota_soak_no_such_node_$$}

wall() { date +%s.%N; }
{
  echo "tracks=$TRACKS"
  echo "minutes=$MINUTES"
  echo "burst_offset_s=1"
  echo "nota=$NOTA"
  echo "host=$(uname -srm)"
  echo "cpus=$(nproc)"
  echo "default_sink=$default_before"
} > "$WORK/run.env"

command=$(printf '%q ' "$NOTA" record --data "$WORK/data" --title soak \
  --parakeet "$PARAKEET" --vad "$VAD" --mic "$MIC" --system "$SYSTEM")
# A pseudo-terminal for the screen, sized as an ordinary terminal.
NOTA_STARTED=$(wall)
SHELL=/bin/bash script -qfec "stty cols 120 rows 40; exec $command" "$WORK/terminal.out" \
  < /dev/null > /dev/null 2>&1 &
TERMINAL=$!
# The engine loads in a second or two.
sleep 6
kill -0 "$TERMINAL" 2> /dev/null || die "nota record stopped at once: $(tail -c 2000 "$WORK/terminal.out")"
RECORDER=$(pgrep -P "$TERMINAL" -x nota || true)
[[ -n $RECORDER ]] || die "no nota under the terminal"

# The player makes no link of its own: the script links its ports to every
# sink, so the tracks hear the same samples at the same moment.
node="nota_soak_player_$$"
pw-play --target 0 -P "node.name=$node" "$WORK/speech.wav" &
PLAYER=$!
for _ in $(seq 100); do
  [[ $(pw-link -o | grep -c "^$node:output_") -ge 1 ]] && break
  sleep 0.1
done
outs=$(pw-link -o | grep "^$node:output_" | sed 's/^[^:]*://' || true)
[[ -n $outs ]] || die "the player's ports never appeared"
for out in $outs; do
  # A mono player feeds both of a sink's inputs.
  ins=("playback_${out#output_}")
  [[ $out == output_MONO ]] && ins=(playback_FL playback_FR)
  for sink in "${SINKS[@]}"; do
    for in in "${ins[@]}"; do
      pw-link "$node:$out" "$sink:$in" || die "couldn't link $node:$out to $sink:$in"
    done
  done
done
PLAY_STARTED=$(wall)
echo "play_started_after_s=$(awk -v a="$PLAY_STARTED" -v b="$NOTA_STARTED" 'BEGIN { printf "%.3f", a - b }')" >> "$WORK/run.env"

# Sampling, until the speech has played.
cpu_ticks() { awk '{ print $14 + $15 }' "/proc/$1/stat" 2> /dev/null || echo 0; }
rss_kb() { awk '/^VmRSS:/ { print $2 }' "/proc/$1/status" 2> /dev/null || echo 0; }
hwm_kb() { awk '/^VmHWM:/ { print $2 }' "/proc/$1/status" 2> /dev/null || echo 0; }
printf 'elapsed_s\tnota_rss_kb\tnota_hwm_kb\tengine_rss_kb\tengines\tnota_cpu_ticks\tthreads\tload1\tload5\tmem_avail_kb\tsession_kb\tdefault_sink_changed\n' \
  > "$WORK/samples.tsv"
: > "$WORK/others.txt"
tick=0
sample() {
  local now engine_rss=0 engines=0 pid load5 threads
  now=$(awk -v a="$(wall)" -v b="$PLAY_STARTED" 'BEGIN { printf "%.1f", a - b }')
  for pid in $(pgrep -P "$RECORDER" || true); do
    engine_rss=$((engine_rss + $(rss_kb "$pid")))
    engines=$((engines + 1))
  done
  threads=$(awk '/^Threads:/ { print $2 }' "/proc/$RECORDER/status" 2> /dev/null || echo 0)
  read -r load1 load5 _ < /proc/loadavg
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$now" \
    "$(rss_kb "$RECORDER")" "$(hwm_kb "$RECORDER")" "$engine_rss" "$engines" \
    "$(cpu_ticks "$RECORDER")" "$threads" "$load1" "$load5" \
    "$(awk '/^MemAvailable:/ { print $2 }' /proc/meminfo)" \
    "$(du -sk "$WORK/data/sessions" 2> /dev/null | cut -f1)" \
    "$([[ "$(pactl get-default-sink)" == "$default_before" ]] && echo 0 || echo 1)" \
    >> "$WORK/samples.tsv"
  if (( tick % (OTHERS_EVERY / SAMPLE_EVERY) == 0 )); then
    {
      echo "== $now s, load $load1 $load5"
      # sed reads all of ps's output, so ps is never left writing to a closed pipe.
      ps -eo pid,pcpu,rss,args --sort=-pcpu | sed -n '1,8p' | cut -c1-150
    } >> "$WORK/others.txt"
  fi
  tick=$((tick + 1))
}
while kill -0 "$PLAYER" 2> /dev/null; do
  kill -0 "$RECORDER" 2> /dev/null || die "nota record stopped early: $(tail -c 2000 "$WORK/terminal.out")"
  sample
  # Wakes early when the player ends.
  for _ in $(seq "$SAMPLE_EVERY"); do
    kill -0 "$PLAYER" 2> /dev/null || break
    sleep 1
  done
done
PLAY_ENDED=$(wall)
wait "$PLAYER" || die "pw-play failed"
PLAYER=
sample
# The recording goes on a little after the speech, in silence.
sleep 8
[[ $(pgrep -P "$TERMINAL" -x nota || true) == "$RECORDER" ]] ||
  die "nota record stopped early: $(tail -c 2000 "$WORK/terminal.out")"
STOP_SENT=$(wall)
{
  echo "play_to_stop_s=$(awk -v a="$STOP_SENT" -v b="$PLAY_STARTED" 'BEGIN { printf "%.3f", a - b }')"
  echo "played_s=$(awk -v a="$PLAY_ENDED" -v b="$PLAY_STARTED" 'BEGIN { printf "%.3f", a - b }')"
} >> "$WORK/run.env"
# Stopped as a closing terminal stops it: it must still publish everything.
kill -HUP "$RECORDER"
for _ in $(seq 9000); do
  kill -0 "$TERMINAL" 2> /dev/null || break
  sleep 0.1
done
kill -0 "$TERMINAL" 2> /dev/null && die "nota record didn't stop in 15 minutes"
stopped=0
wait "$TERMINAL" || stopped=$?
TERMINAL=
RECORDER=
echo "stop_took_s=$(awk -v a="$(wall)" -v b="$STOP_SENT" 'BEGIN { printf "%.1f", a - b }')" >> "$WORK/run.env"
echo "nota_exit=$stopped" >> "$WORK/run.env"

# nota's summary, without the terminal's control sequences.
sed 's/\x1b\[[0-9;?]*[a-zA-Z]//g' "$WORK/terminal.out" | tr '\r' '\n' |
  awk '/nota: recorded to/ { on = 1; sub(/.*nota: recorded to/, "nota: recorded to") }
    /^Script done/ { on = 0 } on && NF' | tee "$WORK/summary.txt"

for m in "${MODULES[@]}"; do pactl unload-module "$m" || true; done
MODULES=()

status=0
python3 -I "$ANALYSER" "$WORK" || status=$?
# A run that failed keeps its directory, whatever --keep says.
[[ $status -ne 0 ]] || FINISHED=1
exit $status
