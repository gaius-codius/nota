//! The screen's loop, on threads and channels: events in, marks and notes
//! out.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nota_core::{Clock, SessionTime};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::crossterm::event::{self, KeyEvent};

use crate::annotation::Annotation;
use crate::screen::{Recording, Update};

/// How often the screen redraws with nothing new, so the elapsed time and
/// the REC dot keep moving.
const REDRAW: Duration = Duration::from_millis(250);

/// The most events applied between two draws, so a flood of updates can't
/// hold the screen still.
const MAX_BATCH: usize = 1_000;

/// How long the input thread waits for a key before checking whether it
/// should stop.
const INPUT_POLL: Duration = Duration::from_millis(100);

/// How long stopping the input thread waits for it. It checks every
/// [`INPUT_POLL`], but once the terminal has hung up crossterm's read never
/// returns (it retries the read that keeps failing). The thread is then left
/// behind rather than holding up the stop: the terminal it reads is gone, so
/// it can't take keys meant for anything else.
const STOP_WAIT: Duration = Duration::from_secs(1);

/// Something for the screen to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A key from the terminal.
    Key {
        /// The key.
        key: KeyEvent,
        /// When it was read, so a mark lands at the moment of the key press
        /// however long the event waits in the channel.
        at: SessionTime,
    },
    /// News from the rest of nota.
    Update(Update),
    /// The terminal changed size.
    Resize,
    /// Reading the terminal failed and the input thread has stopped: no more
    /// keys will come.
    InputLost(io::ErrorKind),
    /// The rest of nota is closing the screen (the recording is stopping,
    /// say on a signal).
    Close,
}

/// Why the screen closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The stop was confirmed on the screen: `s` or Ctrl+C, then `y`.
    Stopped,
    /// The rest of nota closed it, by [`Event::Close`] or by dropping every
    /// sender of its events.
    Closed,
}

/// Why [`run`] stopped early.
#[derive(Debug)]
pub enum RunError<E> {
    /// Drawing to the terminal failed.
    Terminal(E),
    /// Reading keys from the terminal failed, so marks and notes can't be
    /// added any more.
    InputLost(io::ErrorKind),
    /// A mark or note was made but nothing is receiving them any more, so it
    /// couldn't be stored. It's handed back rather than lost.
    AnnotationsClosed(Annotation),
}

impl<E: std::fmt::Display> std::fmt::Display for RunError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Terminal(err) => write!(f, "drawing the screen failed: {err}"),
            Self::InputLost(kind) => write!(f, "reading keys from the terminal failed: {kind}"),
            Self::AnnotationsClosed(annotation) => write!(
                f,
                "a {} at {:?} couldn't be stored: nothing receives marks and notes",
                match annotation {
                    Annotation::Mark(_) => "mark",
                    Annotation::Note(_) => "note",
                },
                annotation.at().elapsed()
            ),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for RunError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Terminal(err) => Some(err),
            Self::InputLost(_) | Self::AnnotationsClosed(_) => None,
        }
    }
}

/// Runs `screen` on `terminal` until it ends: draws it, applies each event,
/// and sends each new mark and note to `annotations` to be stored. Redraws
/// at least every 250 ms, applying at most 1 000 events in between. A note
/// still being typed when the loop ends is saved, unless it's blank.
///
/// It ends with [`Ended::Stopped`] as soon as a stop is confirmed on the
/// screen; events after that key are left unapplied. It ends with
/// [`Ended::Closed`] on [`Event::Close`], or once every sender of `events`
/// is gone. Either way the caller then stops its [`InputThread`].
///
/// # Errors
///
/// - [`RunError::Terminal`] if drawing fails.
/// - [`RunError::InputLost`] if the input thread reports that reading the
///   terminal failed.
/// - [`RunError::AnnotationsClosed`] if a mark or note can't be sent on.
pub fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    screen: &mut Recording,
    events: &Receiver<Event>,
    annotations: &Sender<Annotation>,
) -> Result<Ended, RunError<B::Error>> {
    let send = |annotation| {
        annotations
            .send(annotation)
            .map_err(|err| RunError::AnnotationsClosed(err.0))
    };
    // A note half typed is kept: it's already pinned to a moment.
    let save_draft = |screen: &mut Recording| match screen.save_draft() {
        Some(note) => send(note),
        None => Ok(()),
    };
    loop {
        terminal
            .draw(|frame| screen.draw(frame))
            .map_err(RunError::Terminal)?;
        let first = match events.recv_timeout(REDRAW) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                save_draft(screen)?;
                return Ok(Ended::Closed);
            }
        };
        // Apply what's already waiting before drawing again, so a burst of
        // levels costs one draw.
        let waiting = events.try_iter().take(MAX_BATCH - 1);
        for event in std::iter::once(first).chain(waiting) {
            let added = match event {
                Event::Key { key, at } => screen.handle_key_at(key, at),
                Event::Update(update) => {
                    screen.update(update);
                    None
                }
                Event::Resize => None,
                Event::InputLost(kind) => {
                    save_draft(screen)?;
                    return Err(RunError::InputLost(kind));
                }
                Event::Close => {
                    save_draft(screen)?;
                    return Ok(Ended::Closed);
                }
            };
            if let Some(annotation) = added {
                send(annotation)?;
            }
            if screen.stop_confirmed() {
                save_draft(screen)?;
                return Ok(Ended::Stopped);
            }
        }
    }
}

/// A thread that reads keys and resizes from the terminal and sends them as
/// [`Event`]s, each key stamped with the session clock as it's read. It
/// stops when told to or dropped (waiting up to a second for the thread
/// either way), or when nothing receives its events. If reading fails it
/// sends [`Event::InputLost`] and stops.
#[derive(Debug)]
pub struct InputThread {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<io::Result<()>>>,
    /// Disconnected once the thread has finished.
    finished: Receiver<()>,
}

impl InputThread {
    /// Starts reading the terminal. The terminal should already be in raw
    /// mode.
    ///
    /// # Errors
    ///
    /// The thread couldn't be started.
    pub fn spawn(events: Sender<Event>, clock: Arc<dyn Clock>) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (finishing, finished) = mpsc::channel::<()>();
        let handle = thread::Builder::new()
            .name("nota-tui-input".into())
            .spawn({
                let stop = Arc::clone(&stop);
                move || {
                    let _finishing = finishing;
                    let read = read_input(&events, clock.as_ref(), &stop);
                    if let Err(err) = &read {
                        let _ = events.send(Event::InputLost(err.kind()));
                    }
                    read
                }
            })?;
        Ok(Self {
            stop,
            handle: Some(handle),
            finished,
        })
    }

    /// Stops reading and waits for the thread: about 100 ms while the
    /// terminal is there, at most a second once it has hung up.
    ///
    /// # Errors
    ///
    /// Reading the terminal failed, the thread panicked, or it didn't stop
    /// within a second ([`io::ErrorKind::TimedOut`]) and was left behind.
    pub fn stop(mut self) -> io::Result<()> {
        self.finish()
    }

    fn finish(&mut self) -> io::Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        match self.finished.recv_timeout(STOP_WAIT) {
            Err(RecvTimeoutError::Disconnected) => handle
                .join()
                .map_err(|_| io::Error::other("the input thread panicked"))?,
            Ok(()) | Err(RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the input thread didn't stop; the terminal has probably hung up",
            )),
        }
    }
}

impl Drop for InputThread {
    /// Stops the thread and waits for it as [`InputThread::stop`] does, so a
    /// dropped handle never leaves a second reader on a working terminal.
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn read_input(events: &Sender<Event>, clock: &dyn Clock, stop: &AtomicBool) -> io::Result<()> {
    while !stop.load(Ordering::Relaxed) {
        if !event::poll(INPUT_POLL)? {
            continue;
        }
        let event = match event::read()? {
            event::Event::Key(key) => Event::Key {
                key,
                at: clock.now(),
            },
            event::Event::Resize(..) => Event::Resize,
            _ => continue,
        };
        if events.send(event).is_err() {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use nota_core::FakeClock;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    use super::*;
    use crate::annotation::{Mark, Note};
    use crate::text::Utterance;
    use crate::theme::Theme;

    /// A key read at the session time `at` nanoseconds.
    fn key_at(code: KeyCode, at: u64) -> Event {
        Event::Key {
            key: KeyEvent::new(code, KeyModifiers::NONE),
            at: SessionTime::from_nanos(at),
        }
    }

    fn key(c: char) -> Event {
        key_at(KeyCode::Char(c), 7)
    }

    fn screen(clock: &Arc<FakeClock>) -> Recording {
        Recording::new(
            "Loop".into(),
            "Mic".into(),
            Arc::clone(clock) as Arc<dyn Clock>,
            Theme::no_color(),
        )
    }

    #[test]
    fn runs_until_the_events_end_and_sends_marks_and_notes_on() {
        let clock = Arc::new(FakeClock::new(SessionTime::from_nanos(7)));
        let mut screen = screen(&clock);
        let mut terminal = Terminal::new(TestBackend::new(62, 20)).unwrap();
        let (event_tx, event_rx) = mpsc::channel();
        let (note_tx, note_rx) = mpsc::channel();
        let text = Utterance::new(
            SessionTime::from_nanos(1),
            SessionTime::from_nanos(5),
            "loop text".into(),
        )
        .unwrap();
        for event in [
            Event::Update(Update::Text(text)),
            key('m'),
            key('n'),
            key('h'),
            key('i'),
            key_at(KeyCode::Enter, 7),
            Event::Resize,
        ] {
            event_tx.send(event).unwrap();
        }
        drop(event_tx);
        let ended = run(&mut terminal, &mut screen, &event_rx, &note_tx).unwrap();
        assert_eq!(ended, Ended::Closed);
        let at = SessionTime::from_nanos(7);
        let sent: Vec<_> = note_rx.try_iter().collect();
        assert_eq!(
            sent,
            [
                Annotation::Mark(Mark { at }),
                Annotation::Note(Note::new(at, "hi").unwrap())
            ]
        );
        // The last draw shows the text with its mark.
        let screen_text = format!("{}", terminal.backend());
        assert!(screen_text.contains("│ ◆ loop text"), "{screen_text}");
    }

    #[test]
    fn a_half_typed_note_is_saved_when_the_events_end() {
        let clock = Arc::new(FakeClock::new(SessionTime::from_nanos(11)));
        let mut screen = screen(&clock);
        let mut terminal = Terminal::new(TestBackend::new(62, 20)).unwrap();
        let (event_tx, event_rx) = mpsc::channel();
        let (note_tx, note_rx) = mpsc::channel();
        for event in [key('n'), key('o'), key('k')] {
            event_tx.send(event).unwrap();
        }
        drop(event_tx);
        let ended = run(&mut terminal, &mut screen, &event_rx, &note_tx).unwrap();
        assert_eq!(ended, Ended::Closed);
        let sent: Vec<_> = note_rx.try_iter().collect();
        // Pinned to when `n` was read (7 ns), not the clock's 11 ns.
        let at = SessionTime::from_nanos(7);
        assert_eq!(sent, [Annotation::Note(Note::new(at, "ok").unwrap())]);
    }

    #[test]
    fn a_mark_nobody_receives_is_handed_back() {
        let clock = Arc::new(FakeClock::new(SessionTime::from_nanos(3)));
        let mut screen = screen(&clock);
        let mut terminal = Terminal::new(TestBackend::new(62, 20)).unwrap();
        let (event_tx, event_rx) = mpsc::channel();
        let (note_tx, note_rx) = mpsc::channel::<Annotation>();
        drop(note_rx);
        event_tx.send(key('m')).unwrap();
        let err = run(&mut terminal, &mut screen, &event_rx, &note_tx).unwrap_err();
        let RunError::AnnotationsClosed(annotation) = &err else {
            panic!("{err}");
        };
        assert_eq!(
            *annotation,
            Annotation::Mark(Mark {
                at: SessionTime::from_nanos(7)
            })
        );
        assert!(err.to_string().contains("mark"));
    }

    #[test]
    fn lost_input_ends_the_loop_and_keeps_the_draft() {
        let clock = Arc::new(FakeClock::new(SessionTime::from_nanos(50)));
        let mut screen = screen(&clock);
        let mut terminal = Terminal::new(TestBackend::new(62, 20)).unwrap();
        let (event_tx, event_rx) = mpsc::channel();
        let (note_tx, note_rx) = mpsc::channel();
        for event in [
            key('n'),
            key('a'),
            Event::InputLost(io::ErrorKind::BrokenPipe),
        ] {
            event_tx.send(event).unwrap();
        }
        // Another sender (the recorder's) is still alive.
        let err = run(&mut terminal, &mut screen, &event_rx, &note_tx).unwrap_err();
        assert!(
            matches!(err, RunError::InputLost(io::ErrorKind::BrokenPipe)),
            "{err}"
        );
        assert!(err.to_string().contains("keys"));
        let sent: Vec<_> = note_rx.try_iter().collect();
        let at = SessionTime::from_nanos(7);
        assert_eq!(sent, [Annotation::Note(Note::new(at, "a").unwrap())]);
        drop(event_tx);
    }

    #[test]
    fn a_confirmed_stop_ends_the_loop_and_keeps_the_draft() {
        let clock = Arc::new(FakeClock::new(SessionTime::from_nanos(20)));
        let mut screen = screen(&clock);
        let mut terminal = Terminal::new(TestBackend::new(62, 20)).unwrap();
        let (event_tx, event_rx) = mpsc::channel();
        let (note_tx, note_rx) = mpsc::channel();
        let ctrl_c = Event::Key {
            key: KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            at: SessionTime::from_nanos(9),
        };
        // Nothing after the `y` is applied: no mark, no second note.
        for event in [key('n'), key('o'), ctrl_c, key('y'), key('m'), key('n')] {
            event_tx.send(event).unwrap();
        }
        // The recorder's sender is still alive: the key alone ends it.
        let ended = run(&mut terminal, &mut screen, &event_rx, &note_tx).unwrap();
        assert_eq!(ended, Ended::Stopped);
        let sent: Vec<_> = note_rx.try_iter().collect();
        let at = SessionTime::from_nanos(7);
        assert_eq!(sent, [Annotation::Note(Note::new(at, "o").unwrap())]);
        assert_eq!(event_rx.try_iter().count(), 2);
        drop(event_tx);
    }

    #[test]
    fn s_then_y_stops_without_a_draft() {
        let clock = Arc::new(FakeClock::new(SessionTime::from_nanos(20)));
        let mut screen = screen(&clock);
        let mut terminal = Terminal::new(TestBackend::new(62, 20)).unwrap();
        let (event_tx, event_rx) = mpsc::channel();
        let (note_tx, note_rx) = mpsc::channel();
        for event in [key('s'), key('y'), key('m')] {
            event_tx.send(event).unwrap();
        }
        let ended = run(&mut terminal, &mut screen, &event_rx, &note_tx).unwrap();
        assert_eq!(ended, Ended::Stopped);
        assert_eq!(note_rx.try_iter().count(), 0);
        drop(event_tx);
    }

    #[test]
    fn close_ends_the_loop_and_keeps_the_draft() {
        let clock = Arc::new(FakeClock::new(SessionTime::from_nanos(20)));
        let mut screen = screen(&clock);
        let mut terminal = Terminal::new(TestBackend::new(62, 20)).unwrap();
        let (event_tx, event_rx) = mpsc::channel();
        let (note_tx, note_rx) = mpsc::channel();
        for event in [key('n'), key('k'), Event::Close, key('m')] {
            event_tx.send(event).unwrap();
        }
        let ended = run(&mut terminal, &mut screen, &event_rx, &note_tx).unwrap();
        assert_eq!(ended, Ended::Closed);
        let sent: Vec<_> = note_rx.try_iter().collect();
        let at = SessionTime::from_nanos(7);
        assert_eq!(sent, [Annotation::Note(Note::new(at, "k").unwrap())]);
        drop(event_tx);
    }
}
