#!/usr/bin/env bash
# Installs the native build dependencies of the Linux workspace on a Debian
# or Ubuntu CI runner: the recorder's capture (cpal's PipeWire and ALSA
# hosts, and rtkit over D-Bus) needs the PipeWire, ALSA and D-Bus headers,
# pkg-config, and libclang for bindgen. See the `capture` module's docs in
# nota-recorder.
set -euo pipefail

sudo apt-get update -qq
sudo apt-get install -y -qq --no-install-recommends \
  libpipewire-0.3-dev libspa-0.2-dev libasound2-dev libdbus-1-dev libclang-dev pkg-config
