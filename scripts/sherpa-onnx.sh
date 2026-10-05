#!/usr/bin/env bash
# Prepares the sherpa-onnx static libraries every build links, from an
# archive pinned by SHA-256, and prints the library directory.
#
# The sherpa-onnx-sys build script would otherwise download its archive with
# no checksum, and trust any copy it already unpacked. So every build points
# SHERPA_ONNX_LIB_DIR at the directory this script fills:
#   - .cargo/config.toml points it at target/sherpa-onnx/sherpa-onnx-lib,
#     under this script's default directory, so a build fails until this
#     script has run (and nota-engine's build script refuses a build with
#     the variable unset);
#   - an explicit SHERPA_ONNX_LIB_DIR in the environment wins (the release
#     build sets it to the no-TTS libraries).
#
#   scripts/sherpa-onnx.sh           # then cargo build, test, clippy...
#
# Each run checks the archive again, unpacks it afresh and replaces
# DIR/sherpa-onnx-lib. The old one is removed first, so after a failed check
# no library is left for a build to use. Cargo bundles the libraries into
# sherpa-onnx-sys's rlib and reruns its build script only when the variable
# changes, so the script also cleans that crate's build output when the
# check fails or the pinned archive differs from the one last unpacked
# (recorded in DIR/sherpa-onnx-lib.sha256).
#
# On Linux x86-64 the pinned archive is upstream's build without
# text-to-speech, with empty stand-ins for the three text-to-speech libraries
# the sys crate always links: no GPL code (espeak-ng, ucd-tools) is linked,
# and every build matches the release build. Elsewhere it is the crate's
# default archive, which links GPL text-to-speech code: those builds must not
# be distributed (no-TTS archives for them are planned).
#
# Usage: sherpa-onnx.sh [--no-tts] [--archive-name] [DIR]
#   DIR             download and unpack here (default: target/sherpa-onnx in
#                   the repo); the archive is kept and reused if it matches
#   --no-tts        fail unless this platform's pinned archive is the no-TTS
#                   one (builds for distribution)
#   --archive-name  print this platform's archive file name and exit
# Requires: bash, curl, tar (bzip2), sha256sum or shasum.
set -euo pipefail

# Must match sherpa-onnx-sys in Cargo.lock; a version bump fails here until
# this pin (and the checksums) is updated with it.
version=1.13.8
die() {
  echo "sherpa-onnx.sh: $*" >&2
  exit 1
}

require_no_tts=0
print_name=0
dest=
while [[ $# -gt 0 ]]; do
  case $1 in
    --no-tts) require_no_tts=1 ;;
    --archive-name) print_name=1 ;;
    -*) die "unknown option: $1" ;;
    *)
      [[ -z $dest ]] || die "more than one DIR given"
      dest=$1
      ;;
  esac
  shift
done

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

# Per platform: the archive's suffix, its SHA-256, and whether it's the
# no-TTS build. A case, not an associative array: macOS ships bash 3.2.
no_tts=0
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)
    platform=linux-x64
    suffix=linux-x64-static-no-tts-lib
    sha256=36f2ebd0b9aa09248ff6461a30dcb7edfe7eecf2deb67dfb0ceb3aaf4906051f
    no_tts=1
    ;;
  Darwin-arm64)
    platform=osx-arm64
    suffix=osx-arm64-static-lib
    sha256=9091bf160dc7fdacedbc906b212badf53c2993f4e5277a0e03998e96c31d60da
    ;;
  Darwin-x86_64)
    platform=osx-x64
    suffix=osx-x64-static-lib
    sha256=a3f88da3e54c850a12d61431e73f8affcd1f13738b75b768847dd79541835b4b
    ;;
  MINGW*-x86_64 | MSYS*-x86_64)
    platform=win-x64
    suffix=win-x64-static-MT-Release-lib
    sha256=56ffcf3c454c1f14f7bc9887286cc8143e7e542dc632804e1c447d5f8d534eaf
    ;;
  *) die "no pinned sherpa-onnx archive for $(uname -s)-$(uname -m)" ;;
esac
if [[ $require_no_tts -eq 1 && $no_tts -eq 0 ]]; then
  die "no pinned no-TTS archive for $platform"
fi
name=sherpa-onnx-v$version-$suffix
if [[ $print_name -eq 1 ]]; then
  echo "$name.tar.bz2"
  exit 0
fi

locked=$(grep -A1 '^name = "sherpa-onnx-sys"$' "$repo/Cargo.lock" | sed -n 's/^version = "\(.*\)"$/\1/p') || true
if [[ $locked != "$version" ]]; then
  die "Cargo.lock has sherpa-onnx-sys ${locked:-(none)}, this script pins $version; update both together"
fi

sha256_of() {
  if command -v sha256sum >/dev/null; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# Absolute, since the sys crate's build script runs in its own directory.
dest=${dest:-$repo/target/sherpa-onnx}
mkdir -p "$dest"
dest=$(cd "$dest" && pwd)
url=https://github.com/k2-fsa/sherpa-onnx/releases/download/v$version/$name.tar.bz2
archive=$dest/$name.tar.bz2
lib=$dest/sherpa-onnx-lib
unpack=$dest/sherpa-onnx-unpack
stamp=$dest/sherpa-onnx-lib.sha256

# Drops the libraries an earlier run bundled into sherpa-onnx-sys's rlib:
# its build output in every profile and target of the target directory
# (`cargo clean -p` only cleans one profile).
clean_sys() {
  local target_dir
  # Cargo's own answer, so build.target-dir and CARGO_TARGET_DIR count. From
  # the caller's directory, as their builds resolve a relative one.
  target_dir=$(cargo metadata --manifest-path "$repo/Cargo.toml" --format-version 1 --no-deps --offline |
    sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | sed 's/\\\\/\\/g') ||
    die "cargo metadata failed"
  [[ -n $target_dir ]] || die "cargo metadata gave no target directory"
  if command -v cygpath >/dev/null; then
    target_dir=$(cygpath -u "$target_dir")
  fi
  if [[ -d $target_dir ]]; then
    # -H: the target directory itself may be a symlink.
    find -H "$target_dir" \( -path '*/.fingerprint/sherpa-onnx-sys-*' -o -path '*/build/sherpa-onnx-sys-*' \
      -o -name 'libsherpa_onnx_sys*' \) -prune -exec rm -rf {} +
  fi
  rm -f "$stamp"
}

# Nothing is left to link until the archive has passed its check.
rm -rf "$lib" "$unpack"
if [[ -f $stamp && $(cat "$stamp") != "$sha256" ]]; then
  clean_sys
fi

if [[ ! -f $archive ]]; then
  curl -fsSL --proto =https --retry 3 --retry-all-errors --connect-timeout 30 -o "$archive.part" "$url"
  mv "$archive.part" "$archive"
fi
got=$(sha256_of "$archive")
if [[ $got != "$sha256" ]]; then
  rm -f "$archive"
  clean_sys
  die "checksum mismatch for $archive: expected $sha256, got $got; removed it"
fi

mkdir "$unpack"
tar -xjf "$archive" -C "$unpack"
[[ -d $unpack/$name/lib ]] || die "unexpected archive layout: no $name/lib"
if [[ $platform == win-x64 ]]; then
  first=sherpa-onnx-c-api.lib
else
  first=libsherpa-onnx-c-api.a
fi
[[ -f $unpack/$name/lib/$first ]] || die "unexpected archive layout: no $name/lib/$first"
if [[ $no_tts -eq 1 ]]; then
  for stub in espeak-ng piper_phonemize ucd; do
    if [[ -e $unpack/$name/lib/lib$stub.a ]]; then
      die "the no-TTS archive unexpectedly contains lib$stub.a"
    fi
    # An archive with no members: just the global header.
    printf '!<arch>\n' >"$unpack/$name/lib/lib$stub.a"
  done
fi
mv "$unpack/$name/lib" "$lib"
rm -rf "$unpack"
echo "$sha256" >"$stamp"

# Windows tools want a Windows path (forward slashes work).
if command -v cygpath >/dev/null; then
  cygpath -m "$lib"
else
  echo "$lib"
fi
