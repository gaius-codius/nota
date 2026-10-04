#!/usr/bin/env bash
# The M1a bounded-loss criteria end to end with real capture: the recorder
# captures speech through PipeWire and is killed after each disk operation
# in turn, then salvage runs and the result is checked (the `real_capture`
# example).
#
# Speech (the test fixture, looped) plays into a temporary null sink, and
# the recorder captures that sink by name, so nothing is heard and the
# user's devices, defaults and volumes are left alone. The sink and the
# player are removed on exit, by module index and PID.
#
# For each crash point N (the Nth disk-changing recorder operation, out of
# the operations an uncrashed run of the same length makes):
#   kill   the recorder records on the plain disk, stops dead after
#          operation N and is SIGKILLed: a recorder kill.
#   power  the same on a LazyFS mount, then `lazyfs::clear-cache`: every
#          write since the last fsync is gone, as after a power cut.
# Then `real_capture check` salvages and checks the recovered audio against
# what the recorder captured, the fsync'd positions it logged and the rows
# it committed, and that a second salvage changes nothing. A copy of the
# crashed disk is also salvaged with salvage itself killed partway (after
# operation 1 + N mod its operation count, row commits included) and re-run;
# it must end in the same files and rows.
#
# Operations are numbered as they happen, and their count varies a little
# from run to run (capture timing), so the points are numbered from one
# uncrashed run; a point past the end of a shorter run is reported as
# unreached, not failed. The summary counts points by the operation they
# stopped at.
#
# Run it on a real disk: on tmpfs every fsync is free, so the lag measures
# nothing. The default scratch directory is under ~/.cache, and tmpfs is
# refused unless --allow-tmpfs.
#
# Not run in CI: it needs a running PipeWire (with pipewire-pulse for
# `pactl`), `pw-play`, `ffmpeg`, the test fixture
# (scripts/fetch-test-models.sh), and for `power` FUSE and a LazyFS build
# (see scripts/lazyfs-crash.sh). Each point records in real time, so a full
# run takes a while: about an hour for both modes at the defaults.
#
# Usage: scripts/real-capture-crash.sh [--mode kill|power|both] [--seconds S]
#          [--segment-seconds K] [--step K] [--from N] [--to N]
#          [--scratch DIR] [--allow-tmpfs] [--keep]
#   --mode             which crashes (default both)
#   --seconds S        recording length per point (default 8)
#   --segment-seconds  segment window, short so publishing runs (default 2)
#   --step K           every Kth crash point (default 1: all of them)
#   NOTA_TEST_MODELS   the test models (default ~/.local/share/nota/test-models)
#   LAZYFS             the LazyFS binary
#
# Prints one line per point and a summary; failing points keep their
# directory under the work directory.

set -euo pipefail

LAZYFS=${LAZYFS:-$HOME/.local/share/nota/lazyfs/lazyfs/build/lazyfs}
MODELS=${NOTA_TEST_MODELS:-$HOME/.local/share/nota/test-models}
MODE=both
SECONDS_PER_POINT=8
SEGMENT=2
STEP=1
FROM=1
TO=
SCRATCH=${XDG_CACHE_HOME:-$HOME/.cache}/nota
ALLOW_TMPFS=0
KEEP=0

usage() { sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; }

while [[ $# -gt 0 ]]; do
  case $1 in
    --mode) MODE=$2; shift 2 ;;
    --seconds) SECONDS_PER_POINT=$2; shift 2 ;;
    --segment-seconds) SEGMENT=$2; shift 2 ;;
    --step) STEP=$2; shift 2 ;;
    --from) FROM=$2; shift 2 ;;
    --to) TO=$2; shift 2 ;;
    --scratch) SCRATCH=$2; shift 2 ;;
    --allow-tmpfs) ALLOW_TMPFS=1; shift ;;
    --keep) KEEP=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

die() { echo "real-capture-crash: $*" >&2; exit 2; }

case $MODE in
  kill) MODES=(kill) ;;
  power) MODES=(power) ;;
  both) MODES=(kill power) ;;
  *) die "--mode is kill, power or both" ;;
esac
[[ $STEP =~ ^[1-9][0-9]*$ ]] || die "--step needs a positive number"
FIXTURE=$MODELS/fixtures/invented-lecture.wav
[[ -f $FIXTURE ]] || die "no fixture at $FIXTURE (run scripts/fetch-test-models.sh)"
if [[ $MODE != kill ]]; then
  [[ -x $LAZYFS ]] || die "no LazyFS binary at $LAZYFS (set LAZYFS)"
  command -v fusermount3 > /dev/null || die "fusermount3 not found"
fi

REPO=$(cd "$(dirname "$0")/.." && pwd)
cargo build --manifest-path "$REPO/Cargo.toml" --release --locked --quiet \
  -p nota-recorder --example real_capture
BIN=${CARGO_TARGET_DIR:-$REPO/target}/release/examples/real_capture
[[ -x $BIN ]] || die "built, but no binary at $BIN"

mkdir -p "$SCRATCH"
if [[ $(findmnt -n -o FSTYPE --target "$SCRATCH") == tmpfs && $ALLOW_TMPFS -eq 0 ]]; then
  die "$SCRATCH is on tmpfs, where fsync is free (use --scratch on a real disk, or --allow-tmpfs)"
fi
WORK=$(mktemp -d "$SCRATCH/nota-real-crash.XXXXXX")
SINK="nota_real_crash_$$"

# State for cleanup: only processes, modules and mounts this script started.
MODULE=
PLAYER=
LZ_PID=
WRITER_PID=
MNT=
DONE_FD=

unmount() {
  if [[ -n $MNT ]]; then
    if mountpoint -q "$MNT"; then
      fusermount3 -u "$MNT" 2> /dev/null || fusermount3 -uz "$MNT" 2> /dev/null || true
    elif [[ -n $LZ_PID ]] && ! kill -0 "$LZ_PID" 2> /dev/null; then
      fusermount3 -uz "$MNT" 2> /dev/null || true
    fi
  fi
  if [[ -n $LZ_PID ]]; then
    # LazyFS exits once unmounted (with status 134: it aborts on every exit).
    # One that never mounted, or hangs, is killed by its PID.
    lazyfs_gone() { ! kill -0 "$LZ_PID" 2> /dev/null; }
    wait_for 10 lazyfs_gone || kill -9 "$LZ_PID" 2> /dev/null || true
    wait "$LZ_PID" 2> /dev/null || true
    LZ_PID=
  fi
  if [[ -n $DONE_FD ]]; then
    exec {DONE_FD}<&-
    DONE_FD=
  fi
  MNT=
}

kill_writer() {
  if [[ -n $WRITER_PID ]] && kill -0 "$WRITER_PID" 2> /dev/null; then
    kill -9 "$WRITER_PID" 2> /dev/null || true
  fi
  [[ -n $WRITER_PID ]] && wait "$WRITER_PID" 2> /dev/null || true
  WRITER_PID=
}

cleanup() {
  kill_writer
  unmount
  [[ -n $PLAYER ]] && kill "$PLAYER" 2> /dev/null || true
  [[ -n $MODULE ]] && pactl unload-module "$MODULE" || true
}
trap cleanup EXIT
trap 'exit 130' INT TERM

wait_for() {
  local limit=$1 tries
  shift
  tries=$((limit * 50))
  while ! "$@"; do
    tries=$((tries - 1))
    [[ $tries -gt 0 ]] || return 1
    sleep 0.02
  done
}

mount_lazyfs() {
  local dir=$1
  mkdir -p "$dir/root" "$dir/mnt"
  mkfifo "$dir/faults.fifo" "$dir/done.fifo"
  cat > "$dir/lazyfs.toml" << EOF
[faults]
fifo_path="$dir/faults.fifo"
fifo_path_completed="$dir/done.fifo"
[cache]
apply_eviction=false
[cache.simple]
custom_size="64mb"
blocks_per_page=1
[filesystem]
log_all_operations=false
logfile="$dir/lazyfs.log"
EOF
  (
    ulimit -c 0
    exec "$LAZYFS" "$dir/mnt" --config-path "$dir/lazyfs.toml" \
      -o modules=subdir -o subdir="$dir/root" -f
  ) >> "$dir/lazyfs.out" 2>&1 &
  LZ_PID=$!
  MNT=$dir/mnt
  exec {DONE_FD}<> "$dir/done.fifo"
  wait_for 10 mountpoint -q "$dir/mnt" || die "LazyFS didn't mount (see $dir/lazyfs.out)"
}

clear_cache() {
  local dir=$1 ack
  kill -0 "$LZ_PID" 2> /dev/null || die "LazyFS isn't running"
  printf 'lazyfs::clear-cache\n' > "$dir/faults.fifo"
  read -r -t 30 -u "$DONE_FD" ack || die "no clear-cache ack from LazyFS"
  [[ $ack == finished::clear-cache ]] || die "unexpected ack: $ack"
}

# The speech, looped for longer than the whole run can take.
default_before=$(pactl get-default-sink)
MODULE=$(pactl load-module module-null-sink "sink_name=$SINK" \
  "sink_properties=node.description=nota-real-crash priority.session=1 priority.driver=1")
# PLAYER is pw-play, the pipeline's last process; ffmpeg ends on the broken
# pipe once it's gone.
ffmpeg -loglevel quiet -stream_loop -1 -i "$FIXTURE" -t 86400 -f wav - |
  pw-play --target "$SINK" - &
PLAYER=$!

# The recorder's options: this sink. Always run "$BIN" itself, never through
# a function, so a background job's PID is the recorder's.
WRITE_OPTS=(--source "$SINK" --seconds "$SECONDS_PER_POINT" --segment-seconds "$SEGMENT")
check() { "$BIN" check "$@" --segment-seconds "$SEGMENT"; }

# How many operations an uncrashed run makes, on the plain disk.
mkdir -p "$WORK/count/rec"
TOTAL=$("$BIN" write "$WORK/count/rec" "$WORK/count/log" "$WORK/count/ref" "${WRITE_OPTS[@]}" | sed -n 's/^ops \([0-9]*\).*/\1/p')
[[ $TOTAL =~ ^[0-9]+$ ]] || die "the write workload didn't report its operation count"
check "$WORK/count/rec" "$WORK/count/log" "$WORK/count/ref" > "$WORK/count/check.out" ||
  die "an uncrashed run fails its own check: $(cat "$WORK/count/check.out")"
echo "uncrashed: $(sed 's/^result //' "$WORK/count/check.out")"
# The baseline must have recorded something worth crashing: nearly all of
# its seconds, published as segments.
base_recovered=$(sed -n 's/.* recovered=\([0-9]*\) .*/\1/p' "$WORK/count/check.out")
base_rows=$(sed -n 's/.* rows=\([0-9]*\) .*/\1/p' "$WORK/count/check.out")
[[ ${base_recovered:-0} -ge $(((SECONDS_PER_POINT - 1) * 16000)) && ${base_rows:-0} -ge 2 ]] ||
  die "the uncrashed run recorded too little (recovered ${base_recovered:-0} samples, ${base_rows:-0} rows)"
TO=${TO:-$TOTAL}
[[ $TO -le $TOTAL ]] || TO=$TOTAL
[[ $FROM -le $TO ]] || die "no crash points in $FROM..$TO (a run makes $TOTAL operations)"

echo "Real-capture crash checks: modes ${MODES[*]}; $TOTAL operations in ${SECONDS_PER_POINT}s;" \
  "points $FROM..$TO step $STEP; work dir $WORK"

RESULTS=$WORK/results
: > "$RESULTS"
FAILED=()
START=$SECONDS

# interrupted DIR LOG REF N OUT: salvages a copy of DIR with salvage killed
# partway, re-runs it, and writes the state it ends in to OUT. Fails if
# salvage didn't stop where asked, or the re-run fails its check. Not run in
# a subshell, so WRITER_PID stays visible to cleanup.
interrupted() {
  local dir=$1 log=$2 ref=$3 n=$4 out=$5 ops k
  cp -a "$dir" "$dir.count"
  ops=$("$BIN" salvage "$dir.count" --segment-seconds "$SEGMENT" | sed -n 's/^ops \([0-9]*\).*/\1/p')
  rm -rf "$dir.count"
  [[ $ops =~ ^[0-9]+$ ]] || return 1
  if [[ $ops -eq 0 ]]; then
    echo "salvage_ops=0" > "$out"
    return 0
  fi
  k=$((1 + n % ops))
  cp -a "$dir" "$dir.cut"
  "$BIN" salvage "$dir.cut" --segment-seconds "$SEGMENT" --stop-after "$k" \
    --marker "$dir.cut.stopped" > /dev/null 2>&1 &
  WRITER_PID=$!
  salvage_stopped_or_done() { [[ -e $dir.cut.stopped ]] || ! kill -0 "$WRITER_PID" 2> /dev/null; }
  if ! wait_for 60 salvage_stopped_or_done || [[ ! -e $dir.cut.stopped ]]; then
    kill_writer
    echo "salvage_ops=$ops cut_at=$k never stopped there" > "$out"
    return 1
  fi
  kill_writer
  local state
  state=$(check "$dir.cut" "$log" "$ref" | sed -n 's/^result ok .*\(state=[0-9a-f]*\).*/\1/p')
  echo "salvage_ops=$ops cut_at=$k cut_$state" > "$out"
  rm -rf "$dir.cut" "$dir.cut.stopped"
  [[ -n $state ]]
}

# run_point MODE N
run_point() {
  local mode=$1 n=$2
  local dir=$WORK/$mode-$n
  local rec log=$dir/log ref=$dir/ref
  mkdir -p "$dir"
  if [[ $mode == power ]]; then
    mount_lazyfs "$dir"
    rec=$dir/mnt/rec
  else
    rec=$dir/rec
  fi
  mkdir "$rec"

  "$BIN" write "$rec" "$log" "$ref" "${WRITE_OPTS[@]}" --stop-after "$n" > "$dir/write.out" 2>&1 &
  WRITER_PID=$!
  stopped_or_done() { [[ -e $log.stopped ]] || ! kill -0 "$WRITER_PID" 2> /dev/null; }
  if ! wait_for $((SECONDS_PER_POINT + 60)) stopped_or_done; then
    kill_writer
    echo "$mode $n: the writer neither stopped nor finished" > "$dir/result"
    unmount
    return 1
  fi
  if [[ ! -e $log.stopped ]]; then
    kill_writer
    unmount
    [[ $KEEP -eq 1 ]] || rm -rf "$dir"
    return 2
  fi
  kill_writer
  [[ $mode == power ]] && clear_cache "$dir"

  # The crashed disk, copied before salvage touches it.
  cp -a "$rec" "$dir/crashed"
  if ! check "$rec" "$log" "$ref" > "$dir/check.out" 2>&1; then
    { echo "$mode $n:"; cat "$dir/check.out"; } > "$dir/result"
    unmount
    return 1
  fi
  local line cut state
  line=$(sed -n 's/^result ok //p' "$dir/check.out")
  state=$(sed -n 's/.*state=\([0-9a-f]*\).*/\1/p' <<< "$line")
  if [[ $mode == power ]]; then
    # A second power cut after salvage: what salvage did must have been
    # durable.
    clear_cache "$dir"
    if ! check "$rec" "$log" "$ref" --recovered yes > "$dir/recheck.out" 2>&1 ||
      ! grep -q "state=$state" "$dir/recheck.out"; then
      { echo "$mode $n: after salvage and a second power cut:"; cat "$dir/recheck.out"; } > "$dir/result"
      unmount
      return 1
    fi
  fi
  if ! interrupted "$dir/crashed" "$log" "$ref" "$n" "$dir/cut.out"; then
    echo "$mode $n: salvage killed partway: $(cat "$dir/cut.out")" > "$dir/result"
    unmount
    return 1
  fi
  cut=$(cat "$dir/cut.out")
  if [[ $cut == *cut_state=* && $cut != *cut_state=$state* ]]; then
    echo "$mode $n: salvage killed partway and re-run ends differently ($cut, uninterrupted state=$state)" > "$dir/result"
    unmount
    return 1
  fi
  echo "$mode $n $line $cut" >> "$RESULTS"
  unmount
  [[ $KEEP -eq 1 ]] || rm -rf "$dir"
}

UNREACHED=0
for mode in "${MODES[@]}"; do
  for ((n = FROM; n <= TO; n += STEP)); do
    status=0
    run_point "$mode" "$n" || status=$?
    case $status in
      0) tail -n 1 "$RESULTS" ;;
      2)
        UNREACHED=$((UNREACHED + 1))
        echo "$mode $n unreached: this run made fewer operations"
        ;;
      *)
        FAILED+=("$mode-$n")
        sed 's/^/FAIL /' "$WORK/$mode-$n/result" >&2
        ;;
    esac
  done
done

if [[ "$(pactl get-default-sink)" != "$default_before" ]]; then
  # Not changed back: the user may have changed it themselves meanwhile.
  echo "real-capture-crash: the default sink changed during the run (was $default_before)" >&2
  FAILED+=(default-sink)
fi

echo
echo "Summary ($((SECONDS - START))s):"
for mode in "${MODES[@]}"; do
  awk -v mode="$mode" '
    $1 == mode {
      n++
      for (i = 3; i <= NF; i++) { split($i, kv, "="); v[kv[1]] = kv[2] }
      if (v["lag_max_ms"] + 0 > lag) lag = v["lag_max_ms"] + 0
      if (v["wall_lag_max_ms"] + 0 > wall) wall = v["wall_lag_max_ms"] + 0
      if (v["loss_ms"] + 0 > loss) loss = v["loss_ms"] + 0
      if (v["loss_ms"] + 0 > 0) lossy++
      if (v["beyond_durable_ms"] + 0 > beyond) beyond = v["beyond_durable_ms"] + 0
      if (v["cut_state"] != "") cut++
      split(v["stop"], s, ":"); kinds[s[2] ":" s[3]]++
      delete v
    }
    END {
      printf "  %s: %d points passed; max lag %.1f ms (wall clock %.1f ms); max loss %.1f ms;", mode, n, lag, wall, loss
      printf " recovered past durable up to %.1f ms; interrupted salvage matched %d times\n", beyond, cut
      printf "    points that lost unsynced audio: %d\n", lossy
      printf "    crash points by operation:"
      for (k in kinds) printf " %s=%d", k, kinds[k]
      printf "\n"
    }' "$RESULTS"
done
echo "  unreached: $UNREACHED"
echo "  failed: ${#FAILED[@]}${FAILED[*]:+ (${FAILED[*]})}"
echo "Results: $RESULTS"
if [[ ${#FAILED[@]} -gt 0 ]]; then
  exit 1
fi
