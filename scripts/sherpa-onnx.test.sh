#!/usr/bin/env bash
# Tests that sherpa-onnx.sh refuses a tampered archive and leaves nothing
# for a build to link, without downloading anything: the tampered archive is
# put where the script would have downloaded it, next to libraries an
# earlier run unpacked and the sys crate's build output from a build that
# bundled them (in a throwaway CARGO_TARGET_DIR). No checksum stamp, so only
# the failed check can explain the clean.
#
# Requires: what sherpa-onnx.sh requires.
set -euo pipefail

script=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/sherpa-onnx.sh
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
# A symlink, as a target directory may be.
mkdir "$dir/real-target"
ln -s real-target "$dir/target"
export CARGO_TARGET_DIR=$dir/target

status=0
fail() {
  echo "sherpa-onnx.test.sh: $*" >&2
  status=1
}

name=$("$script" --archive-name)
mkdir -p "$dir/sherpa-onnx-lib" "$dir/other/lib"
touch "$dir/sherpa-onnx-lib/libsherpa-onnx-c-api.a"
# The sys crate's output in a debug, a release and a cross-target layout,
# next to another crate's.
for layout in debug release x86_64-unknown-linux-gnu/debug; do
  out=$CARGO_TARGET_DIR/$layout
  mkdir -p "$out/.fingerprint/sherpa-onnx-sys-0123456789abcdef" \
    "$out/build/sherpa-onnx-sys-0123456789abcdef" "$out/deps"
  touch "$out/deps/libsherpa_onnx_sys-0123456789abcdef.rlib" "$out/libsherpa_onnx_sys.rlib" \
    "$out/deps/libsherpa_onnx-0123456789abcdef.rlib"
done
echo "not the pinned archive" >"$dir/$name"

if out=$("$script" "$dir" 2>"$dir/err"); then
  fail "accepted a tampered archive (printed $out)"
fi
grep -q 'checksum mismatch' "$dir/err" || fail "expected a checksum mismatch, got: $(cat "$dir/err")"
[[ ! -e $dir/sherpa-onnx-lib ]] || fail "left the earlier libraries for a build to link"
[[ ! -e $dir/$name ]] || fail "kept the tampered archive"
left=$(find -H "$CARGO_TARGET_DIR" -name '*sherpa*onnx*sys*')
[[ -z $left ]] || fail "left sherpa-onnx-sys's build output, which bundles the earlier libraries: $left"
[[ -f $CARGO_TARGET_DIR/release/deps/libsherpa_onnx-0123456789abcdef.rlib ]] ||
  fail "removed another crate's build output"
[[ -d $dir/other/lib ]] || fail "removed a directory it doesn't own"

if "$script" --bogus "$dir" >/dev/null 2>&1; then
  fail "accepted an unknown option"
fi

exit "$status"
