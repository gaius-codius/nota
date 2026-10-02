#!/usr/bin/env bash
# Fails if a nota binary links GPL code from sherpa-onnx's text-to-speech:
# espeak-ng (any `espeak` symbol) or ucd-tools (`ucd_` symbols). Release
# builds use the no-TTS libraries (sherpa-onnx-no-tts.sh); this proves it.
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
# Symbol names only; Mach-O adds a leading underscore.
symbols=$(nm "$bin" | awk 'NF >= 2 { print $NF }')

if ! grep -qE '^_?SherpaOnnxCreateOfflineRecognizer$' <<<"$symbols"; then
  echo "$bin: no SherpaOnnxCreateOfflineRecognizer symbol; not the engine binary, or stripped" >&2
  exit 1
fi

gpl=$(grep -E 'espeak|^_?ucd_' <<<"$symbols" || true)
if [[ -n $gpl ]]; then
  echo "$bin links GPL code (espeak-ng or ucd-tools), $(wc -l <<<"$gpl") symbols, for example:" >&2
  head -n 5 <<<"$gpl" >&2
  exit 1
fi
echo "$bin: no espeak-ng or ucd-tools symbols"
