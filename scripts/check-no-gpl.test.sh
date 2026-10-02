#!/usr/bin/env bash
# Tests check-no-gpl.sh on small objects built here, so CI shows it can fail
# without building nota twice.
#
# Requires: cc, nm, strip.
set -euo pipefail

check=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-no-gpl.sh
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT

# object NAME SYMBOL...: an object file defining each SYMBOL as a function.
object() {
  local name=$1
  shift
  printf 'void %s(void) {}\n' "$@" >"$dir/$name.c"
  cc -c -o "$dir/$name.o" "$dir/$name.c"
}

status=0
expect() { # pass|fail NAME
  if "$check" "$dir/$2.o" >/dev/null 2>&1; then got=pass; else got=fail; fi
  if [[ $got != "$1" ]]; then
    echo "check-no-gpl.sh on $2: expected $1, got $got" >&2
    status=1
  fi
}

object clean SherpaOnnxCreateOfflineRecognizer SherpaOnnxCreateOfflineSpeakerDiarization
object espeak SherpaOnnxCreateOfflineRecognizer espeak_Initialize
object espeak_ng SherpaOnnxCreateOfflineRecognizer espeak_ng_InitializePath
object ucd SherpaOnnxCreateOfflineRecognizer ucd_tolower
object no_engine main
object stripped SherpaOnnxCreateOfflineRecognizer
strip "$dir/stripped.o"

expect pass clean
expect fail espeak
expect fail espeak_ng
expect fail ucd
expect fail no_engine
expect fail stripped

exit "$status"
