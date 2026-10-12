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
//!
//! # Timestamps
//!
//! cpal stamps each buffer with when `PipeWire` captured it: the graph
//! cycle's time less the stream's delay, on `CLOCK_MONOTONIC`, or the
//! callback's time less the buffer's length when the driver hasn't started.
//! Each buffer is sent with its stamp and that delay, the cycle's time
//! less the stamp ([`CaptureSender::audio_captured`]), so the recorder
//! times losses and drift by them. A stream's first buffers carry no
//! delay, as `PipeWire` hasn't measured one yet, so their stamps are when
//! their audio ended: they're held back until a buffer's delay settles,
//! and dated back from it ([`settle`]), so a track's first epoch starts
//! when its audio did. The delay steps when the graph's quantum changes;
//! the cycle's time steps only when the graph does.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, DeviceId, ErrorKind, HostId, StreamConfig};
use nota_core::SampleRate;
use thread_priority::{
    RealtimeThreadSchedulePolicy, ThreadPriority, ThreadPriorityValue, ThreadSchedulePolicy,
    set_thread_priority_and_policy, thread_native_id,
};

use super::{CaptureBackend, CaptureError, CaptureNotice, CaptureSender, Devices, Source};

mod settle;
mod watch;

use settle::{Settling, Stamp, Timed};

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
/// stays on that node: if it goes away, the track's stream ends rather
/// than record whatever `PipeWire` moves it to.
///
/// Each stream has a watch on `PipeWire`'s graph, on a thread of its own,
/// that reports which device the source is on as it changes
/// ([`CaptureSender::device`]): a new default followed, or the device
/// lost. A route change also opens an epoch, as cpal reports it
/// ([`CaptureNotice::RouteChanged`]) or the watch does
/// ([`DeviceChange::Changed`](nota_core::recorder::DeviceChange::Changed)).
///
/// The stream's thread asks for real-time priority on its first buffer:
/// `SCHED_FIFO` directly where the user's rtprio limit allows it, and rtkit
/// otherwise. Refused both ways, it records at normal priority and reports
/// a [`CaptureNotice::Warning`].
#[derive(Debug, Default, Clone, Copy)]
pub struct PipeWireBackend;

/// A running `PipeWire` stream. Capture stops when it's dropped, then the
/// buffers it held back are sent, and then the watch on its device stops:
/// fields drop in order, and cpal's stream joins its thread as it goes, so
/// no buffer comes after those held.
pub struct PipeWireStream {
    _stream: cpal::Stream,
    _held: Held,
    _watch: watch::Watch,
}

/// The buffers a stream holds back ([`Settling`]), sent to its sender
/// when this is dropped.
struct Held {
    settling: Arc<Mutex<Settling>>,
    events: CaptureSender,
}

impl Drop for Held {
    fn drop(&mut self) {
        release(&self.settling, &self.events);
    }
}

impl std::fmt::Debug for PipeWireStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PipeWireStream")
    }
}

impl CaptureBackend for PipeWireBackend {
    type Stream = PipeWireStream;

    fn devices(&self) -> Result<Devices, CaptureError> {
        watch::Subscription::snapshot()
    }

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
        let watched = events.clone();
        let failed_source = source.clone();
        let settling = Arc::new(Mutex::new(Settling::new(rate)));
        let failing = Arc::clone(&settling);
        let held = Arc::clone(&settling);
        let sender = events.clone();
        let mut promotion = Promotion::new();
        let promoted = Arc::clone(&promotion.promoted);
        let stream = device
            .build_input_stream::<i16, _, _>(
                config,
                // Only a copy into the channel, in a buffer it already has,
                // or for a stream's first buffers into room made for them:
                // no I/O, no waiting on the recorder, and in the steady
                // state no allocation. The first buffer also asks for
                // real-time priority, a few system calls.
                move |samples: &[i16], info: &cpal::InputCallbackInfo| {
                    promotion.on_buffer(promote_current_thread);
                    // cpal stamps the buffer with PipeWire's capture time,
                    // on CLOCK_MONOTONIC.
                    let stamp = info.timestamp();
                    let monotonic = |instant: cpal::StreamInstant| {
                        u64::try_from(instant.as_nanos())
                            .ok()
                            .map(Duration::from_nanos)
                    };
                    let stamp = monotonic(stamp.callback)
                        .zip(monotonic(stamp.capture))
                        .map(|(cycle, capture)| Stamp { cycle, capture });
                    // Otherwise only a failure and the stream's end take
                    // the lock.
                    settling
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .buffer(samples, stamp, &mut send(&events));
                },
                move |error: cpal::Error| match stream_error(
                    &failed_source,
                    &error,
                    promoted.load(Ordering::SeqCst),
                ) {
                    Some(Ok(notice)) => errors.notice(notice),
                    Some(Err(failure)) => {
                        // The audio held was the track's before it failed.
                        release(&failing, &errors);
                        errors.failed(failure);
                    }
                    None => {}
                },
                None,
            )
            .map_err(|e| start_error(source, &e))?;
        stream.play().map_err(|e| start_error(source, &e))?;
        Ok(PipeWireStream {
            _stream: stream,
            _held: Held {
                settling: held,
                events: sender,
            },
            _watch: watch::watch(followed(source), watched),
        })
    }
}

/// Sends a buffer to `events`, stamped if it's timed.
fn send(events: &CaptureSender) -> impl FnMut(&[i16], Timed) + '_ {
    |samples, timed| match timed {
        Some((captured, delay)) => events.audio_captured(samples, captured, delay),
        None => events.audio(samples),
    }
}

/// Sends what `settling` holds to `events`, as the stream ends or fails.
fn release(settling: &Mutex<Settling>, events: &CaptureSender) {
    settling
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .release(&mut send(events));
}

/// `source` as the watch follows it: cpal's own names for the defaults,
/// given as a device, follow the default as cpal does, rather than name a
/// node the graph doesn't have.
fn followed(source: &Source) -> Source {
    match source {
        Source::Device(name) if name == "sink_default" => Source::SystemAudio,
        Source::Device(name) if name == "input_default" => Source::Microphone,
        other => other.clone(),
    }
}

/// Whether `source` follows a default rather than one device.
fn follows(source: &Source) -> bool {
    !matches!(followed(source), Source::Device(_))
}

/// What an error the running stream reports means: a notice, with
/// capture going on, a failure that ends it, or nothing to report. With
/// `promoted`, the thread is already real-time, so rtkit's refusal doesn't
/// matter.
///
/// A device that went away ends a pinned stream. A followed one's only
/// such report is cpal's when the session manager clears the default
/// (no device of the kind is left): the stream isn't ended, so it follows
/// the next default there is (cpal reports that as a change), and the
/// watch reports the loss once its grace has run out.
pub(super) fn stream_error(
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
        ErrorKind::DeviceNotAvailable if follows(source) => return None,
        ErrorKind::DeviceNotAvailable => Err(CaptureError::DeviceNotAvailable(source.clone())),
        _ => Err(CaptureError::Backend(error.to_string())),
    })
}

/// Why the stream couldn't be opened or started.
pub(super) fn start_error(source: &Source, error: &cpal::Error) -> CaptureError {
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

    /// A stream's held buffers are sent, once, as it's dropped.
    #[test]
    fn what_a_stream_holds_is_sent_as_it_ends() {
        use std::sync::atomic::AtomicU64;

        use nota_core::{FakeClock, SampleIndex, SessionTime, TrackId};

        use super::super::queue::Received;
        use super::super::{CaptureEvent, Progress, test_channel};

        const TRACK: TrackId = TrackId::new(0);
        let (tx, rx) = test_channel();
        let events = CaptureSender {
            events: tx,
            track: TRACK,
            progress: Progress::new(SampleIndex::ZERO, SampleIndex::ZERO),
            clock: Arc::new(FakeClock::new(SessionTime::ZERO)),
            stopping: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
            lost: Arc::new(AtomicBool::new(false)),
            // Begun already, so each buffer queues only its audio.
            began: Arc::new(AtomicBool::new(true)),
            reroutes: Arc::default(),
            asleep: Arc::new(AtomicU64::new(0)),
            rate: SampleRate::SPEECH,
        };
        let settling = Arc::new(Mutex::new(Settling::new(SampleRate::SPEECH)));
        // No delay yet: held.
        let ms = Duration::from_millis(100);
        let stamp = Some(Stamp {
            cycle: ms,
            capture: ms,
        });
        settling
            .lock()
            .unwrap()
            .buffer(&[1, 2], stamp, &mut send(&events));
        let next = || rx.next(Duration::from_millis(1));
        assert!(matches!(next(), Received::Idle), "held");
        drop(Held {
            settling: Arc::clone(&settling),
            events: events.clone(),
        });
        assert!(matches!(
            next(),
            Received::Event(TRACK, CaptureEvent::TimedAudio { samples, .. }) if samples == [1, 2]
        ));
        // Sent once.
        release(&settling, &events);
        assert!(matches!(next(), Received::Idle));
    }

    /// A server that answers lists its devices, and one that can't be
    /// reached says so: never an empty list for either. (CI has no
    /// server; a machine with one has devices.)
    #[test]
    fn the_server_lists_its_devices_or_is_unavailable() {
        match PipeWireBackend.devices() {
            Ok(devices) => assert!(
                !devices.outputs.is_empty() || !devices.inputs.is_empty(),
                "{devices:?}"
            ),
            Err(CaptureError::HostUnavailable(_)) => {}
            Err(other) => panic!("{other}"),
        }
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

    /// cpal's names for the defaults, given as a device, are followed as
    /// defaults; any other name stays pinned.
    #[test]
    fn cpals_default_names_are_followed() {
        let device = |name: &str| Source::Device(name.into());
        assert_eq!(followed(&device("sink_default")), Source::SystemAudio);
        assert_eq!(followed(&device("input_default")), Source::Microphone);
        assert_eq!(
            followed(&device("alsa_input.usb")),
            device("alsa_input.usb")
        );
        assert_eq!(followed(&Source::Microphone), Source::Microphone);
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
        let usb = Source::Device("alsa_input.usb".into());
        for promoted in [false, true] {
            assert_eq!(
                stream_error(&usb, &error(ErrorKind::DeviceNotAvailable), promoted),
                Some(Err(CaptureError::DeviceNotAvailable(usb.clone())))
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

    /// A followed default cleared by the session manager doesn't end the
    /// stream, which follows the next default; cpal's own default names
    /// count as followed.
    #[test]
    fn a_followed_default_removed_does_not_end_the_stream() {
        for source in [
            Source::Microphone,
            Source::SystemAudio,
            Source::Device("input_default".into()),
        ] {
            assert_eq!(
                stream_error(&source, &error(ErrorKind::DeviceNotAvailable), false),
                None,
                "{source:?}"
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
