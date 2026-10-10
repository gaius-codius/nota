//! Capture through a cpal host that doesn't stamp its buffers: the
//! `PulseAudio` and ALSA hosts, which [`pulse`](super::pulse) and
//! [`alsa`](super::alsa) open streams on.
//!
//! The buffers are sent unstamped ([`CaptureSender::audio`]): cpal's
//! timestamps on these hosts count from the stream's own start, not from
//! `CLOCK_MONOTONIC`, so the recorder can't set them against the session
//! clock. Losses are therefore timed at the overrun, and no drift is
//! measured (see the capture module's docs).

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{BufferSize, StreamConfig};
use nota_core::SampleRate;

use super::pipewire::{start_error, stream_error};
use super::{CaptureError, CaptureSender, Devices, Source, devices::Device};

/// A running stream on one of the plain hosts. Capture stops when it's
/// dropped.
pub struct PlainStream {
    _stream: cpal::Stream,
}

impl std::fmt::Debug for PlainStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PlainStream")
    }
}

/// Opens and starts a mono 16-bit stream at `rate` on `device`, sending
/// what it captures to `events`.
///
/// `pinned` names the device as the stream stays on it: these hosts
/// don't follow a default, so a device that goes away ends the stream.
///
/// # Errors
///
/// A [`CaptureError`] if the stream can't be built or started.
pub(super) fn open(
    device: &cpal::Device,
    pinned: &Source,
    rate: SampleRate,
    events: CaptureSender,
) -> Result<PlainStream, CaptureError> {
    let config = StreamConfig {
        channels: 1,
        sample_rate: rate.hz(),
        buffer_size: BufferSize::Default,
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
            None,
        )
        .map_err(|e| start_error(pinned, &e))?;
    stream.play().map_err(|e| start_error(pinned, &e))?;
    Ok(PlainStream { _stream: stream })
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
