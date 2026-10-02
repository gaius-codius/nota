#!/usr/bin/env bash
# Fails if a nota binary links GPL code from sherpa-onnx's text-to-speech:
# espeak-ng (any `espeak` symbol) or ucd-tools (`ucd_` symbols). Builds for
# distribution use the no-TTS libraries (sherpa-onnx-no-tts.sh); this proves
# it.
#
# The binary must still have its symbol table, or the check would pass on
# nothing: it also requires the sherpa-onnx recognizer's symbol. Run it
# before any strip step.
#
# The match is case-sensitive on purpose: `OfflineSpeakerDiarization`
# contains "eSpeak".
#
# Usage: check-no-gpl.sh BINARY
# Requires: nm (binutils or LLVM).
set -euo pipefail

bin=${1:?usage: check-no-gpl.sh BINARY}
# Symbol names only; Mach-O adds a leading underscore. Undefined symbols
# count for the GPL match (the code would ship in a library beside nota), but
# the recognizer must be defined here, so a dynamically linked binary fails.
listing=$(nm --format=bsd "$bin")
symbols=$(awk 'NF >= 2 { print $NF }' <<<"$listing")
defined=$(awk 'NF >= 3 { print $NF }' <<<"$listing")

if ! grep -qE '^_?SherpaOnnxCreateOfflineRecognizer$' <<<"$defined"; then
  echo "$bin: SherpaOnnxCreateOfflineRecognizer isn't defined in it; not the statically linked engine, or stripped" >&2
  exit 1
fi

gpl=$(grep -E 'espeak|^_?ucd_' <<<"$symbols" || true)
if [[ -n $gpl ]]; then
  echo "$bin links GPL code (espeak-ng or ucd-tools), $(wc -l <<<"$gpl") symbols, for example:" >&2
  head -n 5 <<<"$gpl" >&2
  exit 1
fi
echo "$bin: no espeak-ng or ucd-tools symbols"
