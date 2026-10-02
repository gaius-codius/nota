//! Capture through cpal's `PipeWire` host.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, DeviceId, ErrorKind, HostId, StreamConfig};
use nota_core::SampleRate;

use super::{CaptureBackend, CaptureError, CaptureNotice, CaptureSender, Source};

/// Captures from `PipeWire`, through cpal. `PipeWire` converts whatever the
/// device runs at to mono 16-bit samples at the rate asked for.
///
/// [`Source::SystemAudio`] captures the default output's monitor, and
/// follows the default output when it changes; so does
/// [`Source::Microphone`] for the default input. A [`Source::Device`]
/// stays on that node.
#[derive(Debug, Default, Clone, Copy)]
pub struct PipeWireBackend;

/// A running `PipeWire` stream. Capture stops when it's dropped.
pub struct PipeWireStream {
    _stream: cpal::Stream,
}

impl std::fmt::Debug for PipeWireStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PipeWireStream")
    }
}

impl CaptureBackend for PipeWireBackend {
    type Stream = PipeWireStream;

    fn start(
        &self,
        source: &Source,
        rate: SampleRate,
        events: CaptureSender,
    ) -> Result<PipeWireStream, CaptureError> {
        let host = cpal::host_from_id(HostId::PipeWire)
            .map_err(|e| CaptureError::HostUnavailable(e.to_string()))?;
        // cpal names the default sink's monitor `sink_default`; its
        // `default_output_device` is for playback only.
        let name = match source {
            Source::SystemAudio => "sink_default",
            Source::Microphone => "input_default",
            Source::Device(name) => name,
        };
        let device = host
            .device_by_id(&DeviceId::new(HostId::PipeWire, name))
            .ok_or_else(|| CaptureError::DeviceNotAvailable(source.clone()))?;
        let config = StreamConfig {
            channels: 1,
            sample_rate: rate.hz(),
            buffer_size: BufferSize::Default,
        };
        let errors = events.clone();
        let failed_source = source.clone();
        let stream = device
            .build_input_stream::<i16, _, _>(
                config,
                // Only a copy into the channel: no I/O and no waiting on the
                // recorder; one allocation per buffer.
                move |samples: &[i16], _| events.audio(samples),
                move |error: cpal::Error| match stream_error(&failed_source, &error) {
                    Ok(notice) => errors.notice(notice),
                    Err(failure) => errors.failed(failure),
                },
                None,
            )
            .map_err(|e| start_error(source, &e))?;
        stream.play().map_err(|e| start_error(source, &e))?;
        Ok(PipeWireStream { _stream: stream })
    }
}

/// What an error the running stream reports means: a notice, with
/// capture going on, or a failure that ends it.
fn stream_error(source: &Source, error: &cpal::Error) -> Result<CaptureNotice, CaptureError> {
    match error.kind() {
        ErrorKind::Xrun => Ok(CaptureNotice::Overrun),
        ErrorKind::DeviceChanged => Ok(CaptureNotice::RouteChanged),
        // Real-time priority refused (cpal's `realtime` feature is off, so
        // it isn't asked for yet), and the default-device watch failing to
        // start (`BackendError`, the only one a running `PipeWire` stream
        // reports): the stream goes on.
        ErrorKind::RealtimeDenied | ErrorKind::BackendError => {
            Ok(CaptureNotice::Warning(error.to_string()))
        }
        ErrorKind::DeviceNotAvailable => Err(CaptureError::DeviceNotAvailable(source.clone())),
        _ => Err(CaptureError::Backend(error.to_string())),
    }
}

/// Why the stream couldn't be opened or started.
fn start_error(source: &Source, error: &cpal::Error) -> CaptureError {
    match error.kind() {
        ErrorKind::HostUnavailable => CaptureError::HostUnavailable(error.to_string()),
        ErrorKind::DeviceNotAvailable => CaptureError::DeviceNotAvailable(source.clone()),
        ErrorKind::UnsupportedConfig => CaptureError::UnsupportedConfig(error.to_string()),
        _ => CaptureError::Backend(format!("{source}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(kind: ErrorKind) -> cpal::Error {
        cpal::Error::with_message(kind, "detail")
    }

    #[test]
    fn stream_errors_that_dont_stop_capture_are_notices() {
        let mic = Source::Microphone;
        for (kind, notice) in [
            (ErrorKind::DeviceChanged, CaptureNotice::RouteChanged),
            (
                ErrorKind::RealtimeDenied,
                CaptureNotice::Warning("detail".into()),
            ),
            (
                ErrorKind::BackendError,
                CaptureNotice::Warning("detail".into()),
            ),
        ] {
            assert_eq!(stream_error(&mic, &error(kind)), Ok(notice), "{kind:?}");
        }
    }

    #[test]
    fn other_stream_errors_are_failures() {
        let mic = Source::Microphone;
        assert_eq!(
            stream_error(&mic, &error(ErrorKind::DeviceNotAvailable)),
            Err(CaptureError::DeviceNotAvailable(mic.clone()))
        );
        for kind in [ErrorKind::StreamInvalidated, ErrorKind::Other] {
            assert_eq!(
                stream_error(&mic, &error(kind)),
                Err(CaptureError::Backend("detail".into())),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn start_errors_keep_their_kind() {
        let node = Source::Device("node".into());
        assert_eq!(
            start_error(&node, &error(ErrorKind::HostUnavailable)),
            CaptureError::HostUnavailable("detail".into())
        );
        assert_eq!(
            start_error(&node, &error(ErrorKind::DeviceNotAvailable)),
            CaptureError::DeviceNotAvailable(node.clone())
        );
        assert_eq!(
            start_error(&node, &error(ErrorKind::UnsupportedConfig)),
            CaptureError::UnsupportedConfig("detail".into())
        );
        assert_eq!(
            start_error(&node, &error(ErrorKind::PermissionDenied)),
            CaptureError::Backend("device node: detail".into())
        );
    }
}
