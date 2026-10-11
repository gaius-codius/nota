#!/usr/bin/env bash
# Tests lazyfs-crash.sh without FUSE, LazyFS, models or audio: a copy of the
# script runs in a stand-in repo against stand-ins for everything outside it
# (cargo, mountpoint and fusermount3 on PATH, and the LazyFS and lazyfs_crash
# binaries). The stand-in LazyFS "mounts" by leaving a marker holding its PID
# in the mount directory, answers clear-cache on its FIFOs, exits on
# unmount and crashes, printing what LazyFS prints, on SIGUSR1; the stand-in
# lazyfs_crash logs the writes and fsyncs LazyFS would, and fires a fault by
# sending that SIGUSR1. Nothing touches a real mount or device.
#
#   1. An option without its value, and a bad value, fail with status 2 and
#      say which option, before anything is built.
#   2. A LazyFS that can't do RENAME_NOREPLACE fails with status 2 before
#      any repair point runs, and leaves no LazyFS running.
#   3. A full run with every kind of point passes from a repo, a target
#      directory and a LazyFS path that hold a space and glob characters; an
#      ops point the run doesn't reach is reported, not failed; the work
#      directory goes, and another directory and processes like the run's own
#      (a LazyFS on a mount next to the work directory, a writer) survive.
#   4. A fault that never fires fails its point, and the counts add up.
#   5. Repair points that are never reached, or never fire, fail and count
#      as unreached, not passed; one that fails its check counts as failed
#      after reaching; --step selects fewer points without counting the rest.
#   6. A scratch directory LazyFS can't take is refused with status 2, and
#      only the work directory the run made there is removed.
#   7. An interrupted run kills the writer and the LazyFS it started, and
#      nothing else.
#
# LAZYFS_CRASH_SH names the script to test (default: lazyfs-crash.sh next to
# this one), so a deliberately broken copy can be checked to fail.
#
# Requires: bash, coreutils, awk, sed, grep, mkfifo.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
script=${LAZYFS_CRASH_SH:-$here/lazyfs-crash.sh}
dir=$(mktemp -d)
# Every process the stand-ins start records its PID in a state directory;
# whatever a broken script under test leaves behind is killed by that PID.
# shellcheck disable=SC2329 # run by the EXIT trap
cleanup() {
  local pid
  cat "$dir"/state/*.pids "$dir"/decoy-state/*.pids 2>/dev/null | while read -r pid; do
    kill -9 "$pid" 2>/dev/null || true
  done || true
  rm -rf "$dir"
}
trap cleanup EXIT

status=0
fail() {
  echo "lazyfs-crash.test.sh: $*" >&2
  status=1
}

# A space and glob characters, for every path the script is given.
odd='a b*[c]'
repo="$dir/repo $odd"
bin=$dir/bin
tools="$dir/tools $odd"
target="$dir/target $odd"
export FAKE_STATE=$dir/state
mkdir -p "$repo/scripts" "$bin" "$tools" "$target/debug/examples" "$FAKE_STATE" "$dir/cwd"
cp "$script" "$repo/scripts/lazyfs-crash.sh"
chmod +x "$repo/scripts/lazyfs-crash.sh"
# The work directory may only hold characters LazyFS can take.
scratch=$dir/scratch
mkdir "$scratch"
[[ $scratch =~ ^[A-Za-z0-9._/-]+$ ]] || {
  echo "lazyfs-crash.test.sh: the temporary directory $scratch has characters LazyFS can't take; set TMPDIR" >&2
  exit 2
}
# What makes the stand-in lazyfs_crash misbehave, each an extended regular
# expression matched against its recording's path (such as /point-2/), or
# empty: finish without reaching --stop-after, hang instead of stopping,
# leave an injected fault unfired, fail the check after a crash. And
# FAKE_NO_RENAME, set for a LazyFS without RENAME_NOREPLACE.
export FAKE_UNREACHED='' FAKE_HANG='' FAKE_NOFIRE='' FAKE_CHECK_FAIL='' FAKE_NO_RENAME=''

# cargo: records its arguments, one per line, and builds nothing.
cat >"$bin/cargo" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$@" >>"$FAKE_STATE/cargo.args"
EOF
# mountpoint -q DIR: mounted while a stand-in LazyFS's marker is in DIR.
cat >"$bin/mountpoint" <<'EOF'
#!/usr/bin/env bash
[[ $1 == -q && -e $2/.fake-mounted ]]
EOF
# fusermount3 -u|-uz DIR: logs the call, and ends the stand-in LazyFS whose
# marker is in DIR. Without one, -u fails as on a directory that isn't a
# mount, and -uz succeeds as on a dead mount.
cat >"$bin/fusermount3" <<'EOF'
#!/usr/bin/env bash
printf '%s %s\n' "$1" "$2" >>"$FAKE_STATE/fusermount.log"
if [[ -e $2/.fake-mounted ]]; then
  kill -TERM "$(cat "$2/.fake-mounted")"
else
  [[ $1 == -uz ]]
fi
EOF
# The stand-in LazyFS: MOUNT --config-path FILE [...].
cat >"$tools/lazyfs" <<'EOF'
#!/usr/bin/env bash
set -u
mnt=$1
shift
config=
while [[ $# -gt 0 ]]; do
  [[ $1 != --config-path ]] || config=$2
  shift
done
echo $$ >>"$FAKE_STATE/lazyfs.pids"
faults=$(sed -n 's/^fifo_path="\(.*\)"$/\1/p' "$config")
done_fifo=$(sed -n 's/^fifo_path_completed="\(.*\)"$/\1/p' "$config")
# Read-write, so opening it never waits for a writer. Commands are read
# in a child, without a timeout (one can drop a line half read), while this
# process waits for a signal.
exec 3<>"$faults"
while read -r line <&3; do
  [[ $line != lazyfs::clear-cache ]] || echo finished::clear-cache >"$done_fifo"
done &
reader=$!
echo "$reader" >>"$FAKE_STATE/lazyfs.pids"
end=
trap 'end=unmount' TERM
trap 'end=crash' USR1
echo $$ >"$mnt/.fake-mounted"
while [[ -z $end ]]; do
  wait "$reader"
done
kill "$reader"
rm -f "$mnt/.fake-mounted"
[[ $end != crash ]] || echo "Killing LazyFS"
# LazyFS aborts on every exit.
exit 134
EOF
# The stand-in lazyfs_crash: the subcommands and options the script uses.
# A recording on a mount is DIR/mnt/rec, with LazyFS's config in DIR.
cat >"$target/debug/examples/lazyfs_crash" <<'EOF'
#!/usr/bin/env bash
set -u
command=$1
shift
case $command in
  probe-rename)
    if [[ -n $FAKE_NO_RENAME ]]; then
      echo "renameat2 with RENAME_NOREPLACE: Invalid argument" >&2
      exit 1
    fi
    exit 0
    ;;
  prepare-repair)
    mkdir -p "$1"
    : >"$2"
    exit 0
    ;;
esac
rec=$1 promises=$2
shift 2
stop='' end='' repair='' recovered=''
while [[ $# -gt 0 ]]; do
  case $1 in
    --stop-after) stop=$2; shift ;;
    --end) end=$2; shift ;;
    --repair) repair=1 ;;
    --recovered) recovered=1 ;;
  esac
  shift
done
mount=${rec%/mnt/rec}
config=$mount/lazyfs.toml
[[ $rec == */mnt/rec ]] || config=/nonexistent
matches() { [[ -n $1 && $rec =~ $1 ]]; }
# log OP FILE: a write or fsync of FILE, in LazyFS's log when it keeps one.
log() {
  if grep -qx 'log_all_operations=true' "$config" 2>/dev/null; then
    echo "[lazyfs.ops]: lfs_$1(path=$mount/root/rec/$2, size=4)" >>"$mount/lazyfs.log"
  fi
}
# stop_at OPS: with --stop-after K <= OPS, stops dead after operation K and
# waits to be killed.
stop_at() {
  [[ -n $stop ]] || return 0
  if matches "$FAKE_HANG"; then
    echo $$ >>"$FAKE_STATE/hung.pids"
    exec sleep 300
  fi
  if matches "$FAKE_UNREACHED" || [[ $stop -gt $1 ]]; then exit 0; fi
  echo $$ >>"$FAKE_STATE/stopped.pids"
  : >"$promises.stopped"
  exec sleep 300
}
# An injected fault crashes LazyFS, and the next operation fails.
fault() {
  grep -q '^\[\[injection\]\]' "$config" 2>/dev/null || return 0
  ! matches "$FAKE_NOFIRE" || return 0
  kill -USR1 "$(cat "$mount/mnt/.fake-mounted")"
  exit 1
}
if [[ $command == write ]]; then
  stop_at 3
  fault
  log write session/journal-0
  log write session/journal-0
  log fsync session/journal-0
  log write session/seg-1.flac.tmp
  log fsync session/seg-1.flac.tmp
  log write library.db-wal
  log write library.db-wal
  log fsync library.db-wal
  : >"$promises"
  echo "ops 3"
  exit 0
fi
# check
if [[ -n $repair ]]; then
  stop_at 2
  fault
  log write session/seg-1.flac.tmp
  log fsync session/seg-1.flac.tmp
  echo "ops 2"
else
  fault
  log write session/seg-1.flac.tmp
  log fsync library.db-wal
fi
[[ -z $end ]] || echo end >"$end"
if [[ -z $recovered ]] && matches "$FAKE_CHECK_FAIL"; then
  echo "a promised segment is missing"
  exit 1
fi
exit 0
EOF
chmod +x "$bin"/* "$tools/lazyfs" "$target/debug/examples/lazyfs_crash"

# Decoys the script must leave alone: a directory named like its work
# directory, a LazyFS mounted there and a writer stopped there, started
# outside any run.
decoy=$scratch/nota-lazyfs.decoy
mkdir -p "$decoy/mnt" "$decoy/rec" "$dir/decoy-state"
mkfifo "$decoy/faults.fifo" "$decoy/done.fifo"
printf 'fifo_path="%s"\nfifo_path_completed="%s"\n' "$decoy/faults.fifo" "$decoy/done.fifo" >"$decoy/lazyfs.toml"
FAKE_STATE=$dir/decoy-state "$tools/lazyfs" "$decoy/mnt" --config-path "$decoy/lazyfs.toml" >/dev/null 2>&1 &
decoy_lazyfs=$!
FAKE_STATE=$dir/decoy-state "$target/debug/examples/lazyfs_crash" write "$decoy/rec" "$decoy/promises" \
  --stop-after 1 >/dev/null 2>&1 &
decoy_writer=$!
for _ in $(seq 200); do
  [[ ! -e $decoy/mnt/.fake-mounted || ! -e $decoy/promises.stopped ]] || break
  sleep 0.05
done
[[ -e $decoy/mnt/.fake-mounted && -e $decoy/promises.stopped ]] || {
  echo "lazyfs-crash.test.sh: the decoys didn't start" >&2
  exit 2
}

# check_left NAME: nothing the run started is still running (whatever is
# gets killed, by its PID), and the decoys are untouched.
check_left() {
  local name=$1 pid
  while read -r pid; do
    if kill -0 "$pid" 2>/dev/null; then
      fail "$name: left PID $pid running: $(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null)"
      kill -9 "$pid" 2>/dev/null || true
    fi
  done < <(cat "$FAKE_STATE"/*.pids 2>/dev/null)
  kill -0 "$decoy_lazyfs" 2>/dev/null || fail "$name: killed a LazyFS it didn't start"
  kill -0 "$decoy_writer" 2>/dev/null || fail "$name: killed a writer it didn't start"
  [[ -e $decoy/mnt/.fake-mounted ]] || fail "$name: unmounted a LazyFS it didn't mount"
  [[ -d $decoy/rec ]] || fail "$name: removed a directory it didn't make"
  if grep -v " $scratch/nota-lazyfs\.[A-Za-z0-9]\{6\}/[a-z0-9-]*/mnt\$" "$FAKE_STATE/fusermount.log" \
    >"$dir/stray" 2>/dev/null; then
    fail "$name: unmounted something outside its work directory: $(cat "$dir/stray")"
  fi
}

# run NAME ARGS...: runs the script copy with the stand-ins and ARGS, from a
# fresh state, leaving its status in $code and its output in $dir/NAME.out
# and $dir/NAME.err; then check_left.
run() {
  local name=$1
  shift
  rm -rf "$FAKE_STATE"
  mkdir "$FAKE_STATE"
  code=0
  (cd "$dir/cwd" && PATH=$bin:$PATH LAZYFS=$tools/lazyfs CARGO_TARGET_DIR=$target \
    "$repo/scripts/lazyfs-crash.sh" "$@" >"$dir/$name.out" 2>"$dir/$name.err") || code=$?
  check_left "$name"
}

# expect NAME STATUS [-e|-o] TEXT...: the run NAME ended with STATUS and
# printed each TEXT (fixed strings) to stderr (-e, the default) or stdout (-o).
expect() {
  local name=$1 want=$2 file text
  shift 2
  file=$dir/$name.err
  if [[ $code -ne $want ]]; then
    fail "$name: expected status $want, got $code; stdout: $(cat "$dir/$name.out"); stderr: $(cat "$dir/$name.err")"
    return
  fi
  for text in "$@"; do
    case $text in
      -e) file=$dir/$name.err ;;
      -o) file=$dir/$name.out ;;
      *) grep -qF -- "$text" "$file" || fail "$name: expected '$text' in $(cat "$file")" ;;
    esac
  done
}

# work_dirs: the run's work directories left in the scratch directory.
work_dirs() { find "$scratch" -mindepth 1 -maxdepth 1 ! -name nota-lazyfs.decoy; }

# 1. Options without their values, or with bad ones.
for option in --only --step --from --to --scratch; do
  run missing-value "$option"
  expect missing-value 2 "$option needs a value"
  [[ ! -e $FAKE_STATE/cargo.args ]] || fail "$option without a value: built before refusing"
done
# An option where the value should be isn't taken as the value.
for option in --only --step --from --to --scratch; do
  run option-as-value "$option" --keep
  expect option-as-value 2 "$option needs a value"
  [[ ! -e $FAKE_STATE/cargo.args ]] || fail "$option --keep: built before refusing"
done
run bad-step --step 0 --scratch "$scratch"
expect bad-step 2 "lazyfs-crash: --step needs a positive number"
run bad-only --only everything --scratch "$scratch"
expect bad-only 2 "lazyfs-crash: --only needs ops, sqlite, torn, reorder, salvage or repair"

# 2. A LazyFS without RENAME_NOREPLACE.
FAKE_NO_RENAME=1 run no-rename --only repair --scratch "$scratch"
expect no-rename 2 "lazyfs-crash: repair needs a LazyFS build with RENAME_NOREPLACE support (set LAZYFS)"
! grep -q 'crash checks' "$dir/no-rename.out" || fail "no-rename: ran crash points: $(cat "$dir/no-rename.out")"
[[ -s $FAKE_STATE/lazyfs.pids ]] || fail "no-rename: never started LazyFS to probe it"
rm -rf "$(work_dirs)"

# 3. A full run, from odd paths, with ops point 3 unreached.
FAKE_UNREACHED=/point-3/ run full --scratch "$scratch"
expect full 0 -o \
  "LazyFS crash checks: 28 points (3 ops, 3 sqlite, 10 torn, 2 reorder, 2 salvage, 8 repair); points 1..28 step 1" \
  "point 3 unreached (after recorder operation 3): this run made fewer operations" \
  "27 passed, 0 failed, 1 ops unreached." \
  "Repair missing: 4 available, 4 selected, 4 reached, 4 passed, 0 failed after reaching, 0 unreached (failed)." \
  "Repair mismatched: 4 available, 4 selected, 4 reached, 4 passed, 0 failed after reaching, 0 unreached (failed)."
grep -qxF -- "$repo/Cargo.toml" "$FAKE_STATE/cargo.args" 2>/dev/null ||
  fail "full: cargo wasn't given the repo's manifest as one argument: $(cat "$FAKE_STATE/cargo.args" 2>/dev/null)"
stopped=0
[[ ! -f $FAKE_STATE/stopped.pids ]] || stopped=$(wc -l <"$FAKE_STATE/stopped.pids")
[[ $stopped -eq 6 ]] || fail "full: expected 6 stopped writers (2 ops points, 4 repair), got $stopped"
[[ -z $(work_dirs) ]] || fail "full: kept the work directory of a run that passed: $(work_dirs)"

# 4. A sqlite fault that never fires.
FAKE_NOFIRE=/point-2/ run nofire --only sqlite --scratch "$scratch"
expect nofire 1 \
  "point 2 (after write 1 of library.db-wal): the writer finished and the fault never fired" \
  -o "LazyFS crash checks: 3 points (0 ops, 3 sqlite, 0 torn, 0 reorder, 0 salvage, 0 repair)" \
  "2 passed, 1 failed, 0 ops unreached." "Failed points: 2 "
work=$(work_dirs)
[[ -f $work/point-2/result && ! -e $work/point-1 && ! -e $work/point-3 ]] ||
  fail "nofire: expected only point 2's directory kept in $work"
rm -rf "$work"

# 5. Repair points: 1-4 are the missing scenario (ops 1 and 2, then the
# fsync and the write of the segment), 5-8 the mismatched one. Point 2's
# repair finishes before its stop, point 4's fault never fires, and point 7
# fails its check after the crash.
FAKE_UNREACHED=/point-2/ FAKE_NOFIRE=/point-4/ FAKE_CHECK_FAIL=/point-7/ \
  run repair --only repair --scratch "$scratch"
expect repair 1 \
  "point 2 (in missing repair, after recorder operation 2): repair finished (status 0) without reaching operation 2" \
  "point 4 (in missing repair, after write 1 of session/seg-1.flac.tmp): repair-missing finished and the fault never fired" \
  "point 7 (in mismatched repair, after fsync 1 of session/seg-1.flac.tmp): after the crash" \
  -o "5 passed, 3 failed, 0 ops unreached." "Failed points: 2 4 7 " \
  "Repair missing: 4 available, 4 selected, 2 reached, 2 passed, 0 failed after reaching, 2 unreached (failed)." \
  "Repair mismatched: 4 available, 4 selected, 4 reached, 3 passed, 1 failed after reaching, 0 unreached (failed)."
rm -rf "$(work_dirs)"
# Every third point: 1, 4 and 7. Points not selected count nowhere.
run repair-step --only repair --step 3 --scratch "$scratch"
expect repair-step 0 -o "points 1..8 step 3" "Ran 3 crash points" "3 passed, 0 failed" \
  "Repair missing: 4 available, 2 selected, 2 reached, 2 passed, 0 failed after reaching, 0 unreached (failed)." \
  "Repair mismatched: 4 available, 1 selected, 1 reached, 1 passed, 0 failed after reaching, 0 unreached (failed)."

# 6. A scratch directory LazyFS can't take, next to a decoy matching what
# the unquoted name would split and expand to.
odd_scratch="$dir/cwd/scratch $odd"
mkdir -p "$odd_scratch/nota-lazyfs.decoy" "$dir/cwd/scratch" "$dir/cwd/b*[c]"
run odd-scratch --scratch "$odd_scratch"
expect odd-scratch 2 "has characters LazyFS can't take; pick another --scratch"
left=$(find "$odd_scratch" -mindepth 1 -maxdepth 1 ! -name nota-lazyfs.decoy)
[[ -z $left ]] || fail "odd-scratch: left its work directory: $left"
[[ -d $odd_scratch/nota-lazyfs.decoy && -d $dir/cwd/scratch && -d "$dir/cwd/b*[c]" ]] ||
  fail "odd-scratch: removed a directory it didn't make"

# 7. Interrupted while point 1's writer hangs.
rm -rf "$FAKE_STATE"
mkdir "$FAKE_STATE"
(cd "$dir/cwd" && FAKE_HANG=/point-1/ PATH=$bin:$PATH LAZYFS=$tools/lazyfs CARGO_TARGET_DIR=$target \
  exec "$repo/scripts/lazyfs-crash.sh" --only ops --scratch "$scratch" >"$dir/interrupted.out" 2>&1) &
crash_pid=$!
for _ in $(seq 200); do
  [[ ! -s $FAKE_STATE/hung.pids ]] || break
  sleep 0.05
done
if [[ -s $FAKE_STATE/hung.pids ]]; then
  kill -TERM "$crash_pid"
  # Waiting on a writer it should have killed would hang the test.
  for _ in $(seq 300); do
    kill -0 "$crash_pid" 2>/dev/null || break
    sleep 0.05
  done
  if kill -0 "$crash_pid" 2>/dev/null; then
    fail "interrupted: still running 15s after SIGTERM: $(cat "$dir/interrupted.out")"
    kill -9 "$crash_pid" 2>/dev/null || true
  fi
  code=0
  wait "$crash_pid" || code=$?
  [[ $code -eq 130 ]] || fail "interrupted: expected status 130, got $code: $(cat "$dir/interrupted.out")"
  check_left interrupted
else
  fail "interrupted: point 1's writer never started: $(cat "$dir/interrupted.out")"
  kill -9 "$crash_pid" 2>/dev/null || true
  wait "$crash_pid" 2>/dev/null || true
fi

kill "$decoy_lazyfs" "$decoy_writer" 2>/dev/null || true
wait "$decoy_lazyfs" "$decoy_writer" 2>/dev/null || true
exit "$status"
