#!/usr/bin/env bash
# T4 (synthesis test 4, an M2 exit criterion): device changes during a
# recording in `nota`. "Switching the default output, removing a pinned
# device, unplugging a USB mic, a Bluetooth profile change: each shown
# within 2 s and logged; never a silent wrong-device recording."
#
# For each case this starts `nota record` in a pseudo-terminal, changes a
# device, and times the change to the warning on the Recording screen (the
# screen is rebuilt from nota's output, each write stamped as it is read).
# Then it stops nota and reads the session: the timeline must hold the
# event, within 2 s of the change, and (where a tone was played into each
# device) the audio of every epoch must be the tone of the device the
# timeline names, so nothing was recorded silently from another one. The
# cases:
#   default-output   the system audio follows the default output, which is
#                    switched --switches times (default 4) between two
#                    sinks; each switch is timed and its epoch's opening is
#                    read for the old device's tone (GAI-434).
#   pinned-removed   the microphone track pinned to a device that is removed.
#   usb-mic-pinned   the microphone track pinned to a USB microphone, which
#                    is unplugged: Lost.
#   usb-mic-default  the microphone track follows the default input, a USB
#                    microphone, which is unplugged: Lost, or Changed to
#                    the input the server falls back to.
#   bluetooth        the system audio follows a default output whose
#                    profile changes (the node goes, another comes): Lost,
#                    or Changed to the new node.
#
# Two modes:
#   private  (the default) Runs on a private PipeWire, WirePlumber and
#            pipewire-pulse with no hardware, in a temporary runtime
#            directory of its own, as scripts/real-capture-first-epoch.sh
#            does: nothing of the user's is touched. The devices are null
#            sinks and virtual sources the script makes; a USB unplug and a
#            Bluetooth profile change are their removal (and, for
#            Bluetooth, a new node made the default). It shows that nota
#            reports each case, not how real hardware behaves.
#   real     Runs on the user's own PipeWire, with the user at the
#            terminal. The script changes no default, volume or device
#            except where it says so first and waits for Enter:
#              - default-output switches the default output (and puts it
#                back) only with --allow-default-switch and --other-sink;
#              - the USB microphone and the Bluetooth profile are changed
#                by the user, when asked; the change is timed from the
#                server's first event for it (`pactl subscribe`);
#              - pinned-removed uses a null sink the script makes and
#                removes (no hardware);
#              - a quiet tone plays into the two outputs of default-output
#                only with --tone.
#            The defaults (output and input) are read first and put back
#            on exit, whatever happens; the script says if it had to.
#            Everything it makes is named ndc_* and removed on exit; a
#            player or nota is stopped by its PID.
#
# What counts, per case (the report is also written to report.txt):
#   - the warning is on the screen within 2 s of the change;
#   - the timeline holds the matching event within 2 s of the change;
#   - a lost track has no audio after the loss;
#   - with tones: the epochs' audio is the devices' tones, in order;
#   - nota exits 0.
#
# Not run in CI: it needs PipeWire 1.x, WirePlumber 0.5 (private),
# pipewire-pulse, `pactl`, `pw-play`, `pw-link`, `ffmpeg`, python3 and a
# release build of nota (built here). The recordings go under ~/.cache, not
# /tmp, which is memory (each holds a 256 MiB ballast file).
#
# Usage: scripts/device-changes.sh [private|real] [--cases LIST]
#          [--switches N] [--other-sink NODE] [--usb-mic NODE]
#          [--bt-sink NODE] [--allow-default-switch] [--tone]
#          [--keep] [--out DIR]
#   --cases LIST    comma-separated cases (default: all that can run)
#   --switches N    switches in default-output (default 4; even ends where
#                   it started)
#   --other-sink    real: the sink to switch to (a sink name from
#                   `pactl list short sinks`); the current default is the
#                   other one
#   --usb-mic       real: the USB microphone's source name
#   --bt-sink       real: the Bluetooth device's sink name, which must
#                   already be the default output
#   --allow-default-switch   real: the user's OK to switch the default output
#   --tone          real: play a quiet tone into both outputs, so the audio
#                   after each switch can be told apart
#   --keep          keep the work directory (kept anyway if a case failed)
#   --out DIR       the work directory (default ~/.cache/nota/device-changes/<date>)
set -euo pipefail

mode=private
if [[ ${1:-} == private || ${1:-} == real ]]; then
  mode=$1
  shift
fi
keep=0
out=
pyargs=()
while [[ $# -gt 0 ]]; do
  case $1 in
    --keep) keep=1; shift ;;
    --out) out=${2:?}; shift 2 ;;
    --cases | --switches | --other-sink | --usb-mic | --bt-sink)
      pyargs+=("$1" "${2:?}"); shift 2 ;;
    --allow-default-switch | --tone) pyargs+=("$1"); shift ;;
    -h | --help) sed -n '2,/^set -/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

die() { echo "device-changes: $*" >&2; exit 2; }

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
for c in python3 pactl pw-play pw-link ffmpeg; do
  command -v "$c" >/dev/null || die "$c is required"
done
if [[ $mode == private ]]; then
  for c in pipewire wireplumber pipewire-pulse; do
    command -v "$c" >/dev/null || die "$c is required"
  done
fi

# The interpreter itself, found before the private instance's environment is
# set: a version manager's shim reads the user's config and state from the
# directories that environment moves.
python=$(python3 -I -c 'import sys; print(sys.executable)')
# Built before the private instance's environment is set, so the build sees
# the user's own.
cargo build --manifest-path "$repo/Cargo.toml" --release --locked --quiet -p nota
nota=${CARGO_TARGET_DIR:-$repo/target}/release/nota
[[ -x $nota ]] || die "built, but no $nota"

work=${out:-${XDG_CACHE_HOME:-$HOME/.cache}/nota/device-changes/$(date +%Y%m%d-%H%M%S)}
[[ ! -e $work ]] || die "$work exists"
mkdir -p "$work"
runtime=
pids=()
saved_sink=
saved_source=
finished=0

# Everything the script made on a real server, found by its name: a run
# killed past Python's own clean-up leaves no sink behind.
unload_fixtures() {
  local idx
  for idx in $(pactl list short modules 2>/dev/null |
    awk '($2 == "module-null-sink" && $0 ~ /sink_name=ndc_/) || ($2 == "module-remap-source" && $0 ~ /source_name=ndc_/) { print $1 }'); do
    pactl unload-module "$idx" || true
  done
}

cleanup() {
  if [[ $mode == real ]]; then
    unload_fixtures
    local sink source
    sink=$(pactl get-default-sink 2>/dev/null || true)
    source=$(pactl get-default-source 2>/dev/null || true)
    if [[ -n $saved_sink && $sink != "$saved_sink" ]]; then
      pactl set-default-sink "$saved_sink" || true
      echo "device-changes: the default output was $sink; put back to $saved_sink" >&2
    fi
    if [[ -n $saved_source && $source != "$saved_source" ]]; then
      pactl set-default-source "$saved_source" || true
      echo "device-changes: the default input was $source; put back to $saved_source" >&2
    fi
  fi
  # Written so an empty list is no error under `set -u` in bash 3.2.
  for pid in ${pids[@]+"${pids[@]}"}; do
    kill "$pid" 2>/dev/null || true
  done
  for pid in ${pids[@]+"${pids[@]}"}; do
    wait "$pid" 2>/dev/null || true
  done
  [[ -z $runtime ]] || rm -rf "$runtime"
  if [[ $keep -eq 1 || $finished -eq 0 ]]; then
    echo "Kept $work" >&2
  else
    rm -rf "$work"
  fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

if [[ $mode == private ]]; then
  # The no-hardware config below is WirePlumber 0.5's; 0.4 would ignore it
  # and open the user's real devices.
  wp_version=$(wireplumber --version | sed -n 's/.*libwireplumber \([0-9]*\)\.\([0-9]*\).*/\1 \2/p')
  read -r wp_major wp_minor <<<"${wp_version:-0 0}"
  if [ "$wp_major" -eq 0 ] && [ "$wp_minor" -lt 5 ]; then
    die "needs WirePlumber 0.5 or later"
  fi
  runtime=$(mktemp -d "${TMPDIR:-/tmp}/nota-devchg.XXXXXX")
  export XDG_RUNTIME_DIR=$runtime/run XDG_CONFIG_HOME=$runtime/config \
    XDG_STATE_HOME=$runtime/state XDG_DATA_HOME=$runtime/data
  # Nothing may point at the user's servers or session bus, nor start a bus
  # of its own that would outlive the script (D-Bus autolaunches one from a
  # display).
  unset PIPEWIRE_REMOTE PIPEWIRE_RUNTIME_DIR PULSE_SERVER PULSE_RUNTIME_PATH \
    DISPLAY WAYLAND_DISPLAY
  export DBUS_SESSION_BUS_ADDRESS=disabled:
  mkdir -p -m 700 "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d" \
    "$XDG_STATE_HOME" "$XDG_DATA_HOME"
  # No hardware: the instance sees only the null sinks and sources made.
  cat >"$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d/90-no-hardware.conf" <<'EOF'
wireplumber.profiles = {
  main = {
    monitor.alsa = disabled
    monitor.alsa-midi = disabled
    monitor.bluez = disabled
    monitor.bluez-midi = disabled
    monitor.v4l2 = disabled
    monitor.libcamera = disabled
  }
}
EOF
  pipewire >"$runtime/pipewire.log" 2>&1 &
  pids+=($!)
  for _ in $(seq 50); do
    [ -S "$XDG_RUNTIME_DIR/pipewire-0" ] && break
    sleep 0.1
  done
  [ -S "$XDG_RUNTIME_DIR/pipewire-0" ] || die "the private PipeWire didn't start"
  wireplumber >"$runtime/wireplumber.log" 2>&1 &
  pids+=($!)
  pipewire-pulse >"$runtime/pipewire-pulse.log" 2>&1 &
  pids+=($!)
  for _ in $(seq 50); do
    pactl info >/dev/null 2>&1 && break
    sleep 0.1
  done
  # pactl must be talking to this instance, which has no hardware.
  server=$(pactl info | sed -n 's/^Server String: //p')
  case $server in
    "$XDG_RUNTIME_DIR"/*) ;;
    *) die "pactl reached $server, not the private instance" ;;
  esac
  if pactl list short sinks | grep -v auto_null | grep -q . ||
    pactl list short sources | grep -v '\.monitor' | grep -q .; then
    die "the private instance has devices of its own: $(pactl list short sinks)"
  fi
else
  pactl info >/dev/null 2>&1 || die "no PipeWire (pipewire-pulse) to talk to"
  [[ -t 0 ]] || die "real mode needs a terminal: it asks the user to act"
  saved_sink=$(pactl get-default-sink)
  saved_source=$(pactl get-default-source)
  echo "device-changes: defaults saved: output $saved_sink, input $saved_source"
  # Left from an earlier run that was killed.
  unload_fixtures
fi

status=0
"$python" -I -B "$repo/scripts/device-changes.py" "$mode" "$work" "$nota" \
  ${pyargs[@]+"${pyargs[@]}"} || status=$?
# A run that failed keeps its directory, whatever --keep says.
[[ $status -ne 0 ]] || finished=1
exit $status
