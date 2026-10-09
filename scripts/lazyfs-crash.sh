#!/usr/bin/env bash
# Crash checks for the recorder on a real filesystem, through LazyFS
# (https://github.com/dsrhaslab/lazyfs), a FUSE filesystem that keeps
# unsynced file data in its own cache and can drop it on command.
#
# Not run in CI: it needs FUSE, a LazyFS build and a few minutes. Run it by
# hand after changing the write path, journal, salvage or the store.
#
# It runs three kinds of crash point, numbered in this order:
#
#   ops    after the Nth disk-changing recorder operation. `lazyfs_crash
#          write --stop-after N` stops dead there and the script SIGKILLs
#          it, then `lazyfs::clear-cache` drops everything not fsync'd, as
#          after a power cut.
#   sqlite inside SQLite's own I/O, which the recorder's counter can't see:
#          LazyFS crashes itself (losing everything not fsync'd) after the
#          Nth write or fsync of `library.db-wal` (the store's set-up and
#          every segment-row commit) or `library.db` (set-up, and the
#          checkpoint when the store closes).
#   torn   a torn write: the Nth write to a journal, a segment's temp file,
#          `library.db-wal` or `library.db` is split in two, only one half
#          (the first or the second) reaches the disk, and LazyFS crashes.
#
# After each crash, on a fresh mount of what reached the disk:
#   1. `lazyfs_crash check`: salvage, then the invariants, against the
#      promises the writer logged outside the mount;
#   2. clear the cache and `check --recovered`: what salvage did must have
#      been durable.
#
# The ops points are numbered from one uncrashed run on the plain disk; the
# sqlite and torn points from one uncrashed run on LazyFS with its
# operation log on, counting each file's writes and fsyncs. The journals
# are fsync'd on a thread per track, as `nota record` does it, so the order
# of the recorder's operations (and a little of their count) varies from
# run to run; an ops point past the end of a run that finished cleanly is
# reported as unreached, not failed. Each file's own writes and fsyncs
# don't vary, so a sqlite or torn point whose fault never fires fails: it
# means the fault no longer lands where the script thinks. A run that fails
# before its point fails it.
#
# LazyFS loses unsynced file data and sizes, but not directory entries:
# creates, renames and unlinks reach the disk at once. So this can't catch a
# missing directory fsync; the in-memory crash tests cover that.
#
# Usage: scripts/lazyfs-crash.sh [--only KIND] [--step K] [--from N]
#                                [--to N] [--scratch DIR] [--keep]
#   LAZYFS   the LazyFS binary
#            (default ~/.local/share/nota/lazyfs/lazyfs/build/lazyfs)
#   --only KIND    run only the ops, sqlite or torn points (default: all)
#   --step K run every Kth crash point (default 1: all of them)
#   --from N, --to N  the first and last point to run, in the numbering
#            the script prints
#   --scratch DIR  where to make the work directory (default ${TMPDIR:-/tmp})
#   --keep   keep the work directory even when every point passes
#
# Failing points keep their backing directory, promises and logs under the
# work directory.

set -Eeuo pipefail

LAZYFS=${LAZYFS:-$HOME/.local/share/nota/lazyfs/lazyfs/build/lazyfs}
STEP=1
FROM=1
TO=
SCRATCH=${TMPDIR:-/tmp}
KEEP=0
ONLY=

usage() { sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; }

while [[ $# -gt 0 ]]; do
  case $1 in
    --only | --step | --from | --to | --scratch)
      [[ $# -ge 2 ]] || { echo "$1 needs a value" >&2; usage >&2; exit 2; }
      case $1 in
        --only) ONLY=$2 ;;
        --step) STEP=$2 ;;
        --from) FROM=$2 ;;
        --to) TO=$2 ;;
        --scratch) SCRATCH=$2 ;;
      esac
      shift 2
      ;;
    --keep) KEEP=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

die() { echo "lazyfs-crash: $*" >&2; exit 2; }

[[ -x $LAZYFS ]] || die "no LazyFS binary at $LAZYFS (set LAZYFS)"
command -v fusermount3 > /dev/null || die "fusermount3 not found"
# At most 9 digits, so bash arithmetic can't overflow.
[[ $STEP =~ ^[1-9][0-9]{0,8}$ ]] || die "--step needs a positive number"
[[ $FROM =~ ^[1-9][0-9]{0,8}$ ]] || die "--from needs a positive number"
[[ -z $TO || $TO =~ ^[1-9][0-9]{0,8}$ ]] || die "--to needs a positive number"
[[ -z $TO || $FROM -le $TO ]] || die "--from $FROM is after --to $TO"
[[ -z $ONLY || $ONLY =~ ^(ops|sqlite|torn)$ ]] || die "--only needs ops, sqlite or torn"

REPO=$(cd "$(dirname "$0")/.." && pwd)
cargo build --manifest-path "$REPO/Cargo.toml" --example lazyfs_crash --locked --quiet
BIN=${CARGO_TARGET_DIR:-$REPO/target}/debug/examples/lazyfs_crash
[[ -x $BIN ]] || die "built, but no binary at $BIN"

# Absolute: LazyFS names a fault's file by its full path in the backing
# directory.
WORK=$(cd "$(mktemp -d "$SCRATCH/nota-lazyfs.XXXXXX")" && pwd)
# LazyFS reads a torn write's path as a regular expression, and the mount
# options are comma-separated: keep to characters that mean only themselves.
if [[ ! $WORK =~ ^[A-Za-z0-9._/-]+$ ]]; then
  rmdir "$WORK"
  die "the work directory $WORK has characters LazyFS can't take; pick another --scratch"
fi

# State for cleanup: only processes and mounts this script started.
LZ_PID=
WRITER_PID=
MNT=
DONE_FD=

unmount() {
  if [[ -n $MNT ]]; then
    if mountpoint -q "$MNT"; then
      fusermount3 -u "$MNT" 2> /dev/null || fusermount3 -uz "$MNT" 2> /dev/null || true
    elif [[ -n $LZ_PID ]] && ! kill -0 "$LZ_PID" 2> /dev/null; then
      # LazyFS died: the mount is left "not connected".
      fusermount3 -uz "$MNT" 2> /dev/null || true
    fi
  fi
  if [[ -n $LZ_PID ]]; then
    # LazyFS exits once unmounted (with status 134: it aborts on every exit).
    # One that never mounted, or hangs, is killed by its PID.
    if ! wait_for 10 lazyfs_gone; then
      kill -9 "$LZ_PID" 2> /dev/null || true
      # A killed LazyFS that had mounted leaves it "not connected".
      [[ -z $MNT ]] || fusermount3 -uz "$MNT" 2> /dev/null || true
    fi
    wait "$LZ_PID" 2> /dev/null || true
    LZ_PID=
  fi
  if [[ -n $DONE_FD ]]; then
    exec {DONE_FD}<&-
    DONE_FD=
  fi
  MNT=
}

cleanup() {
  if [[ -n $WRITER_PID ]] && kill -0 "$WRITER_PID" 2> /dev/null; then
    kill -9 "$WRITER_PID" 2> /dev/null || true
    wait "$WRITER_PID" 2> /dev/null || true
  fi
  unmount
}
trap cleanup EXIT
# Exit status 1 means failed points; anything unexpected is 2, with a line.
trap 'echo "lazyfs-crash: unexpected failure at line $LINENO" >&2; exit 2' ERR
trap 'exit 130' INT TERM

lazyfs_gone() { ! kill -0 "$LZ_PID" 2> /dev/null; }

# Waits up to $1 seconds for the command after it to succeed.
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

# mount_lazyfs DIR [LOG] [INJECTION]: backing DIR/root, mount DIR/mnt. With
# LOG=1, LazyFS logs every operation to DIR/lazyfs.log; INJECTION is a TOML
# `[[injection]]` table, a fault LazyFS injects by itself.
mount_lazyfs() {
  local dir=$1 log=${2:-0} injection=${3:-} log_all=false
  [[ $log -eq 0 ]] || log_all=true
  mkdir -p "$dir/root" "$dir/mnt"
  # Made here, before LazyFS starts, so opening the ack FIFO below can't
  # race LazyFS creating it (and make a plain file instead). A remount
  # after a crash reuses them.
  [[ -p $dir/faults.fifo ]] || mkfifo "$dir/faults.fifo"
  [[ -p $dir/done.fifo ]] || mkfifo "$dir/done.fifo"
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
log_all_operations=$log_all
logfile="$dir/lazyfs.log"
$injection
EOF
  # No core files: LazyFS aborts on every exit (a joinable thread at exit).
  (
    ulimit -c 0
    exec "$LAZYFS" "$dir/mnt" --config-path "$dir/lazyfs.toml" \
      -o modules=subdir -o subdir="$dir/root" -f
  ) >> "$dir/lazyfs.out" 2>&1 &
  LZ_PID=$!
  MNT=$dir/mnt
  # LazyFS's fault worker blocks opening the ack FIFO until a reader has
  # it open; hold it read-write for the whole mount, so LazyFS never sees
  # EOF or SIGPIPE.
  exec {DONE_FD}<> "$dir/done.fifo"
  wait_for 10 mountpoint -q "$dir/mnt" || die "LazyFS didn't mount (see $dir/lazyfs.out)"
}

# Drops every unsynced byte, and waits for LazyFS to say it's done. One
# command at a time: LazyFS reads commands that arrive together as one.
clear_cache() {
  local dir=$1 ack
  kill -0 "$LZ_PID" 2> /dev/null || die "LazyFS isn't running"
  printf 'lazyfs::clear-cache\n' > "$dir/faults.fifo"
  read -r -t 30 -u "$DONE_FD" ack || die "no clear-cache ack from LazyFS"
  [[ $ack == finished::clear-cache ]] || die "unexpected ack: $ack"
}

# How many operations a full run does, on the plain disk.
mkdir "$WORK/count" "$WORK/count/rec"
OPS=$("$BIN" write "$WORK/count/rec" "$WORK/count/promises" | sed -n 's/^ops //p') ||
  die "the write workload failed on the plain disk"
[[ $OPS =~ ^[0-9]+$ ]] || die "the write workload didn't report its operation count"
"$BIN" check "$WORK/count/rec" "$WORK/count/promises" > /dev/null ||
  die "an uncrashed run fails its own check"

# Each file's writes and fsyncs in a full run on LazyFS, from its log, as
# lines of "<op> <path under rec/> <count>".
mount_lazyfs "$WORK/files" 1
mkdir "$WORK/files/mnt/rec"
"$BIN" write "$WORK/files/mnt/rec" "$WORK/files/promises" > /dev/null ||
  die "the write workload failed on LazyFS (see $WORK/files)"
unmount
awk -v root="$WORK/files/root/rec/" '
  match($0, /lfs_(write|fsync)\(path=[^,)]*/) {
    call = substr($0, RSTART + 4, RLENGTH - 4)
    op = substr(call, 1, index(call, "(") - 1)
    path = substr(call, index(call, "=") + 1)
    if (index(path, root) == 1) count[op " " substr(path, length(root) + 1)]++
  }
  END { for (k in count) print k, count[k] }
' "$WORK/files/lazyfs.log" | LC_ALL=C sort > "$WORK/counts"

# The points, one per line: "ops N", "sqlite <op> <file> <k>" (crash after
# the kth op on the file) or "torn <file> <k> <half>" (the kth write to the
# file, with only that half on the disk).
POINTS=$WORK/points
: > "$POINTS"
if [[ -z $ONLY || $ONLY == ops ]]; then
  for ((n = 1; n <= OPS; n++)); do echo "ops $n"; done >> "$POINTS"
fi
if [[ -z $ONLY || $ONLY == sqlite ]]; then
  awk '$2 == "library.db-wal" || $2 == "library.db" {
    for (k = 1; k <= $3; k++) print "sqlite", $1, $2, k
  }' "$WORK/counts" >> "$POINTS"
fi
if [[ -z $ONLY || $ONLY == torn ]]; then
  awk '$1 == "write" && ($2 ~ /^session\/journal-/ || $2 ~ /^session\/seg-.*\.flac\.tmp$/ ||
                       $2 == "library.db-wal" || $2 == "library.db") {
    for (k = 1; k <= $3; k++) for (half = 1; half <= 2; half++) print "torn", $2, k, half
  }' "$WORK/counts" >> "$POINTS"
fi
mapfile -t POINT_LINES < "$POINTS"
TOTAL=${#POINT_LINES[@]}
[[ $TOTAL -gt 0 ]] || die "no crash points (see $WORK/counts)"
# A pattern that stopped matching would drop a whole kind of point quietly.
if [[ -z $ONLY || $ONLY == sqlite ]]; then
  grep -q '^sqlite write library.db-wal ' "$POINTS" ||
    die "no writes to library.db-wal in LazyFS's log (see $WORK/counts)"
fi
if [[ -z $ONLY || $ONLY == torn ]]; then
  grep -q '^torn session/journal-' "$POINTS" ||
    die "no journal writes in LazyFS's log (see $WORK/counts)"
  grep -q '^torn session/seg-.*\.flac\.tmp ' "$POINTS" ||
    die "no segment temp-file writes in LazyFS's log (see $WORK/counts)"
fi
TO=${TO:-$TOTAL}
[[ $TO -le $TOTAL ]] || TO=$TOTAL
[[ $FROM -le $TO ]] || die "--from $FROM is past the last point, $TO"

KINDS=$(awk '{ n[$1]++ } END { printf "%d ops, %d sqlite, %d torn", n["ops"], n["sqlite"], n["torn"] }' "$POINTS")
echo "LazyFS crash checks: $TOTAL points ($KINDS); points $FROM..$TO step $STEP; work dir $WORK"

PASS=0
FAILED=()
START=$SECONDS

# What point N does, in words.
describe() {
  local kind=$1
  shift
  case $kind in
    ops) echo "after recorder operation $1" ;;
    sqlite) echo "after $1 $3 of $2" ;;
    torn) echo "write $2 of $1 torn, only half $3 on disk" ;;
  esac
}

# fail N MESSAGE [OUTPUT]: records point N's failure in point-N/result.
fail() {
  local n=$1 message=$2 output=${3:-}
  {
    echo "point $n (${POINT_DESC}): $message"
    [[ -z $output ]] || cat "$output"
  } > "$WORK/point-$n/result"
}

# check_point N: on a mount of what reached the disk, checks the invariants,
# drops the cache and checks again. Unmounts; a passed point is removed.
check_point() {
  local n=$1
  local dir=$WORK/point-$n
  if ! "$BIN" check "$dir/mnt/rec" "$dir/promises" > "$dir/check.out" 2>&1; then
    fail "$n" "after the crash" "$dir/check.out"
    unmount
    return 0
  fi
  clear_cache "$dir"
  if ! "$BIN" check "$dir/mnt/rec" "$dir/promises" --recovered > "$dir/recheck.out" 2>&1; then
    fail "$n" "after salvage and a second crash" "$dir/recheck.out"
    unmount
    return 0
  fi
  unmount
  rm -rf "$dir"
}

# unreached N: point N's run finished before it.
unreached() {
  rm -rf "$WORK/point-$1"
  : > "$WORK/point-$1.unreached"
}

# run_ops_point N K: the writer stops dead after its Kth operation, is
# killed, and the cache is dropped.
run_ops_point() {
  local n=$1 k=$2
  local dir=$WORK/point-$n
  local promises=$dir/promises
  mount_lazyfs "$dir"
  mkdir "$dir/mnt/rec"

  "$BIN" write "$dir/mnt/rec" "$promises" --stop-after "$k" > "$dir/write.out" 2>&1 &
  WRITER_PID=$!
  stopped_or_done() { [[ -e $promises.stopped ]] || ! kill -0 "$WRITER_PID" 2> /dev/null; }
  wait_for 60 stopped_or_done || die "point $n: the writer neither stopped nor finished"
  if [[ ! -e $promises.stopped ]]; then
    local status=0
    wait "$WRITER_PID" || status=$?
    WRITER_PID=
    unmount
    if [[ $status -ne 0 ]]; then
      # Only a run that ended cleanly made fewer operations; one that failed
      # is a failure, kept with its output.
      fail "$n" "the writer failed (status $status) before its crash point" "$dir/write.out"
      return 0
    fi
    unreached "$n"
    return 0
  fi
  kill -9 "$WRITER_PID"
  wait "$WRITER_PID" 2> /dev/null || true
  WRITER_PID=

  clear_cache "$dir"
  check_point "$n"
}

# run_fault_point N INJECTION: LazyFS injects the fault and crashes itself
# while the writer runs; the backing directory is then mounted afresh.
run_fault_point() {
  local n=$1 injection=$2
  local dir=$WORK/point-$n
  mount_lazyfs "$dir" 0 "$injection"
  # LazyFS reports a fault it can't parse, and runs without it.
  ! grep -q '\[error\]' "$dir/lazyfs.out" ||
    die "point $n: LazyFS rejected the fault (see $dir/lazyfs.out)"
  mkdir "$dir/mnt/rec"

  "$BIN" write "$dir/mnt/rec" "$dir/promises" > "$dir/write.out" 2>&1 &
  WRITER_PID=$!
  writer_or_lazyfs_gone() { ! kill -0 "$WRITER_PID" 2> /dev/null || lazyfs_gone; }
  wait_for 60 writer_or_lazyfs_gone || die "point $n: the writer neither crashed nor finished"
  local status=0
  if ! kill -0 "$WRITER_PID" 2> /dev/null; then
    wait "$WRITER_PID" || status=$?
    WRITER_PID=
  fi
  # The writer's last call fails once LazyFS is dead; give it a moment to
  # be reaped. A LazyFS still up after that never injected its fault.
  if ! wait_for 1 lazyfs_gone; then
    unmount
    if [[ $status -ne 0 ]]; then
      fail "$n" "the writer failed (status $status) before its fault" "$dir/write.out"
      return 0
    fi
    # Each file's writes and fsyncs are the same in every run.
    fail "$n" "the writer finished and the fault never fired"
    return 0
  fi
  # Whatever the writer does now can't reach the disk.
  if [[ -n $WRITER_PID ]]; then
    kill -9 "$WRITER_PID" 2> /dev/null || true
    wait "$WRITER_PID" 2> /dev/null || true
    WRITER_PID=
  fi
  if ! grep -q "Killing LazyFS" "$dir/lazyfs.out"; then
    fail "$n" "LazyFS died without injecting its fault" "$dir/lazyfs.out"
    unmount
    return 0
  fi
  unmount
  mount_lazyfs "$dir"
  check_point "$n"
}

# run_point N KIND ARGS...: a failed point leaves its reason in
# point-N/result; a passed one removes point-N, and one this run didn't
# reach (an ops point only) leaves point-N.unreached. Called outside any `if`, so `set -e` still
# stops the script on an unexpected error.
run_point() {
  local n=$1 kind=$2
  shift 2
  local root=$WORK/point-$n/root/rec
  mkdir -p "$WORK/point-$n"
  case $kind in
    ops) run_ops_point "$n" "$1" ;;
    sqlite)
      run_fault_point "$n" "[[injection]]
type=\"clear-cache\"
from=\"$root/$2\"
timing=\"after\"
op=\"$1\"
occurrence=$3
crash=true"
      ;;
    torn)
      run_fault_point "$n" "[[injection]]
type=\"torn-op\"
file=\"$root/$1\"
occurrence=$2
parts=2
persist=[$3]"
      ;;
    *) die "unknown point kind $kind" ;;
  esac
}

UNREACHED=0
for ((n = FROM; n <= TO; n += STEP)); do
  read -r -a POINT <<< "${POINT_LINES[n - 1]}"
  POINT_DESC=$(describe "${POINT[@]}")
  run_point "$n" "${POINT[@]}"
  if [[ -e $WORK/point-$n.unreached ]]; then
    UNREACHED=$((UNREACHED + 1))
    echo "  point $n unreached (${POINT_DESC}): this run made fewer operations"
  elif [[ -e $WORK/point-$n/result ]]; then
    FAILED+=("$n")
    sed 's/^/  /' "$WORK/point-$n/result" >&2
  else
    PASS=$((PASS + 1))
  fi
  if (((n - FROM) / STEP % 20 == 19)); then
    echo "  ... point $n: $PASS passed, ${#FAILED[@]} failed ($((SECONDS - START))s)"
  fi
done

RAN=$((PASS + ${#FAILED[@]}))
echo "Ran $RAN crash points in $((SECONDS - START))s: $PASS passed, ${#FAILED[@]} failed, $UNREACHED unreached."
if [[ ${#FAILED[@]} -gt 0 ]]; then
  echo "Failed points: ${FAILED[*]} (details in $WORK/point-N/)"
  exit 1
fi
if [[ $KEEP -eq 0 ]]; then
  rm -rf "$WORK"
else
  echo "Kept $WORK"
fi
