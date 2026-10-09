#!/usr/bin/env bash
# Tests sherpa-onnx.sh without downloading anything:
#
#   1. It refuses a tampered archive and leaves nothing for a build to link.
#      The tampered archive is put where the script would have downloaded
#      it, next to libraries an earlier run unpacked and the sys crate's
#      build output from a build that bundled them (in a throwaway
#      CARGO_TARGET_DIR).
#   2. It downloads, checks and unpacks a good archive. A copy of the script
#      in a stand-in repo pins an archive made here instead of upstream's,
#      and a stub curl on PATH serves it.
#   3. A pin change gives the libraries a new directory, so every target
#      directory's build of the sys crate sees SHERPA_ONNX_LIB_DIR change,
#      and removes the old one.
#   4. It refuses to fill the default directory while .cargo/config.toml
#      names another.
#
# Requires: what sherpa-onnx.sh requires.
set -euo pipefail

scripts=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
script=$scripts/sherpa-onnx.sh
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

# 1. A tampered archive.
name=$("$script" --archive-name)
mkdir -p "$dir/sherpa-onnx-lib-0123456789abcdef" "$dir/other/lib"
touch "$dir/sherpa-onnx-lib-0123456789abcdef/libsherpa-onnx-c-api.a"
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
# And what scripts before the pinned directory name left: an unpinned
# directory and its checksum stamp.
mkdir "$dir/sherpa-onnx-lib"
touch "$dir/sherpa-onnx-lib/libsherpa-onnx-c-api.a"
echo 0000 >"$dir/sherpa-onnx-lib.sha256"

if out=$("$script" "$dir" 2>"$dir/err"); then
  fail "accepted a tampered archive (printed $out)"
fi
grep -q 'checksum mismatch' "$dir/err" || fail "expected a checksum mismatch, got: $(cat "$dir/err")"
[[ ! -e $dir/sherpa-onnx-lib-0123456789abcdef ]] || fail "left the earlier libraries for a build to link"
[[ ! -e $dir/sherpa-onnx-lib && ! -e $dir/sherpa-onnx-lib.sha256 ]] ||
  fail "left the unpinned libraries or their stamp from an older script"
[[ ! -e $dir/$name ]] || fail "kept the tampered archive"
left=$(find -H "$CARGO_TARGET_DIR" -name '*sherpa*onnx*sys*')
[[ -z $left ]] || fail "left sherpa-onnx-sys's build output, which bundles the earlier libraries: $left"
[[ -f $CARGO_TARGET_DIR/release/deps/libsherpa_onnx-0123456789abcdef.rlib ]] ||
  fail "removed another crate's build output"
[[ -d $dir/other/lib ]] || fail "removed a directory it doesn't own"

if "$script" --bogus "$dir" >/dev/null 2>&1; then
  fail "accepted an unknown option"
fi

# 2. A good archive, downloaded. The stand-in repo has the real Cargo.lock
# (for the script's version check) and a copy of the script whose pins are
# all the made archive's checksum.
repo=$dir/repo
mkdir -p "$repo/scripts" "$dir/bin" "$dir/served"
cp "$scripts/../Cargo.lock" "$repo/"
stem=${name%.tar.bz2}
# make_archive EXTRA: an archive in upstream's layout, with EXTRA in it so
# each EXTRA gives a different checksum.
make_archive() {
  rm -rf "$dir/build"
  mkdir -p "$dir/build/$stem/lib"
  touch "$dir/build/$stem/lib/libsherpa-onnx-c-api.a" "$dir/build/$stem/lib/sherpa-onnx-c-api.lib"
  echo "$1" >"$dir/build/$stem/lib/libsherpa-onnx-core.a"
  tar -cjf "$dir/served/$name" -C "$dir/build" "$stem"
}
# pin_script: the copy of the script, pinning the archive served now.
pin_script() {
  local sum
  sum=$(sha256sum "$dir/served/$name" 2>/dev/null || shasum -a 256 "$dir/served/$name")
  sed -E "s/^(sha256_[a-z0-9_]+=)[0-9a-f]{64}$/\1${sum%% *}/" "$script" >"$repo/scripts/sherpa-onnx.sh"
  chmod +x "$repo/scripts/sherpa-onnx.sh"
}
# The stub curl copies the served archive to its -o argument, and logs the
# URL it was asked for.
cat >"$dir/bin/curl" <<EOF
#!/usr/bin/env bash
out=
while [[ \$# -gt 0 ]]; do
  case \$1 in
    -o) out=\$2; shift ;;
    https://*) echo "\$1" >>"$dir/curl.log" ;;
  esac
  shift
done
cp "$dir/served/$name" "\$out"
EOF
chmod +x "$dir/bin/curl"
make_archive one
pin_script
dest=$dir/dest
if ! lib=$(PATH=$dir/bin:$PATH "$repo/scripts/sherpa-onnx.sh" "$dest" 2>"$dir/err"); then
  fail "refused the pinned archive: $(cat "$dir/err")"
fi
[[ $lib == "$dest"/sherpa-onnx-lib-* ]] || fail "printed $lib, not a pinned library directory in $dest"
[[ -f $lib/libsherpa-onnx-c-api.a ]] || fail "didn't unpack the libraries into $lib"
grep -qx "https://github.com/k2-fsa/sherpa-onnx/releases/download/v[0-9.]*/$name" "$dir/curl.log" ||
  fail "asked curl for $(cat "$dir/curl.log"), not upstream's $name"
[[ -f $dest/$name ]] || fail "didn't keep the archive"
[[ ! -e $dest/$name.part ]] || fail "left the partial download"
[[ ! -e $dest/sherpa-onnx-unpack ]] || fail "left the unpacking directory"
if [[ $(uname -s)-$(uname -m) == Linux-x86_64 ]]; then
  for stub in espeak-ng piper_phonemize ucd; do
    [[ $(cat "$lib/lib$stub.a") == '!<arch>' ]] || fail "no empty stand-in for lib$stub.a"
  done
fi

# Again: the kept archive is checked and reused, not downloaded.
rm "$dir/curl.log"
if ! again=$(PATH=$dir/bin:$PATH "$repo/scripts/sherpa-onnx.sh" "$dest" 2>"$dir/err"); then
  fail "refused its own kept archive: $(cat "$dir/err")"
fi
[[ $again == "$lib" ]] || fail "the same pins gave another directory: $again, not $lib"
[[ ! -e $dir/curl.log ]] || fail "downloaded the archive again"

# 3. A new pin: a new directory, and the old one gone.
make_archive two
pin_script
rm "$dest/$name"
if ! moved=$(PATH=$dir/bin:$PATH "$repo/scripts/sherpa-onnx.sh" "$dest" 2>"$dir/err"); then
  fail "refused the newly pinned archive: $(cat "$dir/err")"
fi
[[ $moved == "$dest"/sherpa-onnx-lib-* && $moved != "$lib" ]] ||
  fail "a new pin kept the directory $moved; builds wouldn't see the change"
[[ -f $moved/libsherpa-onnx-c-api.a ]] || fail "didn't unpack the new libraries into $moved"
[[ ! -e $lib ]] || fail "kept the old pin's libraries in $lib"

# 4. The default directory while .cargo/config.toml names another.
mkdir -p "$repo/.cargo"
echo 'SHERPA_ONNX_LIB_DIR = { value = "target/sherpa-onnx/sherpa-onnx-lib-0000000000000000", relative = true }' \
  >"$repo/.cargo/config.toml"
if PATH=$dir/bin:$PATH "$repo/scripts/sherpa-onnx.sh" >/dev/null 2>"$dir/err"; then
  fail "filled the default directory while .cargo/config.toml names another"
fi
grep -q "must set SHERPA_ONNX_LIB_DIR to \"target/sherpa-onnx/${moved##*/}\"" "$dir/err" ||
  fail "expected the directory .cargo/config.toml should name, got: $(cat "$dir/err")"
[[ ! -e $repo/target ]] || fail "wrote to the default directory before refusing"

exit "$status"
