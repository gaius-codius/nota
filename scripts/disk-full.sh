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
# journal: its last segments were finished in the ballast's room.
#
# With --ext4, the same runs are made on a small ext4 image mounted with
# fuse2fs (no root needed; the binary is $FUSE2FS, default `fuse2fs` on the
# PATH, from e2fsprogs 1.47 or later built with --enable-fuse2fs), where the
# tmpfs run can't show what ext4 does with its blocks. fuse2fs is ext4's
# on-disk format in user space: it checks that nota copes with the way ext4
# runs out of space and reuses blocks, not the kernel's journal commit and
# delayed allocation (a kernel mount needs root).
#
# Not run in CI: GitHub's Ubuntu runners refuse unprivileged user
# namespaces. Run it by hand after changing the write path, the ballast or
# the disk check.
#
# Usage: scripts/disk-full.sh [SIZE...]
#   SIZE  sizes to try, as mount takes them (default: 17m to 48m; 20m to
#         48m with --ext4)
#   --no-ballast  record with no ballast, and only report what happened
#   --checks      check the free space every 50 ms as well
#   --no-app      skip the run of `nota record`
#   --ext4        on ext4 (an image mounted with fuse2fs), not tmpfs

set -Eeuo pipefail

cd "$(dirname "$0")/.."
FLAGS=()
SIZES=()
APP=1
EXT4=0
for arg in "$@"; do
  case $arg in
    --no-ballast | --checks) FLAGS+=("$arg") ;;
    --no-app) APP=0 ;;
    --ext4) EXT4=1 ;;
    -h | --help) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) SIZES+=("$arg") ;;
  esac
done
if [[ ${#SIZES[@]} -eq 0 ]]; then
  # ext4's journal and tables leave less than tmpfs does: 17m has no room
  # for the ballast.
  for mb in $(seq $((EXT4 == 1 ? 20 : 17)) 48); do SIZES+=("${mb}m"); done
fi

if [[ $EXT4 -eq 1 ]]; then
  FUSE2FS=${FUSE2FS:-fuse2fs}
  command -v "$FUSE2FS" > /dev/null || { echo "disk-full: no fuse2fs (set FUSE2FS)" >&2; exit 2; }
  command -v fusermount3 > /dev/null || { echo "disk-full: no fusermount3" >&2; exit 2; }
  command -v mkfs.ext4 > /dev/null || { echo "disk-full: no mkfs.ext4" >&2; exit 2; }
fi

# ext4_run SIZE DIR CMD...: runs CMD with a fresh ext4 of SIZE mounted on DIR
# (fuse2fs forks into the background once it has mounted), then unmounts it
# and removes the image. It is called inside `$(...)`, where `set -e` is
# off, so every step is checked: a mount that failed must not run CMD on
# the host's own disk. The mount is in the system's mount table (not in a
# namespace of its own, as the tmpfs one is), so it's unmounted by its mount
# point on every way out, an interrupt included.
ext4_run() {
  local size=$1 dir=$2 image status=0
  shift 2
  image=$(mktemp) || return 1
  # shellcheck disable=SC2064 # $image and $dir are fixed from here on
  trap "fusermount3 -u '$dir' 2> /dev/null || fusermount3 -uz '$dir' 2> /dev/null; rm -f '$image'" RETURN
  truncate -s "$size" "$image" || return 1
  mkfs.ext4 -q -F "$image" || return 1
  "$FUSE2FS" "$image" "$dir" -o fakeroot || return 1
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
  # The screen's output goes to the log; the summary is its last lines.
  if [[ $EXT4 -eq 1 ]]; then
    # 8 MiB: ext4's journal and tables take part of what tmpfs gives whole.
    app_on_ext4() {
      script -qec "$target/debug/nota record --tone yes --title Full --data $1" /dev/null > "$log" 2>&1
      echo "journals left: $(find "$1" -name "journal-*" | wc -l)" >> "$log"
    }
    ext4_run 8m "$dir" app_on_ext4 "$dir" || echo "(the mount or nota failed: $?)" >> "$log"
  else
    unshare --user --map-root-user --mount sh -c '
      mount -t tmpfs -o size=4m tmpfs "$1" &&
      script -qec "$2 record --tone yes --title Full --data $1" /dev/null > "$3" 2>&1
      echo "journals left: $(find "$1" -name "journal-*" | wc -l)" >> "$3"' \
      sh "$dir" "$target/debug/nota" "$log" || echo "(the namespace or nota failed: $?)" >> "$log"
  fi
  summary=$(tr -d '\r' < "$log" | sed 's/\x1b\[[0-9;?]*[a-zA-Z]//g' | grep -A20 -e '^nota: ' -e '^(the namespace' -e '^(the mount' || true)
  if grep -q 'stopped early: the disk is full' <<<"$summary" &&
    grep -q '^journals left: 0$' <<<"$summary"; then
    echo "nota record: $(head -1 <<<"$summary" | sed "s|$dir/||")"
  else
    failed=1
    echo "nota record: FAILED"
    awk '{print "  " $0}' <<<"$summary"
  fi
fi
exit "$failed"
