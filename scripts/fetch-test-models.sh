#!/usr/bin/env bash
# Fills the test-models directory used by the engine tests: the Parakeet and
# Silero VAD models, and a synthetic speech fixture (invented-lecture.wav)
# spoken by a permissively licensed TTS voice. Tests skip when these are
# absent, or fail when NOTA_REQUIRE_TEST_MODELS=1. Everything lands outside
# the repo; nothing it writes is committed.
#
# Every download is checked against the SHA-256 pinned below before it's
# used, and the model files are checked again on every run, including when
# they're already there or linked from NOTA_MODEL_SOURCE. A mismatch fails.
# The fixture is generated from pinned inputs; it isn't pinned itself.
#
# Requires: bash, curl, tar (bzip2), sha256sum, ffmpeg.
#
# Env:
#   NOTA_TEST_MODELS     target dir (default ~/.local/share/nota/test-models)
#   NOTA_MODEL_SOURCE    existing dir holding sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/
#                        and silero_vad_v6.onnx; symlinked instead of downloaded
#   SHERPA_ONNX_BIN_DIR  dir holding sherpa-onnx-offline-tts (default: downloaded
#                        from the sherpa-onnx v1.13.8 release)
set -euo pipefail

# SHA-256 of each download, and of each model file the tests load.
parakeet_archive_sha=5793d0fd397c5778d2cf2126994d58e9d56b1be7c04d13c7a15bb1b4eafb16bf
sherpa_archive_sha=c0bdb7907d3a74bba1d55d22bf4d9fa75586cf1530614ebe88a27b9118e015c4
voice_archive_sha=f35dac93754fe2ac97c66e1f468311d0d2130f7f0f5a89bfa1197e09a0cbdec5
declare -A model_sha=(
  [parakeet-tdt-0.6b-v3-int8/encoder.int8.onnx]=acfc2b4456377e15d04f0243af540b7fe7c992f8d898d751cf134c3a55fd2247
  [parakeet-tdt-0.6b-v3-int8/decoder.int8.onnx]=179e50c43d1a9de79c8a24149a2f9bac6eb5981823f2a2ed88d655b24248db4e
  [parakeet-tdt-0.6b-v3-int8/joiner.int8.onnx]=3164c13fc2821009440d20fcb5fdc78bff28b4db2f8d0f0b329101719c0948b3
  [parakeet-tdt-0.6b-v3-int8/tokens.txt]=d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d
  [silero_vad_v6.onnx]=1a153a22f4509e292a94e67d6f9b85e8deb25b4988682b7e174c65279d8788e3
)
# Silero VAD v6.2.3, by commit: a tag can move.
silero_commit=5cd7945676eb32225748052e2e6a0580e4686a08

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

verify() { # file sha256
  local got
  got=$(sha256sum <"$1") && got=${got%% *}
  [[ $got == "$2" ]] || {
    echo "checksum mismatch: $1 is ${got:-unreadable}, expected $2" >&2
    echo "(a file from an older or interrupted run? delete it and run again)" >&2
    return 1
  }
}

fetch() { # url dest sha256
  if [[ ! -e $2 ]]; then
    curl -fsSL --proto '=https' --retry 3 --retry-all-errors --connect-timeout 30 \
      -o "$2.part" "$1"
    verify "$2.part" "$3" || { rm -f "$2.part"; exit 1; }
    mv "$2.part" "$2"
  fi
  verify "$2" "$3" || exit 1
}

# Parakeet TDT 0.6b v3 (int8)
if [[ ! -e $root/parakeet-tdt-0.6b-v3-int8 ]]; then
  if [[ -n ${NOTA_MODEL_SOURCE:-} ]]; then
    ln -s "$NOTA_MODEL_SOURCE/$parakeet_name" "$root/parakeet-tdt-0.6b-v3-int8"
  else
    fetch "$gh/asr-models/$parakeet_name.tar.bz2" "$dl/$parakeet_name.tar.bz2" \
      "$parakeet_archive_sha"
    tar -xjf "$dl/$parakeet_name.tar.bz2" -C "$dl"
    mkdir -p "$root/parakeet-tdt-0.6b-v3-int8.tmp"
    for f in encoder.int8.onnx decoder.int8.onnx joiner.int8.onnx tokens.txt; do
      cp "$dl/$parakeet_name/$f" "$root/parakeet-tdt-0.6b-v3-int8.tmp/$f"
    done
    mv "$root/parakeet-tdt-0.6b-v3-int8.tmp" "$root/parakeet-tdt-0.6b-v3-int8"
  fi
fi

# Silero VAD v6 (MIT), from upstream at the commit above
if [[ ! -e $root/silero_vad_v6.onnx ]]; then
  if [[ -n ${NOTA_MODEL_SOURCE:-} ]]; then
    ln -s "$NOTA_MODEL_SOURCE/silero_vad_v6.onnx" "$root/silero_vad_v6.onnx"
  else
    fetch "https://raw.githubusercontent.com/snakers4/silero-vad/$silero_commit/src/silero_vad/data/silero_vad.onnx" \
      "$root/silero_vad_v6.onnx" "${model_sha[silero_vad_v6.onnx]}"
  fi
fi

# The model files, however they got here.
for f in "${!model_sha[@]}"; do
  verify "$root/$f" "${model_sha[$f]}" || exit 1
done

# Speech fixture
wav=$root/fixtures/invented-lecture.wav
if [[ ! -e $wav ]]; then
  command -v ffmpeg >/dev/null || { echo "ffmpeg is required" >&2; exit 1; }

  bin_dir=${SHERPA_ONNX_BIN_DIR:-}
  if [[ -z $bin_dir ]]; then
    # Extracted afresh from the checked archive, so the binary that runs
    # is the pinned one.
    bin_dir=$dl/$sherpa_archive/bin
    fetch "$gh/$sherpa_ver/$sherpa_archive.tar.bz2" "$dl/$sherpa_archive.tar.bz2" \
      "$sherpa_archive_sha"
    rm -rf "${dl:?}/$sherpa_archive"
    tar -xjf "$dl/$sherpa_archive.tar.bz2" -C "$dl"
  fi
  export LD_LIBRARY_PATH="$bin_dir/../lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

  fetch "$gh/tts-models/$voice.tar.bz2" "$dl/$voice.tar.bz2" "$voice_archive_sha"
  rm -rf "${dl:?}/$voice"
  tar -xjf "$dl/$voice.tar.bz2" -C "$dl"

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
