//! The screen's loop, on threads and channels: keys and the recorder's
//! events in, commands to the recorder out.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nota_core::recorder::{self, Command};
use nota_core::{Clock, SessionTime};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::crossterm::event::{self, KeyEvent};

use crate::screen::Recording;

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
    /// What the recorder reports.
    Recorder(recorder::Event),
    /// The terminal changed size.
    Resize,
    /// Reading the terminal failed and the input thread has stopped: no more
    /// keys will come.
    InputLost(io::ErrorKind),
}

/// Why the screen closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The stop was confirmed on the screen: `s` or Ctrl+C, then `y`.
    Stopped,
    /// The recorder closed it, by [`recorder::Event::Stopping`] or
    /// [`recorder::Event::Stopped`], or by dropping every sender of its
    /// events.
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
    /// A command was given but nothing receives commands any more: a mark
    /// or note couldn't be stored. It's handed back rather than lost.
    CommandsClosed(Command),
}

impl<E: std::fmt::Display> std::fmt::Display for RunError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Terminal(err) => write!(f, "drawing the screen failed: {err}"),
            Self::InputLost(kind) => write!(f, "reading keys from the terminal failed: {kind}"),
            Self::CommandsClosed(command) => {
                let what = match command {
                    Command::Start(_) => "a start",
                    Command::Mark(_) => "a mark",
                    Command::Note(_) => "a note",
                    Command::Stop => "a stop",
                };
                write!(f, "{what} couldn't be given: nothing receives commands")
            }
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for RunError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Terminal(err) => Some(err),
            Self::InputLost(_) | Self::CommandsClosed(_) => None,
        }
    }
}

/// Runs `screen` on `terminal` until it ends: draws it, applies each event,
/// and sends each command the screen gives to `commands`: each new mark and
/// note to be stored, and the stop. Redraws at least every 250 ms, applying
/// at most 1 000 events in between. A note still being typed when the loop
/// ends is saved, unless it's blank.
///
/// It ends with [`Ended::Stopped`] as soon as a stop is confirmed on the
/// screen, once [`Command::Stop`] is sent; events after that key are left
/// unapplied. It ends with [`Ended::Closed`] when the recorder says it's
/// stopping or has stopped, or once every sender of `events` is gone.
/// Either way the caller then stops its [`InputThread`].
///
/// # Errors
///
/// - [`RunError::Terminal`] if drawing fails.
/// - [`RunError::InputLost`] if the input thread reports that reading the
///   terminal failed.
/// - [`RunError::CommandsClosed`] if a command can't be sent on.
pub fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    screen: &mut Recording,
    events: &Receiver<Event>,
    commands: &Sender<Command>,
) -> Result<Ended, RunError<B::Error>> {
    let send = |command| {
        commands
            .send(command)
            .map_err(|err| RunError::CommandsClosed(err.0))
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
            let was_asking = screen.is_confirming_stop();
            let given = match event {
                Event::Key { key, at } => screen.handle_key_at(key, at),
                Event::Recorder(recorder::Event::Stopping | recorder::Event::Stopped(_)) => {
                    save_draft(screen)?;
                    return Ok(Ended::Closed);
                }
                Event::Recorder(event) => {
                    screen.update(event);
                    None
                }
                Event::Resize => None,
                Event::InputLost(kind) => {
                    save_draft(screen)?;
                    return Err(RunError::InputLost(kind));
                }
            };
            if screen.stop_confirmed() {
                // A note half typed goes before the stop: nothing after the
                // stop is stored.
                save_draft(screen)?;
            }
            if let Some(command) = given {
                send(command)?;
            }
            if screen.stop_confirmed() {
                return Ok(Ended::Stopped);
            }
            // Draw the question before applying another key, so a `y`
            // queued behind it can't stop the recording unseen.
            if !was_asking && screen.is_confirming_stop() {
                break;
            }
        }
    }
}

/// A thread that reads keys and resizes from the terminal and sends them as
/// [`Event`]s, each key stamped with the session clock as it's read. Keys
/// already waiting when it starts are discarded: they were typed before the
/// screen was there to take them (during a slow start-up, say), and a
/// Ctrl+C then `y` typed because nota looked stuck mustn't stop the new
/// recording. It stops when told to or dropped (waiting up to a second for
/// the thread either way), or when nothing receives its events. If reading fails it
/// sends [`Event::InputLost`] and stops.
#[derive(Debug)]
pub struct InputThread {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<io::Result<()>>>,
    /// Disconnected once the thread has finished.
    finished: Receiver<()>,
}

impl InputThread {
    /// Starts reading the terminal, once the keys already waiting are
    /// discarded (it waits up to a second for that). The terminal should
    /// already be in raw mode.
    ///
    /// # Errors
    ///
    /// The thread couldn't be started.
    pub fn spawn(events: Sender<Event>, clock: Arc<dyn Clock>) -> io::Result<Self> {
        Self::spawn_with(Crossterm, events, clock)
    }

    fn spawn_with(
        mut input: impl Input + Send + 'static,
        events: Sender<Event>,
        clock: Arc<dyn Clock>,
    ) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (finishing, finished) = mpsc::channel::<()>();
        let (discarded, ready) = mpsc::channel::<()>();
        let handle = thread::Builder::new()
            .name("nota-tui-input".into())
            .spawn({
                let stop = Arc::clone(&stop);
                move || {
                    let _finishing = finishing;
                    let read = input.discard_waiting().and_then(|()| {
                        drop(discarded);
                        read_input(&mut input, &events, clock.as_ref(), &stop)
                    });
                    if let Err(err) = &read {
                        let _ = events.send(Event::InputLost(err.kind()));
                    }
                    read
                }
            })?;
        // Before the screen is drawn: a key typed at it mustn't be taken
        // for one typed ahead. Either way, the thread is done discarding.
        let _ = ready.recv_timeout(STOP_WAIT);
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

/// Where the input thread reads terminal events: the terminal itself, or a
/// test's script.
trait Input {
    /// Whether an event can be read within `timeout`.
    fn poll(&mut self, timeout: Duration) -> io::Result<bool>;
    /// The next event, waiting for it if need be.
    fn read(&mut self) -> io::Result<event::Event>;

    /// Discards every event that can be read now.
    fn discard_waiting(&mut self) -> io::Result<()> {
        while self.poll(Duration::ZERO)? {
            self.read()?;
        }
        Ok(())
    }
}

/// The terminal, through crossterm.
struct Crossterm;

impl Input for Crossterm {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        event::poll(timeout)
    }

    fn read(&mut self) -> io::Result<event::Event> {
        event::read()
    }

    /// Flushes the terminal's input queue too: crossterm reads it 1 KiB at
    /// a time and is woken only by new input, so a longer queue would
    /// otherwise outlast the discard and arrive with the next key.
    fn discard_waiting(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        flush_terminal_input();
        while self.poll(Duration::ZERO)? {
            self.read()?;
        }
        Ok(())
    }
}

/// Discards what's waiting in the input queue of the terminal crossterm
/// reads: stdin if it's a terminal, else the controlling terminal. Only a
/// help to the discard, so a failure is ignored.
#[cfg(unix)]
fn flush_terminal_input() {
    use rustix::termios::{QueueSelector, tcflush};
    use std::io::IsTerminal;
    let stdin = io::stdin();
    if stdin.is_terminal() {
        let _ = tcflush(&stdin, QueueSelector::IFlush);
    } else if let Ok(tty) = std::fs::File::open("/dev/tty") {
        let _ = tcflush(&tty, QueueSelector::IFlush);
    }
}

fn read_input(
    input: &mut impl Input,
    events: &Sender<Event>,
    clock: &dyn Clock,
    stop: &AtomicBool,
) -> io::Result<()> {
    while !stop.load(Ordering::Relaxed) {
        if !input.poll(INPUT_POLL)? {
            continue;
        }
        let event = match input.read()? {
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
    use std::collections::VecDeque;
    use std::sync::{Mutex, mpsc};

    use nota_core::FakeClock;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    use nota_core::recorder::{Mark, Note};

    use super::*;
    use crate::text::heard;
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
        for event in [
            Event::Recorder(recorder::Event::Text(heard(0, 1, "loop text"))),
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
                Command::Mark(Mark { at }),
                Command::Note(Note::new(at, "hi").unwrap())
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
        assert_eq!(sent, [Command::Note(Note::new(at, "ok").unwrap())]);
    }

    #[test]
    fn a_mark_nobody_receives_is_handed_back() {
        let clock = Arc::new(FakeClock::new(SessionTime::from_nanos(3)));
        let mut screen = screen(&clock);
        let mut terminal = Terminal::new(TestBackend::new(62, 20)).unwrap();
        let (event_tx, event_rx) = mpsc::channel();
        let (note_tx, note_rx) = mpsc::channel::<Command>();
        drop(note_rx);
        event_tx.send(key('m')).unwrap();
        let err = run(&mut terminal, &mut screen, &event_rx, &note_tx).unwrap_err();
        let RunError::CommandsClosed(command) = &err else {
            panic!("{err}");
        };
        assert_eq!(
            *command,
            Command::Mark(Mark {
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
        assert_eq!(sent, [Command::Note(Note::new(at, "a").unwrap())]);
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
        // Half a second after the question opened, so it's an answer.
        let y = key_at(KeyCode::Char('y'), 9 + 500_000_000);
        // Nothing after the `y` is applied: no mark, no second note.
        for event in [key('n'), key('o'), ctrl_c, y, key('m'), key('n')] {
            event_tx.send(event).unwrap();
        }
        // The recorder's sender is still alive: the key alone ends it.
        let ended = run(&mut terminal, &mut screen, &event_rx, &note_tx).unwrap();
        assert_eq!(ended, Ended::Stopped);
        let sent: Vec<_> = note_rx.try_iter().collect();
        let at = SessionTime::from_nanos(7);
        // The note goes first: nothing after the stop is stored.
        assert_eq!(
            sent,
            [Command::Note(Note::new(at, "o").unwrap()), Command::Stop]
        );
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
        let y = key_at(KeyCode::Char('y'), 7 + 500_000_000);
        for event in [key('s'), y, key('m')] {
            event_tx.send(event).unwrap();
        }
        let ended = run(&mut terminal, &mut screen, &event_rx, &note_tx).unwrap();
        assert_eq!(ended, Ended::Stopped);
        assert_eq!(note_rx.try_iter().collect::<Vec<_>>(), [Command::Stop]);
        drop(event_tx);
    }

    #[test]
    fn the_recorder_stopping_ends_the_loop_and_keeps_the_draft() {
        for stopping in [
            recorder::Event::Stopping,
            recorder::Event::Stopped(recorder::Outcome::default()),
        ] {
            let clock = Arc::new(FakeClock::new(SessionTime::from_nanos(20)));
            let mut screen = screen(&clock);
            let mut terminal = Terminal::new(TestBackend::new(62, 20)).unwrap();
            let (event_tx, event_rx) = mpsc::channel();
            let (note_tx, note_rx) = mpsc::channel();
            for event in [key('n'), key('k'), Event::Recorder(stopping), key('m')] {
                event_tx.send(event).unwrap();
            }
            let ended = run(&mut terminal, &mut screen, &event_rx, &note_tx).unwrap();
            assert_eq!(ended, Ended::Closed);
            let sent: Vec<_> = note_rx.try_iter().collect();
            let at = SessionTime::from_nanos(7);
            assert_eq!(sent, [Command::Note(Note::new(at, "k").unwrap())]);
            drop(event_tx);
        }
    }

    /// Every event the recorder sends reaches the screen through the loop.
    /// Those it shows change what's drawn; the rest, not shown yet, leave
    /// the screen open and taking keys.
    #[test]
    fn every_recorder_event_reaches_the_screen() {
        use nota_core::recorder::{
            Cause, DeviceChange, Disk, EngineState, Level, Warning, WarningState,
        };
        use nota_core::{EpochId, SampleIndex, SampleRate, TrackId, TrackTimeline};

        let track = TrackId::new(1);
        let mut timeline = TrackTimeline::new(track);
        let secs = |s: u64| SessionTime::from_nanos(s * 1_000_000_000);
        timeline
            .open_epoch(SessionTime::ZERO, SampleIndex::ZERO, SampleRate::SPEECH)
            .unwrap();
        timeline
            .open_epoch(secs(3), SampleIndex::new(16_000), SampleRate::SPEECH)
            .unwrap();
        let epoch = *timeline.epochs().last().unwrap();
        assert_eq!(epoch.id(), EpochId::new(1));
        let gap = timeline.gaps().next().unwrap();
        let unshown = [
            recorder::Event::Engine(EngineState::Offline("exited".into())),
            recorder::Event::Engine(EngineState::Online),
            recorder::Event::Warning(Warning {
                cause: Cause::Stalled,
                track: Some(track),
                at: secs(1),
                state: WarningState::Raised,
            }),
            recorder::Event::Device {
                track,
                change: DeviceChange::Lost,
                at: secs(1),
            },
            recorder::Event::Disk(Disk {
                free_bytes: 1 << 30,
                left: None,
            }),
            recorder::Event::Durable {
                track,
                up_to: secs(1),
            },
            recorder::Event::Epoch { track, epoch },
            recorder::Event::Gap { track, gap },
        ];
        let shown = [
            recorder::Event::Level {
                track,
                at: secs(1),
                level: Level::FULL_SCALE,
            },
            recorder::Event::Text(heard(0, 1, "heard it")),
            recorder::Event::Transcribing(true),
            recorder::Event::Recorded(42),
        ];
        let clock = Arc::new(FakeClock::new(secs(4)));
        let mut screen = screen(&clock);
        let mut terminal = Terminal::new(TestBackend::new(62, 20)).unwrap();
        let (event_tx, event_rx) = mpsc::channel();
        let (command_tx, command_rx) = mpsc::channel();
        for event in unshown.into_iter().chain(shown) {
            event_tx.send(Event::Recorder(event)).unwrap();
        }
        event_tx.send(key('m')).unwrap();
        drop(event_tx);
        let ended = run(&mut terminal, &mut screen, &event_rx, &command_tx).unwrap();
        assert_eq!(ended, Ended::Closed);
        // Still taking keys after every event that isn't shown.
        let at = SessionTime::from_nanos(7);
        assert_eq!(
            command_rx.try_iter().collect::<Vec<_>>(),
            [Command::Mark(Mark { at })]
        );
        assert_eq!(screen.utterances.len(), 1);
        assert!(screen.transcribing);
        assert_eq!(screen.recorded_bytes, 42);
        let levels: Vec<_> = screen
            .levels
            .columns(secs(4), 17)
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(levels, [Level::FULL_SCALE]);
    }

    /// Terminal events in two lots: those already waiting when the input
    /// thread starts, then those the test sends while it runs.
    struct Script {
        waiting: Arc<Mutex<VecDeque<event::Event>>>,
        later: Receiver<event::Event>,
    }

    impl Input for Script {
        fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
            if !self.waiting.lock().unwrap().is_empty() {
                return Ok(true);
            }
            if timeout.is_zero() {
                return Ok(false);
            }
            // Not holding the lock while waiting, or the test could wait
            // for it indefinitely.
            match self.later.recv_timeout(timeout) {
                Ok(event) => {
                    self.waiting.lock().unwrap().push_back(event);
                    Ok(true)
                }
                Err(_) => Ok(false),
            }
        }

        fn read(&mut self) -> io::Result<event::Event> {
            self.waiting
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| io::Error::other("nothing to read"))
        }
    }

    fn terminal_key(code: KeyCode, modifiers: KeyModifiers) -> event::Event {
        event::Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn keys_waiting_before_the_screen_starts_are_discarded() {
        // Ctrl+C then `y`, typed during a slow start-up.
        let waiting = Arc::new(Mutex::new(VecDeque::from([
            terminal_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            terminal_key(KeyCode::Char('y'), KeyModifiers::NONE),
            event::Event::Resize(80, 24),
        ])));
        let (later_tx, later) = mpsc::channel();
        let script = Script {
            waiting: Arc::clone(&waiting),
            later,
        };
        let clock = Arc::new(FakeClock::new(SessionTime::from_nanos(42)));
        let (event_tx, event_rx) = mpsc::channel();
        let input = InputThread::spawn_with(script, event_tx, clock).unwrap();
        // Discarded before `spawn` returns, so before the screen is drawn.
        assert!(waiting.lock().unwrap().is_empty());
        for event in [
            terminal_key(KeyCode::Char('m'), KeyModifiers::NONE),
            event::Event::FocusGained,
            event::Event::Resize(70, 24),
        ] {
            later_tx.send(event).unwrap();
        }
        let wait = Duration::from_secs(5);
        let sent = [
            event_rx.recv_timeout(wait).unwrap(),
            event_rx.recv_timeout(wait).unwrap(),
        ];
        assert_eq!(
            sent,
            [
                Event::Key {
                    key: KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
                    at: SessionTime::from_nanos(42),
                },
                Event::Resize,
            ]
        );
        input.stop().unwrap();
        assert_eq!(event_rx.try_iter().count(), 0);
    }

    #[test]
    fn the_question_is_drawn_before_a_queued_y_answers_it() {
        let clock = Arc::new(FakeClock::new(SessionTime::from_nanos(20)));
        let mut screen = screen(&clock);
        let mut terminal = Terminal::new(TestBackend::new(62, 20)).unwrap();
        let (event_tx, event_rx) = mpsc::channel();
        let (note_tx, _note_rx) = mpsc::channel();
        // Both waiting at once, as after a draw that held the loop up.
        let y = key_at(KeyCode::Char('y'), 7 + 600_000_000);
        for event in [key('s'), y] {
            event_tx.send(event).unwrap();
        }
        let ended = run(&mut terminal, &mut screen, &event_rx, &note_tx).unwrap();
        assert_eq!(ended, Ended::Stopped);
        let screen_text = format!("{}", terminal.backend());
        assert!(screen_text.contains("stop recording?"), "{screen_text}");
        drop(event_tx);
    }
}
