//! Which audio host records: `PipeWire` when it's running, else
//! `PulseAudio`, else ALSA.
//!
//! [`choose_host`] asks a [`HostProbe`] what's running, and
//! [`AudioBackend`] captures through the host it chose, as the one
//! [`CaptureBackend`] `nota` records with. A host is chosen once, when the
//! backend is made: a server that starts or stops afterwards isn't
//! noticed until the next recording.

use std::ffi::OsString;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use cpal::HostId;
use nota_core::SampleRate;

use super::alsa::AlsaBackend;
use super::cpal_input::PlainStream;
use super::pipewire::PipeWireStream;
use super::pulse::PulseBackend;
use super::{CaptureBackend, CaptureError, CaptureSender, Devices, PipeWireBackend, Source};

/// An audio host nota can capture through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    /// `PipeWire`: system audio and the microphone, with device events.
    PipeWire,
    /// `PulseAudio`: system audio and the microphone, with fewer device
    /// events (see the capture module's docs).
    PulseAudio,
    /// ALSA: the microphone only.
    Alsa,
}

/// What's running on the machine, for [`choose_host`].
pub trait HostProbe {
    /// Whether a `PipeWire` daemon is running and accepts connections.
    fn pipewire_running(&self) -> bool;

    /// Whether a `PulseAudio` server (or `PipeWire`'s stand-in for one)
    /// answers. Asked only when `PipeWire` isn't running.
    fn pulse_reachable(&self) -> bool;
}

/// The host to capture through: `PipeWire` if `probe` finds it running,
/// else `PulseAudio` if it answers, else ALSA, which is always built in
/// and so is the answer when nothing else is running.
pub fn choose_host(probe: &dyn HostProbe) -> Host {
    if probe.pipewire_running() {
        Host::PipeWire
    } else if probe.pulse_reachable() {
        Host::PulseAudio
    } else {
        Host::Alsa
    }
}

/// The probe for the machine nota runs on.
#[derive(Debug, Clone)]
pub struct SystemProbe {
    /// `PIPEWIRE_REMOTE`: which socket the daemon listens on.
    remote: Option<String>,
    /// The directory the daemon's socket is in.
    runtime_dir: Option<PathBuf>,
}

impl SystemProbe {
    /// The probe for what the environment says about `PipeWire`.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            remote: std::env::var("PIPEWIRE_REMOTE").ok(),
            runtime_dir: runtime_dir(
                std::env::var_os("PIPEWIRE_RUNTIME_DIR"),
                std::env::var_os("XDG_RUNTIME_DIR"),
            ),
        }
    }
}

/// The directory `PipeWire`'s socket is in: `pipewire_dir` (its
/// `PIPEWIRE_RUNTIME_DIR`) if it's set and not empty, else `xdg_dir`.
fn runtime_dir(pipewire_dir: Option<OsString>, xdg_dir: Option<OsString>) -> Option<PathBuf> {
    pipewire_dir
        .filter(|dir| !dir.is_empty())
        .or(xdg_dir)
        .map(PathBuf::from)
}

/// What `PipeWire` calls its socket when `PIPEWIRE_REMOTE` doesn't say.
const DEFAULT_PIPEWIRE_REMOTE: &str = "pipewire-0";

/// The socket of the `PipeWire` daemon, given the `PIPEWIRE_REMOTE`
/// variable and the runtime directory (`PIPEWIRE_RUNTIME_DIR`, else
/// `XDG_RUNTIME_DIR`, as libpipewire reads them): the variable names a
/// socket in the runtime directory, or is a path of its own.
fn pipewire_socket(remote: Option<&str>, runtime_dir: Option<&Path>) -> Option<PathBuf> {
    let remote = remote
        .filter(|name| !name.is_empty())
        .unwrap_or(DEFAULT_PIPEWIRE_REMOTE);
    if remote.starts_with('/') {
        return Some(PathBuf::from(remote));
    }
    runtime_dir.map(|dir| dir.join(remote))
}

impl HostProbe for SystemProbe {
    /// Connects to the daemon's socket: a stale socket file from a daemon
    /// that has gone refuses, which is how it's told from one running.
    fn pipewire_running(&self) -> bool {
        pipewire_socket(self.remote.as_deref(), self.runtime_dir.as_deref())
            .is_some_and(|socket| UnixStream::connect(socket).is_ok())
    }

    fn pulse_reachable(&self) -> bool {
        cpal::host_from_id(HostId::PulseAudio).is_ok()
    }
}

/// Captures through the host chosen when it was made: `PipeWire`,
/// `PulseAudio` or ALSA, in that order of preference. On ALSA only the
/// microphone records ([`CaptureError::NoSystemAudio`]).
#[derive(Debug, Clone, Copy)]
pub struct AudioBackend {
    /// The host chosen.
    host: Host,
}

impl AudioBackend {
    /// The backend for this machine's host, as [`SystemProbe`] finds it.
    #[must_use]
    pub fn detect() -> Self {
        Self::with_probe(&SystemProbe::from_env())
    }

    /// The backend for the host `probe` finds.
    #[must_use]
    pub fn with_probe(probe: &dyn HostProbe) -> Self {
        Self {
            host: choose_host(probe),
        }
    }

    /// The host this backend captures through.
    #[must_use]
    pub fn host(&self) -> Host {
        self.host
    }
}

/// A running stream on whichever host the backend chose.
#[derive(Debug)]
pub enum AudioStream {
    /// A stream on `PipeWire`.
    PipeWire(PipeWireStream),
    /// A stream on `PulseAudio` or ALSA.
    Plain(PlainStream),
}

impl CaptureBackend for AudioBackend {
    type Stream = AudioStream;

    fn start(
        &self,
        source: &Source,
        rate: SampleRate,
        events: CaptureSender,
    ) -> Result<AudioStream, CaptureError> {
        Ok(match self.host {
            Host::PipeWire => AudioStream::PipeWire(PipeWireBackend.start(source, rate, events)?),
            Host::PulseAudio => AudioStream::Plain(PulseBackend.start(source, rate, events)?),
            Host::Alsa => AudioStream::Plain(AlsaBackend.start(source, rate, events)?),
        })
    }

    fn devices(&self) -> Result<Devices, CaptureError> {
        match self.host {
            Host::PipeWire => PipeWireBackend.devices(),
            Host::PulseAudio => PulseBackend.devices(),
            Host::Alsa => AlsaBackend.devices(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;

    use nota_core::{Clock, FakeClock, SessionTime, TrackId};

    use super::super::start;
    use super::*;
    use crate::test_dir::TestDir;

    /// A probe with fixed answers that counts how often it was asked.
    struct Fake {
        pipewire: bool,
        pulse: bool,
        pulse_asked: Cell<u32>,
    }

    impl Fake {
        fn new(pipewire: bool, pulse: bool) -> Self {
            Self {
                pipewire,
                pulse,
                pulse_asked: Cell::new(0),
            }
        }
    }

    impl HostProbe for Fake {
        fn pipewire_running(&self) -> bool {
            self.pipewire
        }

        fn pulse_reachable(&self) -> bool {
            self.pulse_asked.set(self.pulse_asked.get() + 1);
            self.pulse
        }
    }

    /// `PipeWire` wins when it runs, even with a `PulseAudio` server
    /// answering (`pipewire-pulse` always does), and `PulseAudio` isn't
    /// even asked: reaching it can take seconds.
    #[test]
    fn pipewire_is_chosen_when_it_runs() {
        let probe = Fake::new(true, true);
        assert_eq!(choose_host(&probe), Host::PipeWire);
        assert_eq!(probe.pulse_asked.get(), 0);
    }

    /// With no `PipeWire`, a `PulseAudio` server is the host.
    #[test]
    fn pulseaudio_is_chosen_without_pipewire() {
        assert_eq!(choose_host(&Fake::new(false, true)), Host::PulseAudio);
    }

    /// With neither, ALSA is what's left.
    #[test]
    fn alsa_is_chosen_when_nothing_else_runs() {
        assert_eq!(choose_host(&Fake::new(false, false)), Host::Alsa);
    }

    /// The backend keeps the host its probe chose.
    #[test]
    fn the_backend_keeps_the_host_its_probe_chose() {
        for (pipewire, pulse, host) in [
            (true, false, Host::PipeWire),
            (false, true, Host::PulseAudio),
            (false, false, Host::Alsa),
        ] {
            let backend = AudioBackend::with_probe(&Fake::new(pipewire, pulse));
            assert_eq!(backend.host(), host);
        }
    }

    /// The backend lists the devices of the host it chose: on ALSA, the
    /// one input that is its default.
    #[test]
    fn the_backend_lists_through_its_host() {
        let backend = AudioBackend::with_probe(&Fake::new(false, false));
        match backend.devices() {
            Ok(devices) => assert_eq!(devices.default_input.as_deref(), Some("default")),
            Err(CaptureError::HostUnavailable(_)) => {}
            Err(other) => panic!("{other}"),
        }
    }

    /// On ALSA only, the system audio fails to start through the
    /// backend, with the message that says what would let it record. (It
    /// is refused before any device is opened, so this needs no sound
    /// card.)
    #[test]
    fn alsa_only_says_the_system_audio_needs_a_server() {
        let backend = AudioBackend::with_probe(&Fake::new(false, false));
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
        let rate = SampleRate::new(16_000).unwrap();
        let refused = start(
            &backend,
            TrackId::new(0),
            &Source::SystemAudio,
            rate,
            &clock,
        );
        let error = refused.expect_err("the system audio can't start on ALSA");
        assert_eq!(error, CaptureError::NoSystemAudio);
        assert_eq!(
            error.to_string(),
            "system audio needs PipeWire or PulseAudio; only the microphone records on ALSA"
        );
    }

    /// A daemon is running where its socket accepts connections, and not
    /// where there's no socket, or one nothing listens on (a daemon that
    /// has gone leaves its file behind).
    #[test]
    fn pipewire_runs_where_its_socket_accepts_connections() {
        let dir = TestDir::new("pipewire-probe");
        let probe = |remote: Option<&str>| SystemProbe {
            remote: remote.map(str::to_owned),
            runtime_dir: Some(dir.0.clone()),
        };
        assert!(!probe(None).pipewire_running(), "no socket");
        let _daemon = UnixListener::bind(dir.0.join("pipewire-0")).unwrap();
        assert!(probe(None).pipewire_running(), "listening");
        assert!(!probe(Some("pipewire-1")).pipewire_running(), "another");
        let stale = dir.0.join("stale");
        drop(UnixListener::bind(&stale).unwrap());
        assert!(stale.exists(), "the socket file is left behind");
        assert!(!probe(Some("stale")).pipewire_running(), "stale");
    }

    /// `PIPEWIRE_RUNTIME_DIR` wins over `XDG_RUNTIME_DIR` unless it's
    /// empty.
    #[test]
    fn the_pipewire_runtime_dir_comes_before_the_xdg_one() {
        let dir =
            |p: Option<&str>, x: Option<&str>| runtime_dir(p.map(Into::into), x.map(Into::into));
        assert_eq!(dir(Some("/pw"), Some("/xdg")), Some(PathBuf::from("/pw")));
        assert_eq!(dir(Some(""), Some("/xdg")), Some(PathBuf::from("/xdg")));
        assert_eq!(dir(None, Some("/xdg")), Some(PathBuf::from("/xdg")));
        assert_eq!(dir(None, None), None);
    }

    /// `PIPEWIRE_REMOTE` names a socket in the runtime directory, or is
    /// a path; without it the default socket is used, and without a
    /// runtime directory there's none to find.
    #[test]
    fn the_pipewire_socket_follows_its_variable() {
        let runtime = Path::new("/run/user/1000");
        let socket = |remote, dir| pipewire_socket(remote, dir);
        assert_eq!(
            socket(None, Some(runtime)),
            Some(PathBuf::from("/run/user/1000/pipewire-0"))
        );
        assert_eq!(
            socket(Some(""), Some(runtime)),
            Some(PathBuf::from("/run/user/1000/pipewire-0"))
        );
        assert_eq!(
            socket(Some("pipewire-1"), Some(runtime)),
            Some(PathBuf::from("/run/user/1000/pipewire-1"))
        );
        assert_eq!(
            socket(Some("/tmp/pw"), None),
            Some(PathBuf::from("/tmp/pw"))
        );
        assert_eq!(socket(None, None), None);
        // A system-wide daemon's directory, as `PIPEWIRE_RUNTIME_DIR` gives it.
        assert_eq!(
            socket(None, Some(Path::new("/run/pipewire"))),
            Some(PathBuf::from("/run/pipewire/pipewire-0"))
        );
    }
}
