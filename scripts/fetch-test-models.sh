#!/usr/bin/env bash
# Fills the test-models directory used by the engine tests: the Parakeet and
# Silero VAD models, and a synthetic speech fixture (invented-lecture.wav)
# spoken by a permissively licensed TTS voice. Tests skip when these are
# absent. Everything lands outside the repo; nothing it writes is committed.
#
# Requires: bash, curl, tar (bzip2), ffmpeg.
#
# Env:
#   NOTA_TEST_MODELS     target dir (default ~/.local/share/nota/test-models)
#   NOTA_MODEL_SOURCE    existing dir holding sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/
#                        and silero_vad_v6.onnx; symlinked instead of downloaded
#   SHERPA_ONNX_BIN_DIR  dir holding sherpa-onnx-offline-tts (default: downloaded
#                        from the sherpa-onnx v1.13.8 release)
set -euo pipefail

root=${NOTA_TEST_MODELS:-$HOME/.local/share/nota/test-models}
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
sentences=$repo/crates/nota-engine/tests/fixtures/invented-lecture.txt
dl=$root/dl
parakeet_name=sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8
sherpa_ver=v1.13.8
sherpa_archive=sherpa-onnx-$sherpa_ver-linux-x64-shared
gh=https://github.com/k2-fsa/sherpa-onnx/releases/download
# Voice: KittenTTS nano (Apache-2.0), speaker id below.
voice=kitten-nano-en-v0_1-fp16
sid=${NOTA_TTS_SID:-0}

mkdir -p "$root/fixtures" "$dl"

fetch() { # url dest
  [[ -e $2 ]] || { curl -fsSL -o "$2.part" "$1" && mv "$2.part" "$2"; }
}

# Parakeet TDT 0.6b v3 (int8)
if [[ ! -e $root/parakeet-tdt-0.6b-v3-int8 ]]; then
  if [[ -n ${NOTA_MODEL_SOURCE:-} ]]; then
    ln -s "$NOTA_MODEL_SOURCE/$parakeet_name" "$root/parakeet-tdt-0.6b-v3-int8"
  else
    fetch "$gh/asr-models/$parakeet_name.tar.bz2" "$dl/$parakeet_name.tar.bz2"
    tar -xjf "$dl/$parakeet_name.tar.bz2" -C "$dl"
    mkdir -p "$root/parakeet-tdt-0.6b-v3-int8.tmp"
    for f in encoder.int8.onnx decoder.int8.onnx joiner.int8.onnx tokens.txt; do
      cp "$dl/$parakeet_name/$f" "$root/parakeet-tdt-0.6b-v3-int8.tmp/$f"
    done
    mv "$root/parakeet-tdt-0.6b-v3-int8.tmp" "$root/parakeet-tdt-0.6b-v3-int8"
  fi
fi

# Silero VAD v6 (MIT), from the upstream v6.0 tag
if [[ ! -e $root/silero_vad_v6.onnx ]]; then
  if [[ -n ${NOTA_MODEL_SOURCE:-} ]]; then
    ln -s "$NOTA_MODEL_SOURCE/silero_vad_v6.onnx" "$root/silero_vad_v6.onnx"
  else
    fetch "https://raw.githubusercontent.com/snakers4/silero-vad/v6.0/src/silero_vad/data/silero_vad.onnx" \
      "$root/silero_vad_v6.onnx"
  fi
fi

# Speech fixture
wav=$root/fixtures/invented-lecture.wav
if [[ ! -e $wav ]]; then
  command -v ffmpeg >/dev/null || { echo "ffmpeg is required" >&2; exit 1; }

  bin_dir=${SHERPA_ONNX_BIN_DIR:-}
  if [[ -z $bin_dir ]]; then
    bin_dir=$dl/$sherpa_archive/bin
    if [[ ! -x $bin_dir/sherpa-onnx-offline-tts ]]; then
      fetch "$gh/$sherpa_ver/$sherpa_archive.tar.bz2" "$dl/$sherpa_archive.tar.bz2"
      tar -xjf "$dl/$sherpa_archive.tar.bz2" -C "$dl"
    fi
  fi
  export LD_LIBRARY_PATH="$bin_dir/../lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

  if [[ ! -e $dl/$voice/model.fp16.onnx ]]; then
    fetch "$gh/tts-models/$voice.tar.bz2" "$dl/$voice.tar.bz2"
    tar -xjf "$dl/$voice.tar.bz2" -C "$dl"
  fi

  work=$dl/synth
  mkdir -p "$work"
  n=0
  while IFS= read -r line; do
    [[ -n $line ]] || continue
    n=$((n + 1))
    "$bin_dir/sherpa-onnx-offline-tts" \
      --kitten-model="$dl/$voice/model.fp16.onnx" \
      --kitten-voices="$dl/$voice/voices.bin" \
      --kitten-tokens="$dl/$voice/tokens.txt" \
      --kitten-data-dir="$dl/$voice/espeak-ng-data" \
      --sid="$sid" \
      --output-filename="$work/s$n.wav" \
      "$line" >/dev/null 2>&1
  done <"$sentences"
  [[ $n -eq 4 ]] || { echo "expected 4 sentences, got $n" >&2; exit 1; }

  # 0.5 s lead, 0.7 s, 0.2 s, 0.7 s between sentences, 0.5 s trail.
  gaps=(0.5 0.7 0.2 0.7 0.5)
  inputs=() filter="" labels=""
  for i in 0 1 2 3 4; do
    inputs+=(-f lavfi -t "${gaps[$i]}" -i "anullsrc=r=16000:cl=mono")
  done
  for i in 1 2 3 4; do inputs+=(-i "$work/s$i.wav"); done
  # inputs 0-4 are silences, 5-8 sentences; interleave them.
  order=(0 5 1 6 2 7 3 8 4)
  for k in "${order[@]}"; do
    filter+="[$k:a]aresample=16000,aformat=sample_fmts=s16:channel_layouts=mono[a$k];"
    labels+="[a$k]"
  done
  filter+="${labels}concat=n=9:v=0:a=1[out]"
  ffmpeg -nostdin -loglevel error -y "${inputs[@]}" -filter_complex "$filter" \
    -map "[out]" -ar 16000 -ac 1 -c:a pcm_s16le "$wav.part.wav"
  mv "$wav.part.wav" "$wav"
fi

echo "test models ready in $root"
