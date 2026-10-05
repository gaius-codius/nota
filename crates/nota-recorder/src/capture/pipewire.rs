//! Capture through cpal's `PipeWire` host.
//!
//! # Real-time priority
//!
//! The stream's callback runs on a thread cpal starts for it. On its first
//! buffer the callback moves that thread to `SCHED_FIFO` at
//! [`RT_PRIORITY`] itself, which the kernel allows within the user's
//! `RLIMIT_RTPRIO` (a `realtime` or `pipewire` group, say). cpal's
//! `realtime-dbus` feature also asks rtkit to make it real-time, from a
//! thread of its own: that covers desktops where only rtkit grants it.
//! `PipeWire`'s own module-rt tries them in the same order. cpal asks rtkit
//! whatever the direct promotion did: rtkit's refusal is reported as a
//! [`CaptureNotice::Warning`] only if the direct promotion failed too (or
//! the refusal came back first), and if both succeed, rtkit's priority
//! (`SCHED_RR` 10) is the one that stays. Asking rtkit also lowers the
//! process's `RLIMIT_RTTIME` soft limit to about one quantum: a real-time
//! thread that runs that long without blocking gets `SIGXCPU`. The callback
//! blocks every buffer, so it stays well inside.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, DeviceId, ErrorKind, HostId, StreamConfig};
use nota_core::SampleRate;
use thread_priority::{
    RealtimeThreadSchedulePolicy, ThreadPriority, ThreadPriorityValue, ThreadSchedulePolicy,
    set_thread_priority_and_policy, thread_native_id,
};

use super::{CaptureBackend, CaptureError, CaptureNotice, CaptureSender, Source};

/// The `SCHED_FIFO` priority the callback's thread asks for: `PipeWire`'s
/// default for client threads, below its own (88).
const RT_PRIORITY: u8 = 83;

/// Moves the calling thread to `SCHED_FIFO` at [`RT_PRIORITY`]. `false` if
/// the kernel refused, as it does beyond the user's `RLIMIT_RTPRIO`.
fn promote_current_thread() -> bool {
    ThreadPriorityValue::try_from(RT_PRIORITY).is_ok_and(|value| {
        set_thread_priority_and_policy(
            thread_native_id(),
            ThreadPriority::Crossplatform(value),
            ThreadSchedulePolicy::Realtime(RealtimeThreadSchedulePolicy::Fifo),
        )
        .is_ok()
    })
}

/// Promotes the callback's thread once, on its first buffer, and
/// remembers whether that worked.
#[derive(Debug)]
struct Promotion {
    tried: bool,
    promoted: Arc<AtomicBool>,
}

impl Promotion {
    fn new() -> Self {
        Self {
            tried: false,
            promoted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Runs `promote` the first time only.
    fn on_buffer(&mut self, promote: impl FnOnce() -> bool) {
        if !self.tried {
            self.tried = true;
            self.promoted.store(promote(), Ordering::SeqCst);
        }
    }
}

/// Captures from `PipeWire`, through cpal. `PipeWire` converts whatever the
/// device runs at to mono 16-bit samples at the rate asked for.
///
/// [`Source::SystemAudio`] captures the default output's monitor, and
/// follows the default output when it changes; so does
/// [`Source::Microphone`] for the default input. A [`Source::Device`]
/// stays on that node.
///
/// The stream's thread asks for real-time priority on its first buffer:
/// `SCHED_FIFO` directly where the user's rtprio limit allows it, and rtkit
/// otherwise. Refused both ways, it records at normal priority and reports
/// a [`CaptureNotice::Warning`].
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
        let mut promotion = Promotion::new();
        let promoted = Arc::clone(&promotion.promoted);
        let stream = device
            .build_input_stream::<i16, _, _>(
                config,
                // Only a copy into the channel, in a buffer it already has:
                // no I/O, no waiting on the recorder, and in the steady
                // state no allocation. The first buffer also asks for
                // real-time priority, a few system calls.
                move |samples: &[i16], _| {
                    promotion.on_buffer(promote_current_thread);
                    events.audio(samples);
                },
                move |error: cpal::Error| match stream_error(
                    &failed_source,
                    &error,
                    promoted.load(Ordering::SeqCst),
                ) {
                    Some(Ok(notice)) => errors.notice(notice),
                    Some(Err(failure)) => errors.failed(failure),
                    None => {}
                },
                None,
            )
            .map_err(|e| start_error(source, &e))?;
        stream.play().map_err(|e| start_error(source, &e))?;
        Ok(PipeWireStream { _stream: stream })
    }
}

/// What an error the running stream reports means: a notice, with
/// capture going on, a failure that ends it, or nothing to report. With
/// `promoted`, the thread is already real-time, so rtkit's refusal doesn't
/// matter.
fn stream_error(
    source: &Source,
    error: &cpal::Error,
    promoted: bool,
) -> Option<Result<CaptureNotice, CaptureError>> {
    Some(match error.kind() {
        ErrorKind::Xrun => Ok(CaptureNotice::Overrun),
        ErrorKind::DeviceChanged => Ok(CaptureNotice::RouteChanged),
        // Real-time priority refused by rtkit.
        ErrorKind::RealtimeDenied if promoted => return None,
        // The same with the direct promotion refused too (or not tried
        // yet); cpal unable to start the thread that asks rtkit, as the
        // stream is built (the only `ResourceExhausted` a stream reports
        // through this callback); and anything cpal flags as a backend
        // error while the stream runs, such as the default-device watch
        // failing to start: the stream goes on.
        ErrorKind::RealtimeDenied | ErrorKind::ResourceExhausted | ErrorKind::BackendError => {
            Ok(CaptureNotice::Warning(error.to_string()))
        }
        ErrorKind::DeviceNotAvailable => Err(CaptureError::DeviceNotAvailable(source.clone())),
        _ => Err(CaptureError::Backend(error.to_string())),
    })
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
            (ErrorKind::Xrun, CaptureNotice::Overrun),
            (ErrorKind::DeviceChanged, CaptureNotice::RouteChanged),
            (
                ErrorKind::RealtimeDenied,
                CaptureNotice::Warning("detail".into()),
            ),
            (
                ErrorKind::ResourceExhausted,
                CaptureNotice::Warning("detail".into()),
            ),
            (
                ErrorKind::BackendError,
                CaptureNotice::Warning("detail".into()),
            ),
        ] {
            assert_eq!(
                stream_error(&mic, &error(kind), false),
                Some(Ok(notice.clone())),
                "{kind:?}"
            );
            if kind != ErrorKind::RealtimeDenied {
                assert_eq!(
                    stream_error(&mic, &error(kind), true),
                    Some(Ok(notice)),
                    "{kind:?}"
                );
            }
        }
    }

    #[test]
    fn rtkit_refusing_after_a_direct_promotion_is_not_reported() {
        let mic = Source::Microphone;
        assert_eq!(
            stream_error(&mic, &error(ErrorKind::RealtimeDenied), true),
            None
        );
    }

    #[test]
    fn promoting_says_whether_the_thread_is_now_real_time() {
        // On a thread of its own, so the test runner's stays as it was.
        // Whether the kernel allows it depends on the machine's rtprio
        // limit; either way the answer must match the thread's policy, and
        // where the limit allows it the promotion must happen.
        let limit = rustix::process::getrlimit(rustix::process::Resource::Rtprio).current;
        let allowed = limit.is_none_or(|limit| limit >= u64::from(RT_PRIORITY));
        let (promoted, policy) = std::thread::spawn(|| {
            let promoted = promote_current_thread();
            (promoted, thread_priority::thread_schedule_policy().unwrap())
        })
        .join()
        .unwrap();
        let fifo = ThreadSchedulePolicy::Realtime(RealtimeThreadSchedulePolicy::Fifo);
        assert_eq!(promoted, policy == fifo, "{policy:?}");
        if allowed {
            assert!(promoted, "rtprio limit {limit:?} allows {RT_PRIORITY}");
        }
        if promoted {
            let priority = std::thread::spawn(|| {
                promote_current_thread();
                thread_priority::get_current_thread_priority().unwrap()
            })
            .join()
            .unwrap();
            assert_eq!(
                priority,
                ThreadPriority::Crossplatform(ThreadPriorityValue::try_from(RT_PRIORITY).unwrap())
            );
        }
    }

    #[test]
    fn the_thread_is_promoted_on_the_first_buffer_only() {
        for outcome in [true, false] {
            let mut promotion = Promotion::new();
            assert!(!promotion.promoted.load(Ordering::SeqCst));
            let mut calls = 0;
            for _ in 0..3 {
                promotion.on_buffer(|| {
                    calls += 1;
                    outcome
                });
            }
            assert_eq!(calls, 1);
            assert_eq!(promotion.promoted.load(Ordering::SeqCst), outcome);
        }
    }

    #[test]
    fn other_stream_errors_are_failures() {
        let mic = Source::Microphone;
        for promoted in [false, true] {
            assert_eq!(
                stream_error(&mic, &error(ErrorKind::DeviceNotAvailable), promoted),
                Some(Err(CaptureError::DeviceNotAvailable(mic.clone())))
            );
            for kind in [ErrorKind::StreamInvalidated, ErrorKind::Other] {
                assert_eq!(
                    stream_error(&mic, &error(kind), promoted),
                    Some(Err(CaptureError::Backend("detail".into()))),
                    "{kind:?}"
                );
            }
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
