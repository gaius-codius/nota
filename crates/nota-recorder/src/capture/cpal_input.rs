//! Capture through a cpal host that doesn't stamp its buffers: the
//! `PulseAudio` and ALSA hosts, which [`pulse`](super::pulse) and
//! [`alsa`](super::alsa) open streams on.
//!
//! The buffers are sent unstamped ([`CaptureSender::audio`]): cpal's
//! timestamps on these hosts count from the stream's own start, not from
//! `CLOCK_MONOTONIC`, so the recorder can't set them against the session
//! clock. No drift is measured, and a loss is timed at the overrun the
//! host reports, if it reports one (see the capture module's docs).
//!
//! A stream that's dropped can still deliver the buffer that was in
//! flight: on `PulseAudio` the server's acknowledgement of the deletion
//! comes after the drop. The recorder discards what arrives after the
//! stop, but [`Progress`](super::Progress) has counted it as delivered.

use std::time::Duration;

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{BufferSize, StreamConfig};
use nota_core::SampleRate;

use super::pipewire::{start_error, stream_error};
use super::{CaptureError, CaptureSender, Devices, Source, devices::Device};

/// How long opening a stream may wait for the audio server to answer.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// A running stream on one of the plain hosts. Capture stops when it's
/// dropped.
pub struct PlainStream {
    /// Held so the stream runs until this is dropped.
    _stream: cpal::Stream,
}

impl std::fmt::Debug for PlainStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PlainStream")
    }
}

/// Opens and starts a mono 16-bit stream at `rate` on `device`, sending
/// what it captures to `events`, with buffers of `buffer`.
///
/// `source` is what was asked for, which start errors name. `pinned`
/// names the device as the stream stays on it: these hosts don't follow
/// a default, so a device that goes away ends the stream.
///
/// # Errors
///
/// A [`CaptureError`] if the stream can't be built or started.
pub(super) fn open(
    device: &cpal::Device,
    (source, pinned): (&Source, &Source),
    rate: SampleRate,
    buffer: BufferSize,
    events: CaptureSender,
) -> Result<PlainStream, CaptureError> {
    let config = StreamConfig {
        channels: 1,
        sample_rate: rate.hz(),
        buffer_size: buffer,
    };
    let errors = events.clone();
    let failed_source = pinned.clone();
    let stream = device
        .build_input_stream::<i16, _, _>(
            config,
            // Only a copy into the channel, as on the `PipeWire` stream.
            move |samples: &[i16], _: &cpal::InputCallbackInfo| events.audio(samples),
            // No thread of ours to promote here, so rtkit's refusal (which
            // the `PipeWire` stream ignores after a direct promotion) is
            // always a warning.
            move |error: cpal::Error| match stream_error(&failed_source, &error, false) {
                Some(Ok(notice)) => errors.notice(notice),
                Some(Err(failure)) => errors.failed(failure),
                None => {}
            },
            Some(OPEN_TIMEOUT),
        )
        .map_err(|e| start_error(source, &e))?;
    stream.play().map_err(|e| start_error(source, &e))?;
    Ok(PlainStream { _stream: stream })
}

/// Whether a device of `direction` plays (a sink) rather than records.
pub(super) fn plays(direction: cpal::DeviceDirection) -> bool {
    direction == cpal::DeviceDirection::Output
}

/// One device as a host lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Listed {
    /// The host's name for it.
    pub(super) name: String,
    /// Its name as the user knows it.
    pub(super) description: String,
    /// Whether it plays (a sink) rather than records.
    pub(super) plays: bool,
}

/// One device a host lists, kept unless it's a monitor source, which Setup
/// shows as the sink it records.
pub(super) fn listed_unless_monitor(
    name: String,
    description: String,
    plays: bool,
) -> Option<Listed> {
    (plays || !name.ends_with(".monitor")).then_some(Listed {
        name,
        description,
        plays,
    })
}

/// The [`Devices`] a host's `listed` devices make, with its defaults.
/// Each list is by description, then name, as Setup shows them.
pub(super) fn devices_from(
    listed: impl IntoIterator<Item = Listed>,
    default_output: Option<String>,
    default_input: Option<String>,
) -> Devices {
    let mut devices = Devices {
        default_output,
        default_input,
        ..Devices::default()
    };
    for item in listed {
        let list = if item.plays {
            &mut devices.outputs
        } else {
            &mut devices.inputs
        };
        list.push(Device {
            name: item.name,
            description: item.description,
        });
    }
    for list in [&mut devices.outputs, &mut devices.inputs] {
        list.sort_by(|a, b| (&a.description, &a.name).cmp(&(&b.description, &b.name)));
    }
    devices
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(name: &str, description: &str, plays: bool) -> Listed {
        Listed {
            name: name.into(),
            description: description.into(),
            plays,
        }
    }

    /// Only an output direction plays; a duplex device records too.
    #[test]
    fn only_the_output_direction_plays() {
        use cpal::DeviceDirection::{Duplex, Input, Output, Unknown};
        assert!(plays(Output));
        for direction in [Input, Duplex, Unknown] {
            assert!(!plays(direction), "{direction:?}");
        }
    }

    /// A monitor source isn't an input of its own, but a sink is an output
    /// whatever its name.
    #[test]
    fn monitor_sources_are_not_listed_as_inputs() {
        let keep = |name: &str, plays| listed_unless_monitor(name.into(), "d".into(), plays);
        assert_eq!(keep("pci.monitor", false), None);
        assert_eq!(keep("usb", false), Some(listed("usb", "d", false)));
        assert_eq!(
            keep("pci.monitor", true),
            Some(listed("pci.monitor", "d", true))
        );
    }

    /// Sinks and sources are told apart and each list is by description,
    /// then name, with the defaults carried over.
    #[test]
    fn listed_devices_are_split_and_sorted() {
        let devices = devices_from(
            [
                listed("usb", "Headset", false),
                listed("hdmi", "Monitor", true),
                listed("builtin", "Headset", false),
                listed("analog", "Speakers", true),
            ],
            Some("analog".into()),
            Some("usb".into()),
        );
        let names = |list: &[Device]| list.iter().map(|d| d.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&devices.outputs), ["hdmi", "analog"]);
        assert_eq!(names(&devices.inputs), ["builtin", "usb"]);
        assert_eq!(devices.default_output.as_deref(), Some("analog"));
        assert_eq!(devices.default_input.as_deref(), Some("usb"));
    }
}
