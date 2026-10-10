//! The audio server's devices, as Setup lists them before a session exists.
//!
//! A [`Devices`] is a snapshot: the sinks (capturing one records what plays
//! on it), the sources (microphones and the like) and which of each the
//! server makes the default. A [`CaptureBackend`](super::CaptureBackend)
//! gives one from [`devices`](super::CaptureBackend::devices); the preview
//! asks again every few seconds so a headset plugged in while Setup is open
//! shows up.

/// One device of the audio server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// The server's name for it, as [`Source::Device`](super::Source::Device) names it.
    pub name: String,
    /// Its name as the user knows it ("Speakers"); its node name if it has
    /// no other.
    pub description: String,
}

/// The audio server's sinks and sources, and its defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Devices {
    /// The sinks: a [`Source::Device`](super::Source::Device) on one records what plays on it. By
    /// description, then name.
    pub outputs: Vec<Device>,
    /// The sources, such as microphones. By description, then name.
    pub inputs: Vec<Device>,
    /// The name of the default sink, which [`Source::SystemAudio`](super::Source::SystemAudio) follows.
    pub default_output: Option<String>,
    /// The name of the default source, which [`Source::Microphone`](super::Source::Microphone)
    /// follows.
    pub default_input: Option<String>,
}

#[cfg(test)]
mod tests {
    use nota_core::SampleRate;

    use super::super::{CaptureBackend, CaptureError, CaptureSender, Source};
    use super::*;

    /// A backend that says nothing about devices, as a test's fake does.
    struct Silent;

    impl CaptureBackend for Silent {
        type Stream = ();

        fn start(
            &self,
            source: &Source,
            _rate: SampleRate,
            _events: CaptureSender,
        ) -> Result<(), CaptureError> {
            Err(CaptureError::DeviceNotAvailable(source.clone()))
        }
    }

    /// A backend with no server to ask can't list devices: that's an
    /// error, not an empty list, so a screen can tell "none" from "can't
    /// say".
    #[test]
    fn a_backend_with_no_server_cannot_list_devices() {
        assert_eq!(
            Silent.devices(),
            Err(CaptureError::Backend(
                "this backend can't list devices".to_owned()
            ))
        );
    }
}
