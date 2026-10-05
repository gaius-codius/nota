#!/usr/bin/env bash
# Tests that sherpa-onnx.sh refuses a tampered archive and leaves nothing
# for a build to link, without downloading anything: the tampered archive is
# put where the script would have downloaded it, next to libraries a
# previous run unpacked.
#
# Requires: what sherpa-onnx.sh requires.
set -euo pipefail

script=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/sherpa-onnx.sh
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT

status=0
fail() {
  echo "sherpa-onnx.test.sh: $*" >&2
  status=1
}

name=$("$script" --archive-name)
mkdir -p "$dir/lib"
touch "$dir/lib/libsherpa-onnx-c-api.a"
echo "not the pinned archive" >"$dir/$name"

if out=$("$script" "$dir" 2>"$dir/err"); then
  fail "accepted a tampered archive (printed $out)"
fi
grep -q 'checksum mismatch' "$dir/err" || fail "expected a checksum mismatch, got: $(cat "$dir/err")"
[[ ! -e $dir/lib ]] || fail "left the earlier lib directory for a build to link"
[[ ! -e $dir/$name ]] || fail "kept the tampered archive"

if "$script" --bogus "$dir" >/dev/null 2>&1; then
  fail "accepted an unknown option"
fi

exit "$status"
