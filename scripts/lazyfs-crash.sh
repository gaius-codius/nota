#!/usr/bin/env bash
# Crash checks for the recorder on a real filesystem, through LazyFS
# (https://github.com/dsrhaslab/lazyfs), a FUSE filesystem that keeps
# unsynced file data in its own cache and can drop it on command.
#
# Not run in CI: it needs FUSE, a LazyFS build and a few minutes. Run it by
# hand after changing the write path, journal, salvage or the store.
#
# It runs five kinds of crash point, numbered in this order:
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
#   reorder  unsynced writes reordered: in the Gth group of writes to a
#          journal or `library.db-wal` between two of its fsyncs (a WAL
#          group is one commit's frames), the group's writes up to the Ith
#          (I >= 2) reach the disk except the one before it, and LazyFS
#          crashes (LazyFS's `torn-seq`): a later write lands without an
#          earlier one, after whatever came before that.
#   salvage  inside salvage: `lazyfs_crash write --no-publish` leaves every
#          journal unpublished, on the plain disk; on a copy of that, LazyFS
#          crashes itself (losing everything not fsync'd) after the Nth write
#          or fsync of a file `lazyfs_crash check` writes while it salvages:
#          the segments and the store's commits, and the store's opening
#          before salvage and its checkpoint into `library.db` after. (Clean
#          journals leave salvage nothing to report, so its findings file
#          isn't written here.) Journal deletes need no point of their own:
#          LazyFS writes unlinks through.
#
# After each crash, on a fresh mount of what reached the disk:
#   1. `lazyfs_crash check`: salvage, then the invariants, against the
#      promises the writer logged outside the mount; for a salvage point,
#      salvage must also end exactly where an uninterrupted salvage of the
#      same copy did (the same files, byte for byte, and the same rows);
#   2. clear the cache and `check --recovered`: what salvage did must have
#      been durable.
#
# The ops points are numbered from one uncrashed run on the plain disk; the
# sqlite, torn and reorder points from one uncrashed run on LazyFS with its
# operation log on, counting each file's writes and fsyncs, and the salvage
# points from one uncrashed salvage on LazyFS, logged the same way. The journals
# are fsync'd on a thread per track, as `nota record` does it, so the order
# of the recorder's operations (and a little of their count) varies from
# run to run; an ops point past the end of a run that finished cleanly is
# reported as unreached, not failed. Each file's own writes and fsyncs
# don't vary, so a sqlite, torn, reorder or salvage point whose fault never
# fires fails: it means the fault no longer lands where the script thinks.
# A run that fails before its point fails it. Where a journal's fsyncs fall
# among its writes (its reorder groups) could vary, since a sync thread
# makes them while the writer appends; it hasn't in practice, but a journal
# reorder point that "never fired" may be that, not a broken fault.
#
# LazyFS's torn-seq keeps the write it holds back in one slot for all
# files; with one fault per mount, as here, that slot only ever holds the
# faulted file's write.
#
# LazyFS loses unsynced file data and sizes, but not directory entries:
# creates, renames and unlinks reach the disk at once. So this can't catch a
# missing directory fsync; the in-memory crash tests cover that.
#
# Usage: scripts/lazyfs-crash.sh [--only KIND] [--step K] [--from N]
#                                [--to N] [--scratch DIR] [--keep]
#   LAZYFS   the LazyFS binary
#            (default ~/.local/share/nota/lazyfs/lazyfs/build/lazyfs)
#   --only KIND    run only the ops, sqlite, torn, reorder or salvage
#            points (default: all)
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
[[ -z $ONLY || $ONLY =~ ^(ops|sqlite|torn|reorder|salvage)$ ]] ||
  die "--only needs ops, sqlite, torn, reorder or salvage"

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

# The writes and fsyncs in LazyFS's log DIR/lazyfs.log under DIR/root/rec/,
# one per line: "<op> <path under rec/>", in order.
file_ops() {
  awk -v root="$1/root/rec/" '
    match($0, /lfs_(write|fsync)\(path=[^,)]*/) {
      call = substr($0, RSTART + 4, RLENGTH - 4)
      op = substr(call, 1, index(call, "(") - 1)
      path = substr(call, index(call, "=") + 1)
      if (index(path, root) == 1) print op, substr(path, length(root) + 1)
    }
  ' "$1/lazyfs.log"
}
# Counted per file: "<op> <path> <count>".
count_ops() { awk '{ n[$0]++ } END { for (k in n) print k, n[k] }' | LC_ALL=C sort; }
file_ops "$WORK/files" | count_ops > "$WORK/counts"
# Each group of two or more writes to a file between two of its fsyncs, the
# groups LazyFS's torn-seq counts: "<path> <group> <writes>".
file_ops "$WORK/files" | awk '
  function close_group(f) {
    if (run[f] >= 2) print f, ++groups[f], run[f]
    run[f] = 0
  }
  $1 == "write" { run[$2]++; next }
  { close_group($2) }
  END { for (f in run) close_group(f) }
' | LC_ALL=C sort -k1,1 -k2,2n > "$WORK/groups"

# Salvage's own writes and fsyncs: a recording left entirely to salvage,
# made on the plain disk (so all of it is on the disk), then salvaged once on
# LazyFS, uncrashed, where it ends in salvage-end.
if [[ -z $ONLY || $ONLY == salvage ]]; then
  mkdir -p "$WORK/salvage-base/rec"
  "$BIN" write "$WORK/salvage-base/rec" "$WORK/salvage-base/promises" --no-publish > /dev/null ||
    die "the write workload failed with --no-publish"
  mkdir -p "$WORK/salvage-ref/root"
  cp -a "$WORK/salvage-base/rec" "$WORK/salvage-ref/root/rec"
  mount_lazyfs "$WORK/salvage-ref" 1
  "$BIN" check "$WORK/salvage-ref/mnt/rec" "$WORK/salvage-base/promises" \
    --end "$WORK/salvage-end" > /dev/null ||
    die "an uncrashed salvage fails its own check (see $WORK/salvage-ref)"
  unmount
  file_ops "$WORK/salvage-ref" | count_ops > "$WORK/salvage-counts"
fi

# The points, one per line: "ops N", "sqlite <op> <file> <k>" (crash after
# the kth op on the file), "torn <file> <k> <half>" (the kth write to the
# file, with only that half on the disk), "reorder <file> <group> <i>" (the
# group's writes up to the ith on the disk, except the one before it) or
# "salvage <op> <file> <k>" (as sqlite, while salvaging).
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
if [[ -z $ONLY || $ONLY == reorder ]]; then
  awk '$1 ~ /^session\/journal-/ || $1 == "library.db-wal" {
    for (i = 2; i <= $3; i++) print "reorder", $1, $2, i
  }' "$WORK/groups" >> "$POINTS"
fi
if [[ -z $ONLY || $ONLY == salvage ]]; then
  # The shared-memory index isn't data; SQLite rebuilds it.
  awk '$2 != "library.db-shm" { for (k = 1; k <= $3; k++) print "salvage", $1, $2, k }' \
    "$WORK/salvage-counts" >> "$POINTS"
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
if [[ -z $ONLY || $ONLY == reorder ]]; then
  grep -q '^reorder session/journal-' "$POINTS" ||
    die "no journal write groups in LazyFS's log (see $WORK/groups)"
  grep -q '^reorder library.db-wal ' "$POINTS" ||
    die "no library.db-wal write groups in LazyFS's log (see $WORK/groups)"
fi
if [[ -z $ONLY || $ONLY == salvage ]]; then
  grep -q '^salvage write session/seg-.*\.flac\.tmp ' "$POINTS" ||
    die "no segment writes in salvage's log (see $WORK/salvage-counts)"
  grep -q '^salvage fsync library.db-wal ' "$POINTS" ||
    die "no store commits in salvage's log (see $WORK/salvage-counts)"
fi
TO=${TO:-$TOTAL}
[[ $TO -le $TOTAL ]] || TO=$TOTAL
[[ $FROM -le $TO ]] || die "--from $FROM is past the last point, $TO"

KINDS=$(awk '{ n[$1]++ } END {
  printf "%d ops, %d sqlite, %d torn, %d reorder, %d salvage",
    n["ops"], n["sqlite"], n["torn"], n["reorder"], n["salvage"]
}' "$POINTS")
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
    reorder) echo "write group $2 of $1: writes up to $3 on disk except $(($3 - 1))" ;;
    salvage) echo "in salvage, after $1 $3 of $2" ;;
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

# check_point N [END]: on a mount of what reached the disk, checks the
# invariants, drops the cache and checks again. With END, salvage must end
# exactly as recorded there. Unmounts; a passed point is removed.
check_point() {
  local n=$1 end=${2:-}
  local dir=$WORK/point-$n
  if ! "$BIN" check "$dir/mnt/rec" "$dir/promises" --end "$dir/end" > "$dir/check.out" 2>&1; then
    fail "$n" "after the crash" "$dir/check.out"
    unmount
    return 0
  fi
  if [[ -n $end ]] && ! diff "$end" "$dir/end" > "$dir/end.diff"; then
    fail "$n" "salvage ended differently from an uninterrupted salvage" "$dir/end.diff"
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

# run_fault_point N RUN INJECTION: LazyFS injects the fault and crashes
# itself while RUN runs: `write` records afresh; `salvage` checks (and so
# salvages) a copy of the unpublished recording. The backing directory is
# then mounted afresh.
run_fault_point() {
  local n=$1 run=$2 injection=$3
  local dir=$WORK/point-$n
  local end='' what="the writer"
  local cmd=("$BIN" write "$dir/mnt/rec" "$dir/promises")
  if [[ $run == salvage ]]; then
    cp -a "$WORK/salvage-base/rec" "$dir/root/rec"
    cp "$WORK/salvage-base/promises" "$dir/promises"
    end=$WORK/salvage-end
    what="salvage"
    cmd=("$BIN" check "$dir/mnt/rec" "$dir/promises")
  fi
  mount_lazyfs "$dir" 0 "$injection"
  # LazyFS reports a fault it can't parse, and runs without it.
  ! grep -q '\[error\]' "$dir/lazyfs.out" ||
    die "point $n: LazyFS rejected the fault (see $dir/lazyfs.out)"
  [[ $run == salvage ]] || mkdir "$dir/mnt/rec"

  "${cmd[@]}" > "$dir/write.out" 2>&1 &
  WRITER_PID=$!
  writer_or_lazyfs_gone() { ! kill -0 "$WRITER_PID" 2> /dev/null || lazyfs_gone; }
  wait_for 60 writer_or_lazyfs_gone || die "point $n: $what neither crashed nor finished"
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
      fail "$n" "$what failed (status $status) before its fault" "$dir/write.out"
      return 0
    fi
    # Each file's writes and fsyncs are the same in every run.
    fail "$n" "$what finished and the fault never fired"
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
  check_point "$n" "$end"
}

# run_point N KIND ARGS...: a failed point leaves its reason in
# point-N/result; a passed one removes point-N, and one this run didn't
# reach (an ops point only) leaves point-N.unreached. Called outside any `if`, so `set -e` still
# stops the script on an unexpected error.
run_point() {
  local n=$1 kind=$2
  shift 2
  local root=$WORK/point-$n/root/rec
  mkdir -p "$WORK/point-$n/root"
  case $kind in
    ops) run_ops_point "$n" "$1" ;;
    sqlite | salvage)
      local run=write
      [[ $kind == sqlite ]] || run=salvage
      run_fault_point "$n" "$run" "[[injection]]
type=\"clear-cache\"
from=\"$root/$2\"
timing=\"after\"
op=\"$1\"
occurrence=$3
crash=true"
      ;;
    torn)
      run_fault_point "$n" write "[[injection]]
type=\"torn-op\"
file=\"$root/$1\"
occurrence=$2
parts=2
persist=[$3]"
      ;;
    reorder)
      # Writes 1..I-2 and I: LazyFS crashes after the last.
      local persist=$3 i
      for ((i = $3 - 2; i >= 1; i--)); do persist="$i,$persist"; done
      run_fault_point "$n" write "[[injection]]
type=\"torn-seq\"
op=\"write\"
file=\"$root/$1\"
occurrence=$2
persist=[$persist]"
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
