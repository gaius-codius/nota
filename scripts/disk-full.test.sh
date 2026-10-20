#!/usr/bin/env bash
# Tests disk-full.sh --kernel-ext4 without root, loop devices or a build: a
# copy of the script runs in a stand-in repo against stand-ins for sudo,
# setpriv, id, losetup, mount, umount, findmnt, mkfs.ext4, script and cargo
# on PATH, and for the disk_full example and nota. The stand-in losetup and
# mount keep their devices and mounts in a state directory, as the kernel
# would: a device detached while it's mounted goes with its last unmount.
# Nothing touches a real device or mount.
#
#   1. Bad options fail with status 2 before sudo, and so does running it
#      with sudo (as root).
#   2. A dry run prints every command; as root, only losetup, mount and
#      umount; and runs none of them, nor sudo.
#   3. A run passes: sudo once, each run as the user on its own mounted
#      ext4 (mkfs'd as the user, root owned by them), every device and
#      mount gone, the run directory removed, and the result file says so.
#   4. A mount an automounter adds is unmounted too, and reported.
#   5. A run that never fills is stopped at the time limit and reported as
#      timed out, not passed; a failing run is reported with its output; a
#      mount that fails, or leaves something else on the mount point,
#      detaches its device and records nothing.
#   6. Ctrl-C (SIGINT to the process group), SIGTERM and SIGHUP stop the run
#      in progress, leave no device, mount or process, and say so.
#   7. --cleanup unmounts and detaches what a killed run left, and removes
#      its directory.
#
# DISK_FULL_SH names the script to test (default: disk-full.sh next to this
# one), so a deliberately broken copy can be checked to fail.
#
# Requires: bash, coreutils, python3, sed, grep, awk.
# shellcheck disable=SC2016 # the stand-ins' code, expanded when they run
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
script=${DISK_FULL_SH:-$here/disk-full.sh}
dir=$(mktemp -d)
# Whatever a broken script under test leaves running is killed by its PID.
# shellcheck disable=SC2329 # run by the EXIT trap
cleanup() {
  local pid
  cat "$dir"/state/*.pid 2> /dev/null | while read -r pid; do
    kill -9 "$pid" 2> /dev/null || true
  done || true
  rm -rf "$dir"
}
trap cleanup EXIT

status=0
fail() {
  echo "disk-full.test.sh: $*" >&2
  status=1
}

repo=$dir/repo
bin=$dir/bin
target=$dir/target
images=$dir/images
export FAKE_STATE=$dir/state
mkdir -p "$repo/scripts" "$bin" "$target/debug/examples" "$FAKE_STATE/loops"
cp "$script" "$repo/scripts/disk-full.sh"
chmod +x "$repo/scripts/disk-full.sh"
: > "$FAKE_STATE/mounts"
# How the stand-ins behave: the disk_full example (ok, fail or hang), an
# automounter that mounts each device a second time, a mount that fails, and
# one that says it worked but left another filesystem on the mount point.
export FAKE_DISK=ok FAKE_AUTOMOUNT='' FAKE_MOUNT_FAIL='' FAKE_MOUNT_WRONG=''

stub() {
  printf '#!/usr/bin/env bash\nset -euo pipefail\n%s\n' "$2" > "$bin/$1"
  chmod +x "$bin/$1"
}

# cargo: builds nothing; metadata names the stand-in target directory.
stub cargo "case \$1 in
  metadata) printf '{\"target_directory\": \"%s\"}\n' '$target' ;;
  build) echo \"cargo \$*\" >> \"\$FAKE_STATE/cargo\" ;;
esac"
# sudo: once root (FAKE_ROOT), runs the command; counts the calls.
stub sudo 'echo "sudo $*" >> "$FAKE_STATE/sudo"
[[ $1 == -- ]] && shift
FAKE_ROOT=1 exec "$@"'
# id -u: 0 as root.
stub id 'if [[ ${FAKE_ROOT-} == 1 && $* == -u ]]; then echo 0; else exec /usr/bin/id "$@"; fi'
# setpriv: no longer root; checks it's asked for the user and a clean
# environment.
stub setpriv 'want="--reuid=$(/usr/bin/id -u) --regid=$(/usr/bin/id -g) --init-groups --reset-env --"
[[ "$1 $2 $3 $4 $5" == "$want" ]] || { echo "setpriv: wrong options: $*" >&2; exit 99; }
shift 5
[[ ${FAKE_ROOT-} == 1 ]] || { echo "setpriv: not root" >&2; exit 99; }
FAKE_ROOT="" exec "$@"'
# Every root command checks that it is root, and logs itself.
root='[[ ${FAKE_ROOT-} == 1 ]] || { echo "$(basename "$0"): not root" >&2; exit 98; }
echo "$(basename "$0") $*" >> "$FAKE_STATE/root"'
# losetup: devices are files in loops/ holding their backing file; one
# marked .autoclear goes with its last unmount. Listing them needs no root,
# as on Linux.
stub losetup '[[ $1 == --list || $1 == --noheadings ]] || { '"$root"'; }
loops=$FAKE_STATE/loops
mounted() { grep -q "^$1 " "$FAKE_STATE/mounts"; }
case $1 in
  --find)
    n=0
    while [[ -e $loops/loop$n ]]; do n=$((n + 1)); done
    echo "$4" > "$loops/loop$n"
    echo "/dev/fakeloop$n" ;;
  --detach)
    name=${2#/dev/fake}
    [[ -e $loops/$name ]] || { echo "losetup: $2: no such device" >&2; exit 1; }
    if mounted "$2"; then touch "$loops/$name.autoclear"; else rm -f "$loops/$name"; fi ;;
  --noheadings)
    for f in "$loops"/loop*; do
      [[ -e $f && $f != *.autoclear && $(cat "$f") == "$5" ]] && echo "/dev/fake$(basename "$f")"
    done; true ;;
  --list)
    for f in "$loops"/loop*; do
      [[ -e $f && $f != *.autoclear ]] && echo "/dev/fake$(basename "$f") $(cat "$f")"
    done; true ;;
  *) echo "losetup: unexpected $*" >&2; exit 97 ;;
esac'
stub mount "$root"'
[[ "$1 $2 $3 $4" == "-t ext4 -o nosuid,nodev" ]] || { echo "mount: unexpected $*" >&2; exit 97; }
[[ -z $FAKE_MOUNT_FAIL ]] || { echo "mount: wrong fs type" >&2; exit 32; }
[[ -z $FAKE_MOUNT_WRONG ]] || { echo "/dev/other $6" >> "$FAKE_STATE/mounts"; exit 0; }
echo "$5 $6" >> "$FAKE_STATE/mounts"
[[ -z $FAKE_AUTOMOUNT ]] || echo "$5 /run/media/someone/${5##*/}" >> "$FAKE_STATE/mounts"'
stub umount "$root"'
[[ $1 == --all-targets ]] || { echo "umount: unexpected $*" >&2; exit 97; }
dev=${!#}
grep -q "^$dev " "$FAKE_STATE/mounts" || { echo "umount: $dev: not mounted" >&2; exit 32; }
grep -v "^$dev " "$FAKE_STATE/mounts" > "$FAKE_STATE/mounts.new" || true
mv "$FAKE_STATE/mounts.new" "$FAKE_STATE/mounts"
name=${dev#/dev/fake}
if [[ -e $FAKE_STATE/loops/$name.autoclear ]]; then
  rm -f "$FAKE_STATE/loops/$name" "$FAKE_STATE/loops/$name.autoclear"
fi'
# findmnt: the forms the script uses, from the mounts file.
stub findmnt 'mounts=$FAKE_STATE/mounts
case "$*" in
  "-rn -o SOURCE,FSTYPE --mountpoint "*)
    awk -v t="$5" "\$2 == t { print \$1 \" ext4\"; found = 1 } END { exit !found }" "$mounts" ;;
  "-rn -o TARGET --source "*)
    awk -v s="$5" "\$1 == s { print \$2; found = 1 } END { exit !found }" "$mounts" ;;
  "--source "*)
    awk -v s="$2" "\$1 == s { print; found = 1 } END { exit !found }" "$mounts" ;;
  "-rn -o TARGET") awk "{ print \$2 }" "$mounts" ;;
  "-n -o FSTYPE --target "*) echo fakefs ;;
  *) echo "findmnt: unexpected $*" >&2; exit 97 ;;
esac'
stub mkfs.ext4 '[[ $1 == -V ]] && { echo "mkfs.ext4 0.0 (stand-in)"; exit 0; }
[[ ${FAKE_ROOT-} != 1 ]] || { echo "mkfs.ext4: run as root" >&2; exit 99; }
echo "mkfs.ext4 $*" >> "$FAKE_STATE/mkfs"'
# script -qec CMD FILE: runs CMD.
stub script 'exec bash -c "$2"'

# The stand-ins for `disk_full DIR` and `nota record ... --data DIR`: each
# fails unless it runs as the user, on a stand-in mount.
cat > "$target/debug/examples/disk_full" << 'EOF'
#!/usr/bin/env bash
[[ -z ${FAKE_ROOT-} ]] || { echo "ran as root"; exit 3; }
grep -q " $1\$" "$FAKE_STATE/mounts" || { echo "not on a mount: $1"; exit 3; }
case $FAKE_DISK in
  ok) echo "filled at session/journal-1: ok"; echo "ok: the ballast was freed" ;;
  fail) echo "filled at session/journal-1"; echo "disk_full: 3 journals left" >&2; exit 1 ;;
  hang) echo $$ > "$FAKE_STATE/hang.pid"; exec sleep 1000 ;;
esac
EOF
cat > "$target/debug/nota" << 'EOF'
#!/usr/bin/env bash
data=$7
[[ -z ${FAKE_ROOT-} ]] || { echo "ran as root"; exit 3; }
grep -q " $data\$" "$FAKE_STATE/mounts" || { echo "not on a mount: $data"; exit 3; }
[[ -e $data/ballast-1048576 ]] || { echo "no ballast"; exit 3; }
echo "nota: recorded to sessions/1 (2 segments)"
echo "  stopped early: the disk is full"
EOF
chmod +x "$target/debug/examples/disk_full" "$target/debug/nota"

export PATH=$bin:$PATH
run() { "$repo/scripts/disk-full.sh" --kernel-ext4 --images "$images" "$@"; }

# reset: no devices, mounts, logs or images from an earlier case.
reset() {
  rm -rf "$FAKE_STATE"/loops/* "$images"
  rm -f "$FAKE_STATE"/{root,sudo,mkfs,cargo,hang.pid}
  : > "$FAKE_STATE/mounts"
  export FAKE_DISK=ok FAKE_AUTOMOUNT='' FAKE_MOUNT_FAIL='' FAKE_MOUNT_WRONG=''
}

# check_left CASE: no device, mount or run directory left, and no stand-in
# disk_full running.
check_left() {
  [[ -z $(ls -A "$FAKE_STATE/loops") ]] || fail "$1: devices left: $(ls "$FAKE_STATE/loops")"
  [[ ! -s $FAKE_STATE/mounts ]] || fail "$1: mounts left: $(cat "$FAKE_STATE/mounts")"
  if compgen -G "$images/run.*" > /dev/null; then fail "$1: run directory left: $(ls "$images")"; fi
  if [[ -s $FAKE_STATE/hang.pid ]] && kill -0 "$(cat "$FAKE_STATE/hang.pid")" 2> /dev/null; then
    fail "$1: disk_full still running"
  fi
}

# expect CASE FILE REGEX: FILE has a line matching REGEX.
expect() {
  grep -qE -- "$3" "$2" || fail "$1: no line matching '$3' in: $(cat "$2")"
}

# 1. Bad options, and running it as root.
for bad in "--limit" "--limit 0" "--limit 5s" "22" "22m;x" "--ext4" "--bogus"; do
  reset
  # shellcheck disable=SC2086 # the words are the case
  if run $bad > "$dir/out" 2>&1; then code=0; else code=$?; fi
  [[ $code -eq 2 ]] || fail "options '$bad': expected status 2, got $code: $(cat "$dir/out")"
  [[ ! -e $FAKE_STATE/sudo ]] || fail "options '$bad': sudo ran"
done
reset
if FAKE_ROOT=1 run 22m > "$dir/out" 2>&1; then code=0; else code=$?; fi
[[ $code -eq 2 ]] || fail "as root: expected status 2, got $code"
expect "as root" "$dir/out" "not with sudo"

# 2. A dry run.
reset
run --dry-run 22m 48m > "$dir/out" 2>&1 || fail "dry run failed: $(cat "$dir/out")"
[[ ! -e $FAKE_STATE/sudo && ! -e $FAKE_STATE/root && ! -e $FAKE_STATE/mkfs && ! -e $images ]] ||
  fail "dry run ran something"
[[ $(grep -c '^\[root\]' "$dir/out") -eq 12 ]] || fail "dry run: expected 12 root commands: $(cat "$dir/out")"
if grep '^\[root\]' "$dir/out" | grep -vqE '^\[root\] (losetup|mount|umount) '; then
  fail "dry run: root runs more than losetup, mount and umount: $(grep '^\[root\]' "$dir/out")"
fi
expect "dry run" "$dir/out" '^\[you\] sudo -- '
expect "dry run" "$dir/out" '^\[you\] mkfs.ext4 .*root_owner='
expect "dry run" "$dir/out" '^\[you\] timeout --kill-after=10 1800 .*/disk_full '

# 3. A run that passes.
reset
result=$dir/result.txt
if run --result "$result" 22m 23m > "$dir/out" 2>&1; then code=0; else code=$?; fi
[[ $code -eq 0 ]] || fail "pass: status $code: $(cat "$dir/out")"
[[ $(wc -l < "$FAKE_STATE/sudo") -eq 1 ]] || fail "pass: sudo ran $(wc -l < "$FAKE_STATE/sudo") times"
for line in '^22m: filled at session/journal-1: ok \([0-9]+ s\)$' '^23m: ' \
  '^nota record: nota: recorded to sessions/1 \(2 segments\) \([0-9]+ s\)$' \
  '^cleanup: no loop device or mount left$' '^result: PASSED$' '^sizes: 22m 23m; limit 1800 s'; do
  expect pass "$result" "$line"
done
[[ $(grep -c 'root_owner='"$(id -u):$(id -g)" "$FAKE_STATE/mkfs") -eq 3 ]] || fail "pass: mkfs.ext4 not run with the user's root_owner: $(cat "$FAKE_STATE/mkfs")"
[[ $(grep -c '^mount ' "$FAKE_STATE/root") -eq 3 ]] || fail "pass: expected 3 mounts: $(cat "$FAKE_STATE/root")"
if grep -vqE '^(losetup|mount|umount) ' "$FAKE_STATE/root"; then
  fail "pass: root ran more than losetup, mount and umount: $(cat "$FAKE_STATE/root")"
fi
check_left pass
expect pass "$dir/out" "results in $result"

# 4. An automounter's second mount.
reset
export FAKE_AUTOMOUNT=1
run --result "$result" --no-app 22m > "$dir/out" 2>&1 || fail "automount: failed: $(cat "$dir/out")"
expect automount "$result" 'also mounted on /run/media/someone/fakeloop0, by something else'
expect automount "$result" '^result: PASSED$'
check_left automount

# 5. Timed out, failed, and a mount that fails.
reset
export FAKE_DISK=hang
if run --result "$result" --no-app --limit 1 22m 23m > "$dir/out" 2>&1; then code=0; else code=$?; fi
[[ $code -eq 1 ]] || fail "timeout: expected status 1, got $code: $(cat "$dir/out")"
expect timeout "$result" "^22m: TIMED OUT: the disk didn't fill in 1 s \([0-9]+ KiB used, [0-9]+ KiB free\)$"
expect timeout "$result" "^23m: TIMED OUT"
expect timeout "$result" '^result: FAILED$'
check_left timeout

reset
export FAKE_DISK=fail
if run --result "$result" --no-app 22m > "$dir/out" 2>&1; then code=0; else code=$?; fi
[[ $code -eq 1 ]] || fail "failed run: expected status 1, got $code"
expect "failed run" "$result" '^22m: FAILED \([0-9]+ s\)$'
expect "failed run" "$result" '^  disk_full: 3 journals left$'
check_left "failed run"

reset
export FAKE_MOUNT_FAIL=1
if run --result "$result" 22m > "$dir/out" 2>&1; then code=0; else code=$?; fi
[[ $code -eq 1 ]] || fail "mount fails: expected status 1, got $code"
expect "mount fails" "$result" '^22m: FAILED to make or mount the image$'
expect "mount fails" "$result" 'mount: wrong fs type'
expect "mount fails" "$result" '^nota record: FAILED$'
[[ ! -s $FAKE_STATE/hang.pid ]] || fail "mount fails: disk_full ran"
check_left "mount fails"

reset
export FAKE_MOUNT_WRONG=1
if run --result "$result" --no-app 22m > "$dir/out" 2>&1; then code=0; else code=$?; fi
[[ $code -eq 1 ]] || fail "wrong mount: expected status 1, got $code"
expect "wrong mount" "$result" '^22m: FAILED to make or mount the image$'
expect "wrong mount" "$result" 'has "/dev/other ext4" mounted, not /dev/fakeloop0'
if grep -q 'filled at' "$result"; then fail "wrong mount: disk_full ran"; fi
# The other filesystem isn't the script's to unmount.
grep -v '^/dev/other ' "$FAKE_STATE/mounts" > "$dir/mounts" || true
mv "$dir/mounts" "$FAKE_STATE/mounts"
check_left "wrong mount"

# 6. Signals, while disk_full runs. The script starts in a session of its
# own with SIGINT at its default (a background job's is ignored), as from a
# terminal.
for sig in INT TERM HUP; do
  reset
  export FAKE_DISK=hang
  python3 -I -c 'import os, signal, sys
signal.signal(signal.SIGINT, signal.SIG_DFL)
os.setsid()
os.execvp(sys.argv[1], sys.argv[1:])' "$repo/scripts/disk-full.sh" --kernel-ext4 --images "$images" \
    --result "$result" 22m 23m > "$dir/out" 2>&1 &
  pid=$!
  for _ in $(seq 100); do
    [[ -s $FAKE_STATE/hang.pid ]] && break
    sleep 0.1
  done
  if [[ ! -s $FAKE_STATE/hang.pid ]]; then
    fail "SIG$sig: disk_full never started: $(cat "$dir/out")"
    kill -9 "$pid" 2> /dev/null || true
    continue
  fi
  # Ctrl-C reaches the whole foreground group; the others, the script.
  if [[ $sig == INT ]]; then kill -INT -- "-$pid"; else kill "-$sig" "$pid"; fi
  for _ in $(seq 150); do
    kill -0 "$pid" 2> /dev/null || break
    sleep 0.1
  done
  if kill -0 "$pid" 2> /dev/null; then
    fail "SIG$sig: still running 15 s after the signal"
    kill -9 "$pid" 2> /dev/null || true
  fi
  if wait "$pid"; then code=0; else code=$?; fi
  case $sig in INT) want=130 ;; TERM) want=143 ;; HUP) want=129 ;; esac
  [[ $code -eq $want ]] || fail "SIG$sig: expected status $want, got $code: $(cat "$dir/out")"
  expect "SIG$sig" "$result" "^interrupted by SIG$sig"
  expect "SIG$sig" "$result" '^cleanup: no loop device or mount left$'
  expect "SIG$sig" "$result" '^result: INTERRUPTED$'
  [[ $(grep -c '^mount ' "$FAKE_STATE/root") -eq 1 ]] || fail "SIG$sig: went on to another run"
  check_left "SIG$sig"
done

# 7. --cleanup after a run that was killed: a device on an image under
# --images, detached but still mounted (as the script leaves it), and one
# never mounted; and one on another file, which stays.
reset
mkdir -p "$images/run.old/disk1" "$images/run.old/disk2"
images_real=$(cd "$images" && pwd -P)
echo "$images_real/run.old/disk1.img" > "$FAKE_STATE/loops/loop0"
touch "$FAKE_STATE/loops/loop0.autoclear"
echo "/dev/fakeloop0 $images_real/run.old/disk1" > "$FAKE_STATE/mounts"
echo "$images_real/run.old/disk2.img" > "$FAKE_STATE/loops/loop1"
echo "$dir/elsewhere.img" > "$FAKE_STATE/loops/loop2"
if run --cleanup --result "$result" > "$dir/out" 2>&1; then code=0; else code=$?; fi
[[ $code -eq 0 ]] || fail "cleanup: status $code: $(cat "$dir/out")"
expect cleanup "$result" '^cleanup: unmounted and detached /dev/fakeloop0 '
expect cleanup "$result" '^cleanup: unmounted and detached /dev/fakeloop1 '
expect cleanup "$result" '^cleanup: nothing left$'
[[ -e $FAKE_STATE/loops/loop2 ]] || fail "cleanup: detached a device on another file"
rm -f "$FAKE_STATE/loops/loop2"
check_left cleanup

exit "$status"
