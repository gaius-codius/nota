#!/usr/bin/env bash
# Prepares the sherpa-onnx libraries for a build for distribution: upstream's static
# archive built without text-to-speech, so no GPL code (espeak-ng,
# ucd-tools) is linked into nota. Prints the library directory; point
# SHERPA_ONNX_LIB_DIR at it:
#
#   SHERPA_ONNX_LIB_DIR=$(scripts/sherpa-onnx-no-tts.sh DIR) cargo build --release
#
# The sherpa-onnx-sys build script always links espeak-ng, piper_phonemize
# and ucd, so empty archives stand in for them. Nothing in the no-TTS
# libraries references them, so the linker has nothing to pull in.
#
# Ordinary builds, `cargo build --release` included, keep the crate's default
# download (with TTS); nothing may be distributed from them. A build for
# distribution uses this and checks the result with check-no-gpl.sh.
#
# Usage: sherpa-onnx-no-tts.sh DIR    download into DIR (reused if present)
# Requires: bash, curl, tar (bzip2), sha256sum.
set -euo pipefail

# Must match sherpa-onnx-sys in Cargo.lock; a version bump fails here until
# this pin (and the checksum) is updated with it.
version=1.13.8
declare -A sha256=(
  [linux-x64]=36f2ebd0b9aa09248ff6461a30dcb7edfe7eecf2deb67dfb0ceb3aaf4906051f
)

dest=${1:?usage: sherpa-onnx-no-tts.sh DIR}
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

locked=$(grep -A1 '^name = "sherpa-onnx-sys"$' "$repo/Cargo.lock" | sed -n 's/^version = "\(.*\)"$/\1/p') || true
if [[ $locked != "$version" ]]; then
  echo "Cargo.lock has sherpa-onnx-sys ${locked:-(none)}, this script pins $version; update both together" >&2
  exit 1
fi

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) platform=linux-x64 ;;
  *)
    # Upstream also publishes osx-* and win-x64 MT-Release no-TTS archives;
    # those need their own checksums and stubs (Windows names them *.lib).
    echo "no pinned no-TTS archive for $(uname -s)-$(uname -m)" >&2
    exit 1
    ;;
esac

# Absolute, since the sys crate's build script runs in its own directory.
mkdir -p "$dest"
dest=$(cd "$dest" && pwd)
name=sherpa-onnx-v$version-$platform-static-no-tts-lib
url=https://github.com/k2-fsa/sherpa-onnx/releases/download/v$version/$name.tar.bz2
archive=$dest/$name.tar.bz2
lib=$dest/$name/lib

if [[ ! -f $archive ]]; then
  curl -fsSL --retry 3 -o "$archive.part" "$url"
  mv "$archive.part" "$archive"
fi
if ! sha256sum --check --status <<<"${sha256[$platform]}  $archive"; then
  echo "checksum mismatch for $archive (expected ${sha256[$platform]}); removed it" >&2
  rm -f "$archive"
  exit 1
fi

rm -rf "${dest:?}/$name"
tar -xjf "$archive" -C "$dest"
if [[ ! -f $lib/libsherpa-onnx-c-api.a ]]; then
  echo "unexpected archive layout: no $lib/libsherpa-onnx-c-api.a" >&2
  exit 1
fi
for stub in espeak-ng piper_phonemize ucd; do
  if [[ -e $lib/lib$stub.a ]]; then
    echo "the no-TTS archive unexpectedly contains lib$stub.a" >&2
    exit 1
  fi
  # An archive with no members: just the global header.
  printf '!<arch>\n' >"$lib/lib$stub.a"
done

echo "$lib"
