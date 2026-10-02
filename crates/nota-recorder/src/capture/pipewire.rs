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
                // Only a copy into the channel: no I/O, no locks.
                move |samples: &[i16], _| events.audio(samples),
                move |error: cpal::Error| match error.kind() {
                    ErrorKind::Xrun => errors.notice(CaptureNotice::Overrun),
                    ErrorKind::DeviceChanged => errors.notice(CaptureNotice::RouteChanged),
                    ErrorKind::RealtimeDenied => errors.notice(CaptureNotice::RealtimeDenied),
                    ErrorKind::DeviceNotAvailable => {
                        errors.failed(CaptureError::DeviceNotAvailable(failed_source.clone()));
                    }
                    _ => errors.failed(CaptureError::Backend(error.to_string())),
                },
                None,
            )
            .map_err(|e| start_error(source, &e))?;
        stream.play().map_err(|e| start_error(source, &e))?;
        Ok(PipeWireStream { _stream: stream })
    }
}

fn start_error(source: &Source, error: &cpal::Error) -> CaptureError {
    match error.kind() {
        ErrorKind::HostUnavailable => CaptureError::HostUnavailable(error.to_string()),
        ErrorKind::UnsupportedConfig => CaptureError::UnsupportedConfig(error.to_string()),
        _ => CaptureError::Backend(format!("{source}: {error} ({:?})", error.kind())),
    }
}
