//! Capture through cpal's `PulseAudio` host, for systems that run
//! `PulseAudio` and not `PipeWire`.
//!
//! The system audio is the default sink's monitor source, and the
//! microphone the default source. What the host can't do, against the
//! [`PipeWire`](super::PipeWireBackend) backend:
//! - **No default-device events.** The stream stays on the device that was
//!   the default when it started; if the default changes, it goes on
//!   recording the old one, and says so at the start as a
//!   [`CaptureNotice::Warning`].
//! - **No device loss reported.** The host gives a failure only when it
//!   can't read the stream's timing. A server that moves the stream to
//!   another source when its device is unplugged (as `PulseAudio` and
//!   `pipewire-pulse` do) isn't told of, so the track records on from the
//!   new source with no loss, epoch or warning.
//! - **No overruns.** The host doesn't report them, so a loss isn't
//!   noticed: the audio after it is timed early by what was lost, and no
//!   epoch shows the gap.
//! - **No stamps.** Buffers aren't stamped with the server's capture time,
//!   so drift isn't measured (the capture module's docs, "Lost audio and
//!   drift").
//! - **No watch on the server**, so no route-change epochs.
//!
//! The stream asks for [`BUFFER_FRAMES`] frames a buffer: left to the
//! server, a buffer is about two seconds, which would hold up the first
//! audio and trip the stalled detector.
//!
//! Once connected, the host waits for the server without a bound, except
//! while a stream is built (5 s): starting it (`play`, which waits for the
//! first audio), listing devices and looking up the defaults wait as long
//! as the server takes. A server that answers the handshake and then
//! stops answering can hold up a start, and Setup's preview.

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{BufferSize, DeviceId, HostId};
use nota_core::SampleRate;

use super::cpal_input::{self, PlainStream};
use super::{CaptureBackend, CaptureError, CaptureNotice, CaptureSender, Devices, Source};

/// The frames in each buffer the stream asks for: 800, which is 50 ms at
/// nota's 16 kHz. Measured against `pipewire-pulse`, the server's own
/// default delivers every 2 s and holds `play` back for as long.
const BUFFER_FRAMES: u32 = 800;

/// What `PulseAudio` calls the source that records what plays on `sink`.
fn monitor_of(sink: &str) -> String {
    format!("{sink}.monitor")
}

/// The host's name for the device that records `source`, and whether the
/// stream follows a default that can change (it can't: see the module
/// docs). `default_sink` and `default_source` are the server's defaults;
/// `sinks` are the names of its sinks, which a [`Source::Device`] may name.
fn source_name(
    source: &Source,
    default_sink: Option<&str>,
    default_source: Option<&str>,
    sinks: &[String],
) -> Option<(String, bool)> {
    match source {
        Source::SystemAudio => default_sink.map(|sink| (monitor_of(sink), true)),
        Source::Microphone => default_source.map(|name| (name.to_owned(), true)),
        Source::Device(name) if sinks.contains(name) => Some((monitor_of(name), false)),
        Source::Device(name) => Some((name.clone(), false)),
    }
}

/// Captures from `PulseAudio`, through cpal. The server converts whatever
/// the device runs at to mono 16-bit samples at the rate asked for.
///
/// See the module docs for what this host can't report.
#[derive(Debug, Default, Clone, Copy)]
pub struct PulseBackend;

/// The connection to the `PulseAudio` server.
fn host() -> Result<cpal::Host, CaptureError> {
    cpal::host_from_id(HostId::PulseAudio).map_err(|e| CaptureError::HostUnavailable(e.to_string()))
}

impl CaptureBackend for PulseBackend {
    type Stream = PlainStream;

    fn devices(&self) -> Result<Devices, CaptureError> {
        let host = host()?;
        let id_of = |device: &cpal::Device| device.id().ok().map(|id| id.id().to_owned());
        let listed = host
            .devices()
            .map_err(|e| CaptureError::Backend(e.to_string()))?
            .filter_map(|device| {
                let name = id_of(&device)?;
                let description = device.description().ok()?;
                let plays = cpal_input::plays(description.direction());
                cpal_input::listed_unless_monitor(name, description.name().to_owned(), plays)
            });
        Ok(cpal_input::devices_from(
            listed,
            host.default_output_device().as_ref().and_then(id_of),
            host.default_input_device().as_ref().and_then(id_of),
        ))
    }

    fn start(
        &self,
        source: &Source,
        rate: SampleRate,
        events: CaptureSender,
    ) -> Result<PlainStream, CaptureError> {
        let host = host()?;
        let id_of = |device: &cpal::Device| device.id().ok().map(|id| id.id().to_owned());
        let sinks: Vec<String> = host
            .devices()
            .map_err(|e| CaptureError::Backend(e.to_string()))?
            .filter(|d| (d.description()).is_ok_and(|d| cpal_input::plays(d.direction())))
            .filter_map(|d| id_of(&d))
            .collect();
        let (name, follows) = source_name(
            source,
            host.default_output_device()
                .as_ref()
                .and_then(id_of)
                .as_deref(),
            host.default_input_device()
                .as_ref()
                .and_then(id_of)
                .as_deref(),
            &sinks,
        )
        .ok_or_else(|| CaptureError::DeviceNotAvailable(source.clone()))?;
        let device = host
            .device_by_id(&DeviceId::new(HostId::PulseAudio, &name))
            .ok_or_else(|| CaptureError::DeviceNotAvailable(source.clone()))?;
        if follows {
            events.notice(CaptureNotice::Warning(format!(
                "PulseAudio: {source} records {name}, and doesn't follow the default if it changes"
            )));
        }
        let pinned = Source::Device(name);
        let buffer = BufferSize::Fixed(BUFFER_FRAMES);
        cpal_input::open(&device, (source, &pinned), rate, buffer, events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sinks() -> Vec<String> {
        vec!["alsa_output.pci".to_owned(), "alsa_output.usb".to_owned()]
    }

    /// The system audio is the default sink's monitor, and the
    /// microphone the default source; both are pinned to what the default
    /// was at the start.
    #[test]
    fn the_defaults_are_resolved_to_the_devices_they_name() {
        let resolve = |source| {
            source_name(
                &source,
                Some("alsa_output.pci"),
                Some("alsa_input.usb"),
                &sinks(),
            )
        };
        assert_eq!(
            resolve(Source::SystemAudio),
            Some(("alsa_output.pci.monitor".to_owned(), true))
        );
        assert_eq!(
            resolve(Source::Microphone),
            Some(("alsa_input.usb".to_owned(), true))
        );
    }

    /// A sink named as a device records what plays on it; any other name
    /// is the source itself.
    #[test]
    fn a_sink_named_as_a_device_is_captured_by_its_monitor() {
        let resolve =
            |name: &str| source_name(&Source::Device(name.to_owned()), None, None, &sinks());
        assert_eq!(
            resolve("alsa_output.usb"),
            Some(("alsa_output.usb.monitor".to_owned(), false))
        );
        assert_eq!(
            resolve("alsa_input.usb"),
            Some(("alsa_input.usb".to_owned(), false))
        );
    }

    /// With no default sink or source there's nothing to record.
    #[test]
    fn a_default_the_server_hasnt_got_resolves_to_nothing() {
        assert_eq!(
            source_name(&Source::SystemAudio, None, Some("m"), &[]),
            None
        );
        assert_eq!(source_name(&Source::Microphone, Some("s"), None, &[]), None);
    }

    /// A smoke check against whatever server the machine has: one that
    /// answers lists its devices, with no monitor among the inputs, and one
    /// that can't be reached says so. (CI has no server, where this only
    /// proves the error is `HostUnavailable`.)
    #[test]
    fn the_server_lists_its_devices_or_is_unavailable() {
        match PulseBackend.devices() {
            Ok(devices) => {
                assert!(
                    !devices.outputs.is_empty() || !devices.inputs.is_empty(),
                    "{devices:?}"
                );
                assert!(
                    devices.inputs.iter().all(|d| !d.name.ends_with(".monitor")),
                    "{devices:?}"
                );
            }
            Err(CaptureError::HostUnavailable(_)) => {}
            Err(other) => panic!("{other}"),
        }
    }
}
