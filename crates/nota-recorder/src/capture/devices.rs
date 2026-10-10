//! The audio server's devices, as Setup lists them before a session exists.
//!
//! A [`Devices`] is a snapshot: the sinks (capturing one records what plays
//! on it), the sources (microphones and the like) and which of each the
//! server makes the default. A [`CaptureBackend`](super::CaptureBackend)
//! gives one from [`devices`](super::CaptureBackend::devices); the preview
//! asks again every few seconds so a headset plugged in while Setup is open
//! shows up.

use super::Source;

/// One device of the audio server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// The server's name for it, as [`Source::Device`] names it.
    pub name: String,
    /// Its name as the user knows it ("Speakers"); its node name if it has
    /// no other.
    pub description: String,
}

/// The audio server's sinks and sources, and its defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Devices {
    /// The sinks: a [`Source::Device`] on one records what plays on it. By
    /// description, then name.
    pub outputs: Vec<Device>,
    /// The sources, such as microphones. By description, then name.
    pub inputs: Vec<Device>,
    /// The name of the default sink, which [`Source::SystemAudio`] follows.
    pub default_output: Option<String>,
    /// The name of the default source, which [`Source::Microphone`]
    /// follows.
    pub default_input: Option<String>,
}

impl Devices {
    /// The description of the device `source` records now, or `None` if the
    /// server has no such device: the default sink or source for a source
    /// that follows one, the named device (a sink or a source) for a pinned
    /// one.
    #[must_use]
    pub fn describe(&self, source: &Source) -> Option<&str> {
        match source {
            Source::SystemAudio => named(&self.outputs, self.default_output.as_deref()?),
            Source::Microphone => named(&self.inputs, self.default_input.as_deref()?),
            Source::Device(name) => {
                named(&self.outputs, name).or_else(|| named(&self.inputs, name))
            }
        }
    }
}

/// The description of the device in `list` called `name`.
fn named<'a>(list: &'a [Device], name: &str) -> Option<&'a str> {
    list.iter()
        .find(|device| device.name == name)
        .map(|device| device.description.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str, description: &str) -> Device {
        Device {
            name: name.to_owned(),
            description: description.to_owned(),
        }
    }

    fn devices() -> Devices {
        Devices {
            outputs: vec![device("hdmi", "TV"), device("spk", "Speakers")],
            inputs: vec![device("seiren", "Seiren Mini"), device("spk", "Line in")],
            default_output: Some("spk".to_owned()),
            default_input: Some("seiren".to_owned()),
        }
    }

    /// A followed source is the default of its kind, and each kind looks in
    /// its own list: `spk` is a sink and a source with different names.
    #[test]
    fn a_followed_source_is_the_default_of_its_kind() {
        let devices = devices();
        assert_eq!(devices.describe(&Source::SystemAudio), Some("Speakers"));
        assert_eq!(devices.describe(&Source::Microphone), Some("Seiren Mini"));
    }

    /// A pinned device is found among the sinks first, then the sources.
    #[test]
    fn a_pinned_device_is_found_by_name() {
        let devices = devices();
        assert_eq!(
            devices.describe(&Source::Device("hdmi".to_owned())),
            Some("TV")
        );
        assert_eq!(
            devices.describe(&Source::Device("seiren".to_owned())),
            Some("Seiren Mini")
        );
        assert_eq!(
            devices.describe(&Source::Device("spk".to_owned())),
            Some("Speakers"),
            "the sink wins a name both lists have"
        );
    }

    /// No such device, no default, or a default the list doesn't hold, is
    /// nothing to describe.
    #[test]
    fn a_source_with_no_device_has_no_description() {
        let mut devices = devices();
        assert_eq!(devices.describe(&Source::Device("gone".to_owned())), None);
        devices.default_output = Some("gone".to_owned());
        assert_eq!(devices.describe(&Source::SystemAudio), None);
        devices.default_input = None;
        assert_eq!(devices.describe(&Source::Microphone), None);
    }
}
