//! Capture through cpal's ALSA host, the last resort when neither
//! `PipeWire` nor `PulseAudio` is running.
//!
//! ALSA has no way to record what a sound card plays, so only the
//! microphone records: [`Source::SystemAudio`] fails to start with
//! [`CaptureError::NoSystemAudio`], which says so, and the recording goes
//! on with the other track. Like the `PulseAudio` host
//! ([`pulse`](super::pulse)), this one stamps nothing, follows no default
//! and reports no device events but the stream's own failure.

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{DeviceDirection, DeviceId, HostId};
use nota_core::SampleRate;

use super::cpal_input::{self, Listed, PlainStream};
use super::{CaptureBackend, CaptureError, CaptureSender, Devices, Source};

/// ALSA's name for its default capture device, which converts to the
/// rate and format asked for.
const DEFAULT_INPUT: &str = "default";

/// Captures the microphone from ALSA, through cpal.
#[derive(Debug, Default, Clone, Copy)]
pub struct AlsaBackend;

fn host() -> Result<cpal::Host, CaptureError> {
    cpal::host_from_id(HostId::Alsa).map_err(|e| CaptureError::HostUnavailable(e.to_string()))
}

/// The name of the ALSA device that records `source`: `None` for the
/// system audio, which ALSA can't record.
fn input_name(source: &Source) -> Option<&str> {
    match source {
        Source::SystemAudio => None,
        Source::Microphone => Some(DEFAULT_INPUT),
        Source::Device(name) => Some(name),
    }
}

impl CaptureBackend for AlsaBackend {
    type Stream = PlainStream;

    fn devices(&self) -> Result<Devices, CaptureError> {
        let host = host()?;
        let listed = host
            .devices()
            .map_err(|e| CaptureError::Backend(e.to_string()))?
            .filter_map(|device| {
                let description = device.description().ok()?;
                if description.direction() == DeviceDirection::Output {
                    return None;
                }
                Some(Listed {
                    name: device.id().ok()?.id().to_owned(),
                    description: description.name().to_owned(),
                    plays: false,
                })
            });
        Ok(cpal_input::devices_from(
            listed,
            None,
            Some(DEFAULT_INPUT.to_owned()),
        ))
    }

    fn start(
        &self,
        source: &Source,
        rate: SampleRate,
        events: CaptureSender,
    ) -> Result<PlainStream, CaptureError> {
        let name = input_name(source).ok_or(CaptureError::NoSystemAudio)?;
        let device = host()?
            .device_by_id(&DeviceId::new(HostId::Alsa, name))
            .ok_or_else(|| CaptureError::DeviceNotAvailable(source.clone()))?;
        cpal_input::open(&device, &Source::Device(name.to_owned()), rate, events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ALSA records the microphone and named devices, never the system
    /// audio.
    #[test]
    fn only_the_system_audio_has_no_alsa_device() {
        assert_eq!(input_name(&Source::SystemAudio), None);
        assert_eq!(input_name(&Source::Microphone), Some("default"));
        assert_eq!(
            input_name(&Source::Device("hw:1,0".to_owned())),
            Some("hw:1,0")
        );
    }
}
