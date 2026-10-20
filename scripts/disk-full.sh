#!/usr/bin/env bash
# A full disk during a recording, on a real filesystem: a small tmpfs,
# mounted in a user namespace of its own (no root needed), that the
# recorder fills.
#
# For each size, `disk_full` (crates/nota-recorder/examples/disk_full.rs)
# keeps an 8 MiB ballast on the tmpfs and records two tracks of noise onto
# it, as `nota record` does, until the disk is full; then it stops and
# checks that the ballast was freed, the open segments finished, and every
# sample it took is in a published segment, decoded and compared. Sizes
# differ in where the disk fills: a journal write, a FLAC publish or a
# SQLite commit, or with --checks a free-space check (under 1 MiB free).
# Each line says which.
#
# Then the app itself: `nota record --tone yes` on a 4 MiB tmpfs, under a
# pseudo-terminal (`script`), fills it in about half a minute. It must stop
# by itself, say in its summary that the disk is full, and leave no
# journal: its last segments were finished in the ballast's room. nota makes
# its ballast when a recording ends, so the first recording in a data
# directory has none: this run starts with the 1 MiB one (a tone
# recording's) that an earlier recording would have left.
#
# With --ext4, the same runs are made on a small ext4 image mounted with
# fuse2fs (no root needed; the binary is $FUSE2FS, default `fuse2fs` on the
# PATH, from e2fsprogs 1.47 or later built with --enable-fuse2fs), where the
# tmpfs run can't show what ext4 does with its blocks. fuse2fs is ext4's
# on-disk format in user space: it checks that nota copes with the way ext4
# runs out of space and reuses blocks, not the kernel's journal commit and
# delayed allocation.
#
# With --kernel-ext4, they're made on the kernel's own ext4, journal commits
# and delayed allocation included: each image (`mkfs.ext4` defaults) is
# attached to a loop device and mounted, which needs root. Run it as
# yourself, not with sudo: it builds as you, then asks for sudo once, and
# as root it runs only losetup, mount and umount. The rest, mkfs.ext4 and
# the runs included, runs as you (through setpriv), so ext4's reserved
# blocks are kept from the recorder as they are from nota. The images,
# mount points and logs go in a directory of their own under --images, on
# a disk rather than in memory, and it's removed at the end. Each loop
# device is told to detach itself once it's unmounted, and every mount of
# it (an automounter's too) is unmounted on every way out: the end, a
# failure, Ctrl-C, SIGTERM or SIGHUP. After a kill -9 or a power cut,
# `--kernel-ext4 --cleanup` removes what's left. A slow disk fills slowly,
# so each run has a time limit (--limit); a run that reaches it is reported
# as timed out, not passed. The app run is on an 8 MiB image, as with
# --ext4. The results go to the terminal and to --result, with how long
# each run took. --dry-run prints every command and who would run it (you
# or root), and runs nothing.
#
# Not run in CI: GitHub's Ubuntu runners refuse unprivileged user
# namespaces. Run it by hand after changing the write path, the ballast or
# the disk check.
#
# Usage: scripts/disk-full.sh [OPTION...] [SIZE...]
#   SIZE  sizes to try, as mount takes them (default: 17m to 48m; 20m to
#         48m with --ext4; 22m to 48m with --kernel-ext4)
#   --no-ballast   record with no ballast, and only report what happened
#   --checks       check the free space every 50 ms as well
#   --no-app       skip the run of `nota record`
#   --ext4         on ext4 (an image mounted with fuse2fs), not tmpfs
#   --kernel-ext4  on the kernel's ext4 (loop devices, so it asks for sudo)
#   With --kernel-ext4:
#   --limit SECONDS  each run's time limit (default 1800)
#   --images DIR     where the images go (default ~/.cache/nota/disk-full-kernel)
#   --result FILE    where the results go too (default: a new file in
#                    --images, named at the start and the end)
#   --dry-run        print the commands, and who would run each; run nothing
#   --cleanup        unmount and detach whatever an earlier run left

set -Eeuo pipefail

self=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")
start=$PWD
cd "$(dirname "$self")/.."
args=("$@")
FLAGS=()
SIZES=()
APP=1
EXT4=0
KERNEL=0
DRY=0
CLEANUP=0
LIMIT=1800
IMAGES=${XDG_CACHE_HOME:-$HOME/.cache}/nota/disk-full-kernel
RESULT=
KOPTS=0
# The root half of --kernel-ext4: `--as-root UID GID IMAGES RUN_DIR
# DISK_FULL NOTA RESULT --`, then the options the script was started with.
# Only the script passes it, through sudo, which resets HOME and the like:
# the paths come from the user half, absolute.
AS_ROOT=0
if [[ ${1-} == --as-root ]]; then
  [[ $# -ge 9 && $9 == -- ]] || { echo "disk-full: bad --as-root" >&2; exit 2; }
  AS_ROOT=1 RUN_UID=$2 RUN_GID=$3 IMAGES=$4 RUN_DIR=$5 BIN=$6 NOTA=$7 RESULT=$8
  shift 9
fi
usage() {
  echo "disk-full: $*; see --help" >&2
  exit 2
}
while [[ $# -gt 0 ]]; do
  case $1 in
    --no-ballast | --checks) FLAGS+=("$1") ;;
    --no-app) APP=0 ;;
    --ext4) EXT4=1 ;;
    --kernel-ext4) KERNEL=1 ;;
    --dry-run) DRY=1 ;;
    --cleanup) CLEANUP=1 ;;
    --limit | --images | --result)
      [[ $# -ge 2 ]] || usage "$1 needs a value"
      KOPTS=1
      # The root half has its paths from the user half.
      case $1 in
        --limit) LIMIT=$2 ;;
        --images) [[ $AS_ROOT -eq 1 ]] || IMAGES=$2 ;;
        --result) [[ $AS_ROOT -eq 1 ]] || RESULT=$2 ;;
      esac
      shift
      ;;
    -h | --help) sed -n '2,/^$/p' "$self" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) usage "unknown option $1" ;;
    *) SIZES+=("$1") ;;
  esac
  shift
done
if [[ $KERNEL -eq 1 ]]; then
  [[ $EXT4 -eq 0 ]] || usage "--ext4 and --kernel-ext4 can't be used together"
  [[ $LIMIT =~ ^[1-9][0-9]*$ ]] || usage "--limit takes whole seconds, not $LIMIT"
  # Sizes go to truncate and mkfs.ext4; nothing else is passed on.
  for size in "${SIZES[@]}"; do
    [[ $size =~ ^[1-9][0-9]*[kmg]$ ]] || usage "a size is a number and k, m or g, not $size"
  done
  [[ $IMAGES == /* ]] || IMAGES=$start/$IMAGES
  [[ -z $RESULT || $RESULT == /* ]] || RESULT=$start/$RESULT
else
  [[ $DRY$CLEANUP$KOPTS == 000 ]] ||
    usage "--limit, --images, --result, --dry-run and --cleanup are for --kernel-ext4"
fi
if [[ ${#SIZES[@]} -eq 0 ]]; then
  # ext4's journal and tables leave less than tmpfs does: 17m has no room
  # for the ballast. On the kernel's ext4, 20m has none and 21m runs out
  # while it finishes (GAI-473); pass 21m to try it.
  first=17
  [[ $EXT4 -eq 1 ]] && first=20
  [[ $KERNEL -eq 1 ]] && first=22
  for mb in $(seq "$first" 48); do SIZES+=("${mb}m"); done
fi

if [[ $EXT4 -eq 1 ]]; then
  FUSE2FS=${FUSE2FS:-fuse2fs}
  command -v "$FUSE2FS" > /dev/null || { echo "disk-full: no fuse2fs (set FUSE2FS)" >&2; exit 2; }
  command -v fusermount3 > /dev/null || { echo "disk-full: no fusermount3" >&2; exit 2; }
  command -v mkfs.ext4 > /dev/null || { echo "disk-full: no mkfs.ext4" >&2; exit 2; }
fi

# The app run's shell: makes the 1 MiB ballast in $1, records with nota ($2)
# until it stops, and counts the journals it left. Its output is the log.
# shellcheck disable=SC2016 # expanded by that shell
app_run='head -c 1048576 /dev/urandom > "$1/ballast-1048576" &&
  script -qec "$(printf "%q " "$2" record --tone yes --title Full --data "$1")" /dev/null
  echo "journals left: $(find "$1" -name "journal-*" | wc -l)"'

# app_summary LOG DIR: the app run's verdict from its log; fails if it
# didn't stop on a full disk with no journal left.
app_summary() {
  local summary
  # The screen's output goes to the log; the summary is its last lines.
  summary=$(tr -d '\r' < "$1" | sed 's/\x1b\[[0-9;?]*[a-zA-Z]//g' |
    grep -A20 -e '^nota: ' -e '^(the namespace' -e '^(the mount' || true)
  if grep -q 'stopped early: the disk is full' <<<"$summary" &&
    grep -q '^journals left: 0$' <<<"$summary"; then
    echo "nota record: $(head -1 <<<"$summary" | sed "s|$2/||")"
  else
    echo "nota record: FAILED"
    awk '{print "  " $0}' <<<"$summary"
    return 1
  fi
}

# --- The root half of --kernel-ext4 ------------------------------------------

# report LINE...: to the terminal and the result file.
report() {
  printf '%s\n' "$@" >&3
  [[ $DRY -eq 1 ]] || printf '%s\n' "$@" >> "$RESULT"
}

# as_user CMD...: runs CMD as the user who started the script, with a clean
# environment (HOME and the rest from their passwd entry). In a dry run, it
# prints the command instead.
as_user() {
  if [[ $DRY -eq 1 ]]; then
    printf '[you]'; printf ' %q' "$@"; echo
  else
    setpriv --reuid="$RUN_UID" --regid="$RUN_GID" --init-groups --reset-env -- "$@"
  fi
}

# as_root CMD...: runs CMD, one of losetup, mount and umount, as root. In a
# dry run, it prints the command instead.
as_root() {
  case $1 in
    losetup | mount | umount) ;;
    *) echo "disk-full: $1 isn't one of the commands run as root" >&2; return 1 ;;
  esac
  if [[ $DRY -eq 1 ]]; then
    printf '[root]'; printf ' %q' "$@"; echo
  else
    "$@"
  fi
}

# The run in progress: its image, the loop device it's on, the mount point
# and the process recording onto it. Empty between runs.
CUR_IMAGE=
CUR_LOOP=
CUR_MNT=
CUR_CHILD=

# kernel_mount SIZE NAME: makes a fresh ext4 of SIZE as the user, at
# $RUN_DIR/NAME.img, owned by them; attaches it to a loop device and mounts
# it on $RUN_DIR/NAME. Every step is checked: a mount that failed must not
# leave the run recording onto the host's own disk.
# shellcheck disable=SC2329 # run through logged
kernel_mount() {
  CUR_IMAGE=$RUN_DIR/$2.img
  CUR_MNT=$RUN_DIR/$2
  as_user truncate -s "$1" "$CUR_IMAGE" || return 1
  as_user mkfs.ext4 -q -F -E root_owner="$RUN_UID:$RUN_GID" "$CUR_IMAGE" || return 1
  as_user mkdir "$CUR_MNT" || return 1
  if [[ $DRY -eq 1 ]]; then
    as_root losetup --find --show --nooverlap "$CUR_IMAGE"
    CUR_LOOP=/dev/loopN
  else
    CUR_LOOP=$(as_root losetup --find --show --nooverlap "$CUR_IMAGE") || return 1
  fi
  as_root mount -t ext4 -o nosuid,nodev "$CUR_LOOP" "$CUR_MNT" || return 1
  # The kernel keeps a loop device that's in use and detaches it after its
  # last unmount, so from here an unmount is all it takes to free it, even
  # if this script is killed.
  as_root losetup --detach "$CUR_LOOP" || return 1
  [[ $DRY -eq 1 ]] && return 0
  local mounted
  mounted=$(findmnt -rn -o SOURCE,FSTYPE --mountpoint "$CUR_MNT") || true
  [[ $mounted == "$CUR_LOOP ext4" ]] || {
    echo "disk-full: $CUR_MNT has \"$mounted\" mounted, not $CUR_LOOP's ext4" >&2
    return 1
  }
}

# attached IMAGE: the loop devices IMAGE is attached to, one per line.
attached() {
  [[ -n $1 ]] || return 0
  losetup --noheadings --output NAME --associated "$1" 2> /dev/null || true
}

# kernel_unmount: stops the run in progress, unmounts every mount of its
# loop device, an automounter's too, waits for the device to detach, and
# removes the image and mount point. Safe to call twice, and at any point
# of kernel_mount. Fails, saying what's left, if a mount or device is.
kernel_unmount() {
  local loop target status=0
  [[ -z $CUR_CHILD ]] || stop_child
  [[ -n $CUR_IMAGE ]] || return 0
  if [[ $DRY -eq 1 ]]; then
    as_root umount --all-targets "$CUR_LOOP"
  else
    # Only the devices on this run's image: by now the device kernel_mount
    # set up may have gone, and its name gone to someone else's.
    for loop in $(attached "$CUR_IMAGE"); do
      # Detached now if it isn't mounted (the run stopped before the
      # mount), or with its last unmount if it is.
      as_root losetup --detach "$loop" || status=1
      while read -r target; do
        [[ -z $target || $target == "$CUR_MNT" ]] ||
          report "  ($loop was also mounted on $target, by something else; unmounting it)"
      done < <(findmnt -rn -o TARGET --source "$loop" || true)
      if findmnt --source "$loop" > /dev/null; then
        as_root umount --all-targets "$loop" || as_root umount --all-targets --lazy "$loop" || status=1
      fi
    done
    # The kernel detaches an unmounted device shortly after: wait for it.
    for _ in $(seq 50); do
      [[ -n $(attached "$CUR_IMAGE") ]] || break
      sleep 0.1
    done
    for loop in $(attached "$CUR_IMAGE"); do
      report "  LEFT: $loop, on $CUR_IMAGE$(findmnt --source "$loop" > /dev/null && echo ', mounted')"
      status=1
    done
  fi
  if [[ $status -eq 0 ]]; then
    as_user rm -f "$CUR_IMAGE"
    if [[ -d $CUR_MNT ]] || [[ $DRY -eq 1 ]]; then as_user rmdir "$CUR_MNT"; fi
  fi
  CUR_IMAGE='' CUR_LOOP='' CUR_MNT=''
  return "$status"
}

# stop_child: stops the run's process, timeout, which passes SIGTERM on to
# the process group it leads; then, after 10 s or once it has gone, kills
# what's left of that group.
stop_child() {
  local state
  kill -TERM "$CUR_CHILD" 2> /dev/null || true
  for _ in $(seq 100); do
    # Gone, or a zombie until it's waited for (which keeps its group's ID).
    state=$(sed 's/.*) //' "/proc/$CUR_CHILD/stat" 2> /dev/null | cut -d' ' -f1) || true
    [[ -n $state && $state != Z ]] || break
    sleep 0.1
  done
  kill -KILL -- "-$CUR_CHILD" 2> /dev/null || true
  wait "$CUR_CHILD" 2> /dev/null || true
  CUR_CHILD=
}

# limited LOG CMD...: runs CMD as the user, in the background so a signal
# reaches this script's traps at once, with LOG as its output, for at most
# the time limit. Returns its status: 124 if it ran out of time.
limited() {
  local log=$1 status=0
  shift
  if [[ $DRY -eq 1 ]]; then
    as_user timeout --kill-after=10 "$LIMIT" "$@"
    return 0
  fi
  setpriv --reuid="$RUN_UID" --regid="$RUN_GID" --init-groups --reset-env -- \
    timeout --kill-after=10 "$LIMIT" "$@" >> "$log" 2>&1 &
  CUR_CHILD=$!
  wait "$CUR_CHILD" || status=$?
  CUR_CHILD=
  return "$status"
}

# leftovers: the loop devices on files under --images, one per line, as
# NAME FILE.
leftovers() {
  local name file
  { losetup --list --noheadings --output NAME,BACK-FILE 2> /dev/null || true; } |
    while read -r name file; do
      [[ $file != "$IMAGES"/* ]] || echo "$name $file"
    done
}

# finish: ends the run in progress, checks that no loop device or mount of
# any run is left, and removes this run's directory. Runs once.
FINISHED=0
LEFT=0
finish() {
  [[ $FINISHED -eq 0 ]] || return 0
  FINISHED=1
  kernel_unmount || LEFT=1
  [[ $DRY -eq 0 ]] || { as_user rm -r --one-file-system "$RUN_DIR"; return 0; }
  local left
  left=$(leftovers)
  if [[ -n $left ]]; then
    LEFT=1
    report "cleanup: LEFT attached (scripts/disk-full.sh --kernel-ext4 --cleanup removes them):" \
      "  ${left//$'\n'/$'\n'  }"
  elif [[ $LEFT -eq 0 ]]; then
    report "cleanup: no loop device or mount left"
  fi
  if [[ $LEFT -eq 0 ]]; then
    as_user rm -r --one-file-system "$RUN_DIR" || report "cleanup: $RUN_DIR wasn't removed"
  else
    report "cleanup: kept $RUN_DIR"
  fi
}

# on_signal NAME STATUS: cleans up, says so, and exits with STATUS.
# shellcheck disable=SC2329 # run by the traps
on_signal() {
  trap '' INT TERM HUP
  report "interrupted by SIG$1: stopping the run and cleaning up"
  finish
  report "result: INTERRUPTED"
  exit "$2"
}

# cleanup_leftovers: unmounts and detaches every loop device on a file under
# --images, and removes the run directories nothing is mounted in.
cleanup_leftovers() {
  local name file dir status=0
  while read -r name file; do
    [[ -n $name ]] || continue
    as_root losetup --detach "$name" || status=1
    if [[ $DRY -eq 1 ]] || findmnt --source "$name" > /dev/null; then
      as_root umount --all-targets "$name" || as_root umount --all-targets --lazy "$name" || status=1
    fi
    report "cleanup: unmounted and detached $name ($file)"
  done < <(if [[ $DRY -eq 1 ]]; then echo "/dev/loopN $IMAGES/run.XXXXXX/disk1.img"; else leftovers; fi)
  [[ $DRY -eq 1 ]] || sleep 1
  for dir in "$IMAGES"/run.*; do
    [[ -d $dir ]] || continue
    if [[ $DRY -eq 0 ]] && findmnt -rn -o TARGET | grep -F "$dir/" > /dev/null; then
      report "cleanup: $dir still has a mount; kept"
      status=1
    else
      as_user rm -r --one-file-system "$dir" || status=1
    fi
  done
  if [[ $DRY -eq 0 && -n $(leftovers) ]]; then
    report "cleanup: still attached:" "$(leftovers)"
    status=1
  fi
  [[ $status -eq 1 ]] || report "cleanup: nothing left"
  return "$status"
}

# logged CMD...: runs CMD with its output added to $log (in a dry run, on
# the terminal, as it prints).
logged() {
  if [[ $DRY -eq 1 ]]; then "$@"; else "$@" >> "$log" 2>&1; fi
}

# kernel_root_half: the runs, as root. Checks what the user half passed.
kernel_root_half() {
  exec 3>&1
  if [[ $DRY -eq 0 ]]; then
    [[ $(id -u) -eq 0 ]] || { echo "disk-full: --as-root runs as root" >&2; exit 2; }
    [[ $RUN_UID =~ ^[1-9][0-9]*$ && $RUN_GID =~ ^[0-9]+$ ]] || {
      echo "disk-full: bad user $RUN_UID:$RUN_GID" >&2
      exit 2
    }
    local path
    for path in "$IMAGES" "$RESULT"; do
      [[ -e $path && ! -L $path && $(stat -c %u "$path") == "$RUN_UID" ]] || {
        echo "disk-full: $path isn't the user's" >&2
        exit 2
      }
    done
  fi
  if [[ $CLEANUP -eq 1 ]]; then
    cleanup_leftovers
    return
  fi
  if [[ $DRY -eq 0 ]]; then
    [[ $RUN_DIR == "$IMAGES"/run.* && -d $RUN_DIR && ! -L $RUN_DIR &&
      $(stat -c %u "$RUN_DIR") == "$RUN_UID" ]] || {
      echo "disk-full: $RUN_DIR isn't a run directory of the user's" >&2
      exit 2
    }
  fi
  trap 'on_signal INT 130' INT
  trap 'on_signal TERM 143' TERM
  trap 'on_signal HUP 129' HUP
  trap finish EXIT

  local size n=0 log status started out fill failed=0
  for size in "${SIZES[@]}"; do
    n=$((n + 1))
    log=$RUN_DIR/disk$n.log
    [[ $DRY -eq 1 ]] || as_user touch "$log"
    started=$SECONDS
    status=0
    if logged kernel_mount "$size" "disk$n"; then
      limited "$log" "$BIN" "$CUR_MNT" "${FLAGS[@]}" || status=$?
    else
      status=-1
    fi
    out=$(if [[ $DRY -eq 0 ]]; then sed "s|$CUR_MNT/||g" "$log"; fi)
    # How far a run that ran out of time got.
    fill=
    if [[ $status -eq 124 || $status -eq 137 ]]; then
      fill=$(as_user df -k --output=used,avail "$CUR_MNT" | awk 'NR == 2 { print " (" $1 " KiB used, " $2 " KiB free)" }') || true
    fi
    logged kernel_unmount || { failed=1; LEFT=1; }
    [[ $DRY -eq 0 ]] || continue
    case $status in
      0) report "$size: $(head -1 <<<"$out") ($((SECONDS - started)) s)" ;;
      124 | 137)
        failed=1
        report "$size: TIMED OUT: the disk didn't fill in ${LIMIT} s$fill"
        ;;
      *)
        failed=1
        if [[ $status -eq -1 ]]; then
          report "$size: FAILED to make or mount the image"
        else
          report "$size: FAILED ($((SECONDS - started)) s)"
        fi
        report "  ${out//$'\n'/$'\n'  }"
        ;;
    esac
    [[ $LEFT -eq 0 ]] || break
  done

  if [[ $APP -eq 1 && $LEFT -eq 0 ]]; then
    log=$RUN_DIR/app.log
    [[ $DRY -eq 1 ]] || as_user touch "$log"
    started=$SECONDS
    status=0
    # 8 MiB: ext4's journal and tables take part of what tmpfs gives whole.
    if logged kernel_mount 8m app; then
      limited "$log" bash -c "$app_run" bash "$CUR_MNT" "$NOTA" || status=$?
    else
      status=-1
    fi
    [[ $status -eq 0 || $DRY -eq 1 ]] || echo "(the mount or nota failed: $status)" >> "$log"
    local mnt=$CUR_MNT
    logged kernel_unmount || { failed=1; LEFT=1; }
    if [[ $DRY -eq 0 ]]; then
      if [[ $status -eq 124 || $status -eq 137 ]]; then
        failed=1
        report "nota record: TIMED OUT: it didn't stop in ${LIMIT} s"
      elif out=$(app_summary "$log" "$mnt"); then
        report "$out ($((SECONDS - started)) s)"
      else
        failed=1
        report "$out"
      fi
    fi
  fi
  finish
  trap - EXIT
  [[ $DRY -eq 0 ]] || return 0
  if [[ $failed -eq 0 && $LEFT -eq 0 ]]; then
    report "result: PASSED"
  else
    report "result: FAILED"
  fi
  echo "disk-full: results in $RESULT"
  [[ $failed -eq 0 && $LEFT -eq 0 ]]
}

# kernel_user_half: builds as the user, makes the run directory and the
# result file, and hands over to the root half through sudo (in a dry run,
# runs it here, printing).
kernel_user_half() {
  [[ $(id -u) -ne 0 ]] || {
    echo "disk-full: run --kernel-ext4 as yourself, not with sudo: it builds as you, then asks for sudo for the loop devices and mounts" >&2
    exit 2
  }
  local tool tools=(setpriv losetup mount umount findmnt mkfs.ext4 truncate timeout script stat)
  [[ $DRY -eq 1 ]] || tools+=(sudo)
  for tool in "${tools[@]}"; do
    command -v "$tool" > /dev/null || { echo "disk-full: no $tool" >&2; exit 2; }
  done
  local uid gid target run_dir
  uid=$(id -u)
  gid=$(id -g)
  if [[ $CLEANUP -eq 1 ]]; then
    run_dir=- BIN=- NOTA=-
  else
    if [[ $DRY -eq 1 ]]; then
      echo "[you] cargo build --locked --quiet -p nota-recorder --example disk_full"
      [[ $APP -eq 0 ]] || echo "[you] cargo build --locked --quiet -p nota --features fake-capture"
    else
      cargo build --locked --quiet -p nota-recorder --example disk_full
      [[ $APP -eq 0 ]] || cargo build --locked --quiet -p nota --features fake-capture
    fi
    target=$(cargo metadata --format-version 1 --no-deps |
      python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
    BIN=$target/debug/examples/disk_full
    NOTA=$target/debug/nota
  fi
  if [[ $DRY -eq 1 ]]; then
    RESULT=${RESULT:-$IMAGES/result-DATE.txt}
    [[ $CLEANUP -eq 1 ]] || run_dir=$IMAGES/run.XXXXXX
    echo "[you] mkdir -p $(printf %q "$IMAGES")"
    [[ $CLEANUP -eq 1 ]] || echo "[you] mktemp -d $(printf %q "$IMAGES/run.XXXXXX")"
    echo "[you] sudo -- $(printf '%q ' "$BASH" "$self" --as-root "$uid" "$gid" "$IMAGES" "$run_dir" "$BIN" "$NOTA" "$RESULT" -- "${args[@]}")"
    echo "# Then, as root, through sudo (the commands for [you] run as you, through setpriv):"
    RUN_UID=$uid RUN_GID=$gid RUN_DIR=$run_dir
    kernel_root_half
    return
  fi
  mkdir -p "$IMAGES"
  IMAGES=$(cd "$IMAGES" && pwd -P)
  [[ $CLEANUP -eq 1 ]] || run_dir=$(mktemp -d "$IMAGES/run.XXXXXX")
  RESULT=${RESULT:-$IMAGES/result-$(date +%Y%m%d-%H%M%S).txt}
  : > "$RESULT"
  {
    echo "disk-full.sh $(printf '%q ' "${args[@]}")"
    echo "started $(date -Iseconds) on $(uname -r), load $(cut -d' ' -f1-3 /proc/loadavg), at $(git rev-parse --short HEAD 2> /dev/null || echo '?')"
    echo "images in $IMAGES ($(findmnt -n -o FSTYPE --target "$IMAGES")), $(mkfs.ext4 -V 2>&1 | head -1)"
    [[ $CLEANUP -eq 1 ]] || echo "sizes: ${SIZES[*]}; limit ${LIMIT} s per run; flags: ${FLAGS[*]:-none}; app run: $([[ $APP -eq 1 ]] && echo yes || echo no)"
  } | tee "$RESULT"
  echo "disk-full: results in $RESULT; sudo is for losetup, mount and umount"
  exec sudo -- "$BASH" "$self" --as-root "$uid" "$gid" "$IMAGES" "$run_dir" "$BIN" "$NOTA" "$RESULT" -- "${args[@]}"
}

if [[ $KERNEL -eq 1 ]]; then
  if [[ $AS_ROOT -eq 1 ]]; then
    kernel_root_half
  else
    kernel_user_half
  fi
  exit
fi

# --- tmpfs and fuse2fs ------------------------------------------------------

# ext4_run SIZE DIR CMD...: runs CMD with a fresh ext4 of SIZE mounted on DIR
# (fuse2fs forks into the background once it has mounted), then unmounts it
# and removes the image. It is called inside `$(...)`, where `set -e` is
# off, so every step is checked: a mount that failed must not run CMD on
# the host's own disk. The mount is in the system's mount table (not in a
# namespace of its own, as the tmpfs one is), so it's unmounted by its mount
# point on every way out, an interrupt included. fuse2fs's own messages (a
# warning that it doesn't use the journal, from 1.47.4) go to a log of their
# own, shown only if the mount fails, so CMD's output starts with CMD's.
ext4_run() {
  local size=$1 dir=$2 image status=0
  shift 2
  image=$(mktemp) || return 1
  # shellcheck disable=SC2064 # $image and $dir are fixed from here on
  trap "fusermount3 -u '$dir' 2> /dev/null || fusermount3 -uz '$dir' 2> /dev/null; rm -f '$image' '$image.log'" RETURN
  truncate -s "$size" "$image" || return 1
  mkfs.ext4 -q -F "$image" || return 1
  "$FUSE2FS" "$image" "$dir" -o fakeroot > "$image.log" 2>&1 || { cat "$image.log" >&2; return 1; }
  mountpoint -q "$dir" || { echo "disk-full: $dir isn't a mount point" >&2; return 1; }
  "$@" || status=$?
  return "$status"
}

cargo build --locked --quiet -p nota-recorder --example disk_full
target=$(cargo metadata --format-version 1 --no-deps |
  python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
bin=$target/debug/examples/disk_full

failed=0
for size in "${SIZES[@]}"; do
  dir=$(mktemp -d)
  # A tmpfs mount lives only in its namespace, and goes with it; an ext4
  # one is unmounted by ext4_run.
  if [[ $EXT4 -eq 1 ]]; then
    run=(ext4_run "$size" "$dir" "$bin" "$dir" "${FLAGS[@]}")
  else
    # shellcheck disable=SC2016 # expanded by that shell
    run=(unshare --user --map-root-user --mount
      sh -c 'size=$1 dir=$2 bin=$3; shift 3; mount -t tmpfs -o size="$size" tmpfs "$dir" && exec "$bin" "$dir" "$@"'
      sh "$size" "$dir" "$bin" "${FLAGS[@]}")
  fi
  if out=$("${run[@]}" 2>&1); then
    echo "$size: $(head -1 <<<"$out" | sed "s|$dir/||g")"
  else
    failed=1
    echo "$size: FAILED"
    sed "s|^|  |; s|$dir/||g" <<<"$out"
  fi
  rmdir "$dir"
done

if [[ $APP -eq 1 ]]; then
  cargo build --locked --quiet -p nota --features fake-capture
  dir=$(mktemp -d)
  log=$(mktemp)
  trap 'rmdir "$dir" 2>/dev/null; rm -f "$log"' EXIT
  if [[ $EXT4 -eq 1 ]]; then
    # 8 MiB: ext4's journal and tables take part of what tmpfs gives whole.
    ext4_run 8m "$dir" bash -c "$app_run" bash "$dir" "$target/debug/nota" > "$log" 2>&1 ||
      echo "(the mount or nota failed: $?)" >> "$log"
  else
    # shellcheck disable=SC2016 # expanded by that shell
    unshare --user --map-root-user --mount sh -c 'mount -t tmpfs -o size=4m tmpfs "$1" && shift && exec bash -c "$@"' \
      sh "$dir" "$app_run" bash "$dir" "$target/debug/nota" > "$log" 2>&1 ||
      echo "(the namespace or nota failed: $?)" >> "$log"
  fi
  app_summary "$log" "$dir" || failed=1
fi
exit "$failed"
