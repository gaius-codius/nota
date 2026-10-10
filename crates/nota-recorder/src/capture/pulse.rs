//! Capture through cpal's `PulseAudio` host, for systems that run
//! `PulseAudio` and not `PipeWire`.
//!
//! The system audio is the default sink's monitor source, and the
//! microphone the default source. What the host can't do, against the
//! [`PipeWire`](super::PipeWireBackend) backend:
//! - **No default-device events.** The stream stays on the device that was
//!   the default when it started; if the default changes, it goes on
//!   recording the old one, and says so at the start as a
//!   [`CaptureNotice::Warning`]. Only the device going away is reported,
//!   as the stream's failure
//!   ([`CaptureSender::failed`]), which also marks it lost.
//! - **No stamps.** Buffers aren't stamped with the server's capture time,
//!   so losses are timed at the overrun and drift isn't measured (the
//!   capture module's docs, "Lost audio and drift").
//! - **No watch on the server**, so no route-change epochs: the detectors
//!   and the device events see what the stream itself reports.

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{DeviceDirection, DeviceId, HostId};
use nota_core::SampleRate;

use super::cpal_input::{self, Listed, PlainStream};
use super::{CaptureBackend, CaptureError, CaptureNotice, CaptureSender, Devices, Source};

/// What `PulseAudio` calls the source that records what plays on `sink`.
fn monitor_of(sink: &str) -> String {
    format!("{sink}.monitor")
}

/// Whether `name` is a monitor source, which Setup lists as its sink.
fn is_monitor(name: &str) -> bool {
    name.ends_with(".monitor")
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

fn host() -> Result<cpal::Host, CaptureError> {
    cpal::host_from_id(HostId::PulseAudio).map_err(|e| CaptureError::HostUnavailable(e.to_string()))
}

/// The id string of `device`, if it has one.
fn id_of(device: &cpal::Device) -> Option<String> {
    device.id().ok().map(|id| id.id().to_owned())
}

impl CaptureBackend for PulseBackend {
    type Stream = PlainStream;

    fn devices(&self) -> Result<Devices, CaptureError> {
        let host = host()?;
        let listed = host
            .devices()
            .map_err(|e| CaptureError::Backend(e.to_string()))?
            .filter_map(|device| {
                let name = id_of(&device)?;
                let description = device.description().ok()?;
                let plays = description.direction() == DeviceDirection::Output;
                (plays || !is_monitor(&name)).then(|| Listed {
                    name,
                    description: description.name().to_owned(),
                    plays,
                })
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
        let sinks: Vec<String> = host
            .devices()
            .map_err(|e| CaptureError::Backend(e.to_string()))?
            .filter(|d| {
                d.description()
                    .is_ok_and(|d| d.direction() == DeviceDirection::Output)
            })
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
                "PulseAudio: {source} stays on {name} if the default changes"
            )));
        }
        cpal_input::open(&device, &Source::Device(name), rate, events)
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

    /// A server that answers lists its devices, with no monitor among the
    /// inputs, and one that can't be reached says so: never an empty list
    /// for either. (CI has no server; `pipewire-pulse` answers as
    /// `PulseAudio` does.)
    #[test]
    fn the_server_lists_its_devices_or_is_unavailable() {
        match PulseBackend.devices() {
            Ok(devices) => {
                assert!(
                    !devices.outputs.is_empty() || !devices.inputs.is_empty(),
                    "{devices:?}"
                );
                assert!(
                    devices.inputs.iter().all(|d| !is_monitor(&d.name)),
                    "{devices:?}"
                );
            }
            Err(CaptureError::HostUnavailable(_)) => {}
            Err(other) => panic!("{other}"),
        }
    }

    /// Monitor sources are listed through their sinks, not as inputs.
    #[test]
    fn monitors_are_told_by_their_name() {
        assert!(is_monitor("alsa_output.pci.monitor"));
        assert!(!is_monitor("alsa_input.usb"));
    }
}
