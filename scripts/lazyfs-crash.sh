#!/usr/bin/env bash
# Crash checks for the recorder on a real filesystem, through LazyFS
# (https://github.com/dsrhaslab/lazyfs), a FUSE filesystem that keeps
# unsynced file data in its own cache and can drop it on command.
#
# Not run in CI: it needs FUSE, a LazyFS build and a few minutes. Run it by
# hand after changing the write path, journal or salvage.
#
# For each crash point N (the Nth disk-changing recorder operation):
#   1. mount LazyFS on a fresh backing directory;
#   2. run `lazyfs_crash write` on the mount; it stops dead after operation N
#      and the script SIGKILLs it;
#   3. `lazyfs::clear-cache`: everything not fsync'd is gone, as after a
#      power cut;
#   4. `lazyfs_crash check` on the mount: salvage, then the invariants,
#      against the promises the writer logged outside the mount;
#   5. clear the cache again and `check --recovered`: what salvage did must
#      have been durable;
#   6. unmount.
#
# LazyFS loses unsynced file data and sizes, but not directory entries:
# creates, renames and unlinks reach the disk at once. So this can't catch a
# missing directory fsync; the in-memory crash tests cover that.
#
# Usage: scripts/lazyfs-crash.sh [--step K] [--from N] [--to N]
#                                [--scratch DIR] [--keep]
#   LAZYFS   the LazyFS binary
#            (default ~/.local/share/nota/lazyfs/lazyfs/build/lazyfs)
#   --step K run every Kth crash point (default 1: all of them)
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

usage() { sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; }

while [[ $# -gt 0 ]]; do
  case $1 in
    --step) STEP=$2; shift 2 ;;
    --from) FROM=$2; shift 2 ;;
    --to) TO=$2; shift 2 ;;
    --scratch) SCRATCH=$2; shift 2 ;;
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

REPO=$(cd "$(dirname "$0")/.." && pwd)
cargo build --manifest-path "$REPO/Cargo.toml" --example lazyfs_crash --locked --quiet
BIN=${CARGO_TARGET_DIR:-$REPO/target}/debug/examples/lazyfs_crash
[[ -x $BIN ]] || die "built, but no binary at $BIN"

WORK=$(mktemp -d "$SCRATCH/nota-lazyfs.XXXXXX")

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
    lazyfs_gone() { ! kill -0 "$LZ_PID" 2> /dev/null; }
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

# mount_lazyfs DIR: backing DIR/root, mount DIR/mnt.
mount_lazyfs() {
  local dir=$1
  mkdir -p "$dir/root" "$dir/mnt"
  # Made here, before LazyFS starts, so opening the ack FIFO below can't
  # race LazyFS creating it (and make a plain file instead).
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
TOTAL=$("$BIN" write "$WORK/count/rec" "$WORK/count/promises" | sed -n 's/^ops //p') ||
  die "the write workload failed on the plain disk"
[[ $TOTAL =~ ^[0-9]+$ ]] || die "the write workload didn't report its operation count"
"$BIN" check "$WORK/count/rec" "$WORK/count/promises" > /dev/null ||
  die "an uncrashed run fails its own check"
TO=${TO:-$TOTAL}
[[ $TO -le $TOTAL ]] || TO=$TOTAL
[[ $FROM -le $TO ]] || die "--from $FROM is past the last point, $TO (a full run has $TOTAL operations)"

echo "LazyFS crash checks: $TOTAL operations; points $FROM..$TO step $STEP; work dir $WORK"

PASS=0
FAILED=()
START=$SECONDS

# run_point N: a failed point leaves its reason in point-N/result; a passed
# one removes point-N. Called outside any `if`, so `set -e` still stops the
# script on an unexpected error.
run_point() {
  local n=$1
  local dir=$WORK/point-$n
  mkdir -p "$dir"
  mount_lazyfs "$dir"
  mkdir "$dir/mnt/rec"
  local promises=$dir/promises

  "$BIN" write "$dir/mnt/rec" "$promises" --stop-after "$n" > "$dir/write.out" 2>&1 &
  WRITER_PID=$!
  stopped_or_done() { [[ -e $promises.stopped ]] || ! kill -0 "$WRITER_PID" 2> /dev/null; }
  wait_for 60 stopped_or_done || die "point $n: the writer neither stopped nor finished"
  if [[ ! -e $promises.stopped ]]; then
    wait "$WRITER_PID" || true
    WRITER_PID=
    echo "point $n: the writer finished without reaching its crash point" > "$dir/result"
    unmount
    return 0
  fi
  kill -9 "$WRITER_PID"
  wait "$WRITER_PID" 2> /dev/null || true
  WRITER_PID=

  clear_cache "$dir"
  if ! "$BIN" check "$dir/mnt/rec" "$promises" > "$dir/check.out" 2>&1; then
    { echo "point $n: after the crash"; cat "$dir/check.out"; } > "$dir/result"
    unmount
    return 0
  fi
  clear_cache "$dir"
  if ! "$BIN" check "$dir/mnt/rec" "$promises" --recovered > "$dir/recheck.out" 2>&1; then
    { echo "point $n: after salvage and a second crash"; cat "$dir/recheck.out"; } > "$dir/result"
    unmount
    return 0
  fi
  unmount
  rm -rf "$dir"
}

for ((n = FROM; n <= TO; n += STEP)); do
  run_point "$n"
  if [[ -e $WORK/point-$n/result ]]; then
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
echo "Ran $RAN crash points in $((SECONDS - START))s: $PASS passed, ${#FAILED[@]} failed."
if [[ ${#FAILED[@]} -gt 0 ]]; then
  echo "Failed points: ${FAILED[*]} (details in $WORK/point-N/)"
  exit 1
fi
if [[ $KEEP -eq 0 ]]; then
  rm -rf "$WORK"
else
  echo "Kept $WORK"
fi
