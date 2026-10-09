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
# Not run in CI: GitHub's Ubuntu runners refuse unprivileged user
# namespaces. Run it by hand after changing the write path, the ballast or
# the disk check.
#
# Usage: scripts/disk-full.sh [SIZE...]
#   SIZE  tmpfs sizes to try, as mount takes them (default: 17m to 48m)
#   --no-ballast  record with no ballast, and only report what happened
#   --checks      check the free space every 50 ms as well
#   --no-app      skip the run of `nota record`

set -Eeuo pipefail

cd "$(dirname "$0")/.."
FLAGS=()
SIZES=()
APP=1
for arg in "$@"; do
  case $arg in
    --no-ballast | --checks) FLAGS+=("$arg") ;;
    --no-app) APP=0 ;;
    -h | --help) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) SIZES+=("$arg") ;;
  esac
done
if [[ ${#SIZES[@]} -eq 0 ]]; then
  for mb in $(seq 17 48); do SIZES+=("${mb}m"); done
fi

cargo build --locked --quiet -p nota-recorder --example disk_full
target=$(cargo metadata --format-version 1 --no-deps |
  python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
bin=$target/debug/examples/disk_full

failed=0
for size in "${SIZES[@]}"; do
  dir=$(mktemp -d)
  # The mount lives only in the namespace, and goes with it.
  if out=$(unshare --user --map-root-user --mount \
    sh -c 'size=$1 dir=$2 bin=$3; shift 3; mount -t tmpfs -o size="$size" tmpfs "$dir" && exec "$bin" "$dir" "$@"' \
    sh "$size" "$dir" "$bin" "${FLAGS[@]}" 2>&1); then
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
  # The screen's output goes to the log; the summary is its last lines.
  unshare --user --map-root-user --mount sh -c '
    mount -t tmpfs -o size=4m tmpfs "$1" &&
    script -qec "$2 record --tone yes --title Full --data $1" /dev/null > "$3" 2>&1
    echo "journals left: $(find "$1" -name "journal-*" | wc -l)" >> "$3"' \
    sh "$dir" "$target/debug/nota" "$log" || true
  summary=$(tr -d '\r' < "$log" | sed 's/\x1b\[[0-9;?]*[a-zA-Z]//g' | grep -A20 '^nota: ')
  rmdir "$dir"
  rm -f "$log"
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
