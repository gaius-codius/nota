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
use cpal::{BufferSize, DeviceDirection, DeviceId, HostId};
use nota_core::SampleRate;

use super::cpal_input::{self, Listed, PlainStream};
use super::{CaptureBackend, CaptureError, CaptureSender, Devices, Source};

/// ALSA's name for its default capture device, which converts to the
/// rate and format asked for. cpal builds it itself
/// ([`HostTrait::default_input_device`]): a name lookup finds only the
/// devices ALSA's hints list, and a system with no `PipeWire` or
/// `PulseAudio` plugin may not list `default`.
const DEFAULT_INPUT: &str = "default";

/// Captures the microphone from ALSA, through cpal.
#[derive(Debug, Default, Clone, Copy)]
pub struct AlsaBackend;

/// The connection to ALSA.
fn host() -> Result<cpal::Host, CaptureError> {
    cpal::host_from_id(HostId::Alsa).map_err(|e| CaptureError::HostUnavailable(e.to_string()))
}

/// What records `source` on ALSA.
#[derive(Debug, PartialEq, Eq)]
enum Input<'a> {
    /// The system audio, which ALSA can't record.
    None,
    /// The default capture device.
    Default,
    /// The device ALSA's hints list under this name.
    Named(&'a str),
}

/// The ALSA input that records `source`.
fn input_for(source: &Source) -> Input<'_> {
    match source {
        Source::SystemAudio => Input::None,
        Source::Microphone => Input::Default,
        Source::Device(name) => Input::Named(name),
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
        let (device, name) = match input_for(source) {
            Input::None => return Err(CaptureError::NoSystemAudio),
            Input::Default => (host()?.default_input_device(), DEFAULT_INPUT),
            Input::Named(name) => (
                host()?.device_by_id(&DeviceId::new(HostId::Alsa, name)),
                name,
            ),
        };
        let device = device.ok_or_else(|| CaptureError::DeviceNotAvailable(source.clone()))?;
        let pinned = Source::Device(name.to_owned());
        cpal_input::open(
            &device,
            (source, &pinned),
            rate,
            BufferSize::Default,
            events,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nota_core::{Clock, FakeClock, SessionTime, TrackId};

    use super::super::start;
    use super::*;

    /// ALSA records the microphone and named devices, never the system
    /// audio.
    #[test]
    fn only_the_system_audio_has_no_alsa_input() {
        assert_eq!(input_for(&Source::SystemAudio), Input::None);
        assert_eq!(input_for(&Source::Microphone), Input::Default);
        assert_eq!(
            input_for(&Source::Device("hw:1,0".to_owned())),
            Input::Named("hw:1,0")
        );
    }

    /// Asked for the system audio, ALSA refuses before it touches any
    /// device, with the message that says what would let it record.
    #[test]
    fn the_system_audio_is_refused_with_its_message() {
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
        let rate = SampleRate::new(16_000).unwrap();
        let refused = start(
            &AlsaBackend,
            TrackId::new(0),
            &Source::SystemAudio,
            rate,
            &clock,
        );
        assert_eq!(refused.err(), Some(CaptureError::NoSystemAudio));
    }
}
