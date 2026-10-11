//! Playing through cpal, on the default output device of the host nota
//! records through.
//!
//! The whole moment is turned into the device's default rate, as `f32`,
//! before the stream starts (a few seconds of audio), so the stream's
//! callback only copies: it fills each frame of every channel with the next
//! sample, and with silence once they run out. The stream asks for `f32`
//! whatever format the device prefers: the audio server (or ALSA's `default`
//! device) converts. The default device and its volume are never changed.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, HostId, StreamConfig};
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
#[derive(Debug)]
pub struct CpalStream {
    /// The cpal stream, held so it runs until this is dropped. Boxed as
    /// `Any` because cpal's stream has no `Debug` of its own.
    _stream: Box<dyn Any>,
    /// Set once the callback has been asked for audio after the last of
    /// it, or the device failed.
    finished: Arc<AtomicBool>,
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
        let config = device
            .default_output_config()
            .map_err(|e| output_error(&e))?
            .config();
        let frames = resample(samples, rate.hz(), config.sample_rate);
        let finished = Arc::new(AtomicBool::new(false));
        let stream = open(&device, config, frames, &finished)?;
        stream.play().map_err(|e| output_error(&e))?;
        Ok(CpalStream {
            _stream: Box::new(stream),
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

/// Builds the stream on `device` that plays `frames` (one value per frame,
/// at `config`'s rate) on every channel, then silence, and sets `finished`
/// once a buffer comes with nothing left to play, or the device fails.
fn open(
    device: &cpal::Device,
    config: StreamConfig,
    frames: Vec<f32>,
    finished: &Arc<AtomicBool>,
) -> Result<cpal::Stream, OutputError> {
    let channels = usize::from(config.channels).max(1);
    let mut frames = frames.into_iter();
    let done = Arc::clone(finished);
    let failed = Arc::clone(finished);
    device
        .build_output_stream::<f32, _, _>(
            config,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                if nothing_left(out, channels, &mut frames) {
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

/// Fills `out`, `channels` values to a frame, from `frames`, one value to a
/// frame, and with silence once there are none. True if no frame of this
/// buffer came from `frames`: the buffers before it have taken everything,
/// so the sound is on its way out. (Reporting the buffer that takes the
/// last frame would let a caller cut the end off.)
fn nothing_left(out: &mut [f32], channels: usize, frames: &mut impl Iterator<Item = f32>) -> bool {
    let mut played = false;
    for frame in out.chunks_mut(channels) {
        let value = frames.next();
        played |= value.is_some();
        frame.fill(value.unwrap_or(0.0));
    }
    !played && !out.is_empty()
}

/// `samples` at `from` Hz as `f32` in -1..1 at `to` Hz, each new sample
/// interpolated between the two old ones around it.
fn resample(samples: &[i16], from: u32, to: u32) -> Vec<f32> {
    let (from, to) = (i64::from(from.max(1)), i64::from(to.max(1)));
    let count = i64::try_from(samples.len()).unwrap_or(i64::MAX);
    // Past the last sample the last one holds, so the end doesn't fade.
    let pick = |i: i64| {
        let at = usize::try_from(i).unwrap_or(usize::MAX);
        samples
            .get(at)
            .or(samples.last())
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

    /// Doubling the rate puts the average of two neighbours between them,
    /// and the last sample holds.
    #[test]
    fn resampling_up_interpolates_between_neighbours() {
        let out = resample(&[0, 16_384, -16_384], 1, 2);
        assert_eq!(out, [0.0, 0.25, 0.5, 0.0, -0.5, -0.5]);
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

    /// Tripling the rate puts two new samples, a third and two thirds of
    /// the way, between each pair.
    #[test]
    fn resampling_to_three_times_the_rate_interpolates_thirds() {
        let out = resample(&[0, 3_072, 6_144], 16_000, 48_000);
        assert_eq!(out[..4], [0.0, 0.031_25, 0.062_5, 0.093_75]);
    }

    /// Going down takes every other sample at half the rate.
    #[test]
    fn resampling_down_keeps_the_samples_that_line_up() {
        let out = resample(&[0, 8_192, 16_384, 24_576], 2, 1);
        assert_eq!(out, [0.0, 0.5]);
    }

    /// Every channel of a frame gets the same value, and silence follows
    /// the last one; a buffer that held a frame isn't the one that reports
    /// the end.
    #[test]
    fn filling_copies_a_frame_to_every_channel_then_goes_quiet() {
        let mut out = [9.0_f32; 8];
        let mut frames = [0.5_f32, -0.5].into_iter();
        assert!(!nothing_left(&mut out, 2, &mut frames));
        assert_eq!(out.to_vec(), [0.5, 0.5, -0.5, -0.5, 0.0, 0.0, 0.0, 0.0]);
    }

    /// The buffer after the last frame is the one that reports the end, so
    /// a caller that stops on it doesn't cut the sound off.
    #[test]
    fn the_end_is_reported_by_the_buffer_after_the_last_frame() {
        let mut frames = [0.25_f32, 0.5].into_iter();
        let mut out = [9.0_f32; 2];
        assert!(!nothing_left(&mut out, 1, &mut frames));
        assert_eq!(out.to_vec(), [0.25, 0.5]);
        assert!(nothing_left(&mut out, 1, &mut frames));
        assert_eq!(out.to_vec(), [0.0, 0.0]);
    }

    /// A buffer with no room in it says nothing about the end.
    #[test]
    fn an_empty_buffer_doesnt_report_the_end() {
        let mut frames = std::iter::empty();
        assert!(!nothing_left(&mut [], 2, &mut frames));
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

    /// Each of nota's hosts is cpal's own.
    #[test]
    fn every_host_maps_to_its_cpal_host() {
        assert_eq!(host_id(Host::PipeWire), HostId::PipeWire);
        assert_eq!(host_id(Host::PulseAudio), HostId::PulseAudio);
        assert_eq!(host_id(Host::Alsa), HostId::Alsa);
    }

    /// cpal's errors become the reason a card can show.
    #[test]
    fn cpal_errors_become_output_errors() {
        let error = |kind| output_error(&cpal::Error::with_message(kind, "detail"));
        assert_eq!(error(ErrorKind::DeviceNotAvailable), OutputError::NoDevice);
        assert_eq!(
            error(ErrorKind::UnsupportedConfig),
            OutputError::Unsupported("detail".into())
        );
        assert!(matches!(
            error(ErrorKind::DeviceBusy),
            OutputError::Backend(_)
        ));
    }

    /// The stream says it has finished once its flag is set.
    #[test]
    fn a_stream_reports_the_flag_it_shares_with_its_callback() {
        let finished = Arc::new(AtomicBool::new(false));
        let stream = CpalStream {
            _stream: Box::new(()),
            finished: Arc::clone(&finished),
        };
        assert!(!stream.finished());
        finished.store(true, Ordering::Release);
        assert!(stream.finished());
    }
}
