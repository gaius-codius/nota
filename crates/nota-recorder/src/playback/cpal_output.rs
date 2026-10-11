//! Playing through cpal, on the default output device of the host nota
//! records through.
//!
//! The whole moment is turned into the device's rate, as `f32`, before the
//! stream starts (a few seconds of audio), so the stream's callback only
//! copies: it fills each frame of every channel with the next sample, and
//! with silence once they run out. The device's own format is used, as its
//! default output config gives it; the default device and its volume are
//! never changed.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, HostId, SampleFormat, SizedSample, StreamConfig};
use nota_core::SampleRate;

use super::{Output, OutputError, OutputStream};
use crate::capture::{AudioBackend, Host};

/// How long opening a stream may wait for the audio server to answer.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// Plays on the default output device of the host chosen when it was made
/// (see [`AudioBackend`]).
#[derive(Debug, Clone, Copy)]
pub struct CpalOutput {
    /// The host to play through.
    host: Host,
}

impl CpalOutput {
    /// The output for this machine's host, as [`AudioBackend::detect`]
    /// finds it.
    #[must_use]
    pub fn detect() -> Self {
        Self {
            host: AudioBackend::detect().host(),
        }
    }
}

/// A moment playing on the device. The sound stops when it drops.
pub struct CpalStream {
    /// Held so the stream runs until this is dropped.
    _stream: cpal::Stream,
    /// Set once the callback has run out of samples, or the device failed.
    finished: Arc<AtomicBool>,
}

impl std::fmt::Debug for CpalStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CpalStream")
    }
}

impl OutputStream for CpalStream {
    fn finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

impl Output for CpalOutput {
    type Stream = CpalStream;

    fn start(&self, samples: &[i16], rate: SampleRate) -> Result<CpalStream, OutputError> {
        let host = cpal::host_from_id(host_id(self.host)).map_err(|e| output_error(&e))?;
        let device = host.default_output_device().ok_or(OutputError::NoDevice)?;
        let supported = device
            .default_output_config()
            .map_err(|e| output_error(&e))?;
        let config = supported.config();
        let frames = resample(samples, rate.hz(), config.sample_rate);
        let finished = Arc::new(AtomicBool::new(false));
        let stream = match supported.sample_format() {
            SampleFormat::F32 => open::<f32>(&device, config, frames, &finished)?,
            SampleFormat::I16 => open::<i16>(&device, config, frames, &finished)?,
            other => return Err(OutputError::Unsupported(other.to_string())),
        };
        stream.play().map_err(|e| output_error(&e))?;
        Ok(CpalStream {
            _stream: stream,
            finished,
        })
    }
}

/// The cpal host for one of nota's.
const fn host_id(host: Host) -> HostId {
    match host {
        Host::PipeWire => HostId::PipeWire,
        Host::PulseAudio => HostId::PulseAudio,
        Host::Alsa => HostId::Alsa,
    }
}

/// Builds the stream on `device` that plays `frames` (one `f32` per frame,
/// at `config`'s rate) on every channel, then silence, and sets `finished`
/// when they run out or the device fails.
fn open<T>(
    device: &cpal::Device,
    config: StreamConfig,
    frames: Vec<f32>,
    finished: &Arc<AtomicBool>,
) -> Result<cpal::Stream, OutputError>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels).max(1);
    let mut frames = frames.into_iter();
    let done = Arc::clone(finished);
    let failed = Arc::clone(finished);
    device
        .build_output_stream::<T, _, _>(
            config,
            move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
                if fill(out, channels, &mut frames) {
                    done.store(true, Ordering::Release);
                }
            },
            move |error: cpal::Error| {
                if ends_playback(error.kind()) {
                    failed.store(true, Ordering::Release);
                }
            },
            Some(OPEN_TIMEOUT),
        )
        .map_err(|e| output_error(&e))
}

/// Fills `out`, `channels` samples to a frame, from `frames`, one value to a
/// frame, and with silence once there are none; true if they ran out.
fn fill<T>(out: &mut [T], channels: usize, frames: &mut impl Iterator<Item = f32>) -> bool
where
    T: SizedSample + FromSample<f32>,
{
    let mut ran_out = false;
    for frame in out.chunks_mut(channels) {
        let value = frames.next();
        ran_out |= value.is_none();
        frame.fill(T::from_sample(value.unwrap_or(0.0)));
    }
    ran_out
}

/// `samples` at `from` Hz as `f32` in -1..1 at `to` Hz, each new sample
/// interpolated between the two old ones around it.
fn resample(samples: &[i16], from: u32, to: u32) -> Vec<f32> {
    let (from, to) = (i64::from(from.max(1)), i64::from(to.max(1)));
    let count = i64::try_from(samples.len()).unwrap_or(i64::MAX);
    let pick = |i: i64| {
        usize::try_from(i)
            .ok()
            .and_then(|i| samples.get(i))
            .map_or(0, |&s| i64::from(s))
    };
    (0..count * to / from)
        .map(|n| {
            // Where the new sample falls: `within` parts in `to` of the way
            // from the old sample `before` to the next.
            let (before, within) = (n * from / to, n * from % to);
            let mixed = (pick(before) * (to - within) + pick(before + 1) * within) / to;
            f32::from(i16::try_from(mixed).unwrap_or(0)) / 32_768.0
        })
        .collect()
}

/// Whether a stream error means nothing more will be heard. A dropout, a
/// change of default device, a refusal of real-time priority (rtkit is often
/// absent) and a backend's warning leave the stream running.
const fn ends_playback(kind: ErrorKind) -> bool {
    !matches!(
        kind,
        ErrorKind::Xrun
            | ErrorKind::DeviceChanged
            | ErrorKind::RealtimeDenied
            | ErrorKind::ResourceExhausted
            | ErrorKind::BackendError
    )
}

/// An [`OutputError`] for what cpal reported.
fn output_error(error: &cpal::Error) -> OutputError {
    match error.kind() {
        ErrorKind::DeviceNotAvailable => OutputError::NoDevice,
        ErrorKind::UnsupportedConfig => OutputError::Unsupported(error.to_string()),
        _ => OutputError::Backend(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Doubling the rate puts the average of two neighbours between them.
    #[test]
    fn resampling_up_interpolates_between_neighbours() {
        let out = resample(&[0, 16_384, -16_384], 1, 2);
        assert_eq!(out, [0.0, 0.25, 0.5, 0.0, -0.5, -0.25]);
    }

    /// The same rate gives the samples back, scaled.
    #[test]
    fn resampling_at_the_same_rate_only_scales() {
        let out = resample(&[-32_768, 0, 8_192], 16_000, 16_000);
        assert_eq!(out, [-1.0, 0.0, 0.25]);
    }

    /// Going from 16 kHz to 48 kHz triples the count.
    #[test]
    fn resampling_to_three_times_the_rate_triples_the_samples() {
        assert_eq!(resample(&[1; 100], 16_000, 48_000).len(), 300);
    }

    /// Every channel of a frame gets the same value, and silence follows
    /// the last one; the ending is reported once the frames ran out.
    #[test]
    fn filling_copies_a_frame_to_every_channel_then_goes_quiet() {
        let mut out = [9_i16; 8];
        let mut frames = [0.5_f32, -0.5].into_iter();
        assert!(fill(&mut out, 2, &mut frames));
        assert_eq!(out, [16_384, 16_384, -16_384, -16_384, 0, 0, 0, 0]);
    }

    /// A buffer the frames fill exactly hasn't run out yet.
    #[test]
    fn filling_a_buffer_exactly_doesnt_report_the_end() {
        let mut out = [9_i16; 3];
        let mut frames = [0.25_f32, 0.5, -0.25].into_iter();
        assert!(!fill(&mut out, 1, &mut frames));
        assert_eq!(out, [8_192, 16_384, -8_192]);
    }

    /// The warnings a stream reports while it runs don't end the moment; a
    /// device that goes away does. (Real-time priority is refused on any
    /// machine without rtkit, and cut a moment short at the first run.)
    #[test]
    fn only_a_failed_device_ends_playback() {
        for kind in [
            ErrorKind::Xrun,
            ErrorKind::DeviceChanged,
            ErrorKind::RealtimeDenied,
            ErrorKind::ResourceExhausted,
            ErrorKind::BackendError,
        ] {
            assert!(!ends_playback(kind), "{kind:?}");
        }
        assert!(ends_playback(ErrorKind::DeviceNotAvailable));
    }

    /// With a server and a device the moment starts and ends; without one,
    /// it says why. (CI has none; the development machine has both.)
    #[test]
    fn the_default_output_plays_or_says_why_not() {
        match CpalOutput::detect().start(&[0; 160], SampleRate::SPEECH) {
            Ok(stream) => drop(stream),
            Err(OutputError::NoDevice | OutputError::Backend(_) | OutputError::Unsupported(_)) => {}
        }
    }
}
