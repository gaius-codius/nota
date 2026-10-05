//! The screen's loop, on threads and channels: events in, marks and notes
//! out.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::crossterm::event::{self, KeyEvent};

use crate::annotation::Annotation;
use crate::screen::{Recording, Update};

/// How often the screen redraws with nothing new, so the elapsed time and
/// the REC dot keep moving.
const REDRAW: Duration = Duration::from_millis(250);

/// How long the input thread waits for a key before checking whether it
/// should stop.
const INPUT_POLL: Duration = Duration::from_millis(100);

/// Something for the screen to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A key from the terminal.
    Key(KeyEvent),
    /// News from the rest of nota.
    Update(Update),
    /// The terminal changed size.
    Resize,
}

/// Why [`run`] stopped early.
#[derive(Debug)]
pub enum RunError<E> {
    /// Drawing to the terminal failed.
    Terminal(E),
    /// A mark or note was made but nothing is receiving them any more, so it
    /// couldn't be stored. It's handed back rather than lost.
    AnnotationsClosed(Annotation),
}

impl<E: std::fmt::Display> std::fmt::Display for RunError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Terminal(err) => write!(f, "drawing the screen failed: {err}"),
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
            Self::AnnotationsClosed(_) => None,
        }
    }
}

/// Runs `screen` on `terminal` until every sender of `events` is gone:
/// draws it, applies each event, and sends each new mark and note to
/// `annotations` to be stored. Redraws at least every 250 ms.
///
/// # Errors
///
/// [`RunError::Terminal`] if drawing fails, and
/// [`RunError::AnnotationsClosed`] if a mark or note can't be sent on.
pub fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    screen: &mut Recording,
    events: &Receiver<Event>,
    annotations: &Sender<Annotation>,
) -> Result<(), RunError<B::Error>> {
    loop {
        terminal
            .draw(|frame| screen.draw(frame))
            .map_err(RunError::Terminal)?;
        let first = match events.recv_timeout(REDRAW) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };
        // Apply everything already waiting before drawing again, so a burst
        // of levels costs one draw.
        for event in std::iter::once(first).chain(events.try_iter()) {
            let added = match event {
                Event::Key(key) => screen.handle_key(key),
                Event::Update(update) => {
                    screen.update(update);
                    None
                }
                Event::Resize => None,
            };
            if let Some(annotation) = added {
                annotations
                    .send(annotation)
                    .map_err(|err| RunError::AnnotationsClosed(err.0))?;
            }
        }
    }
}

/// A thread that reads keys and resizes from the terminal and sends them as
/// [`Event`]s. It stops when told to, or when nothing receives its events.
#[derive(Debug)]
pub struct InputThread {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<io::Result<()>>,
}

impl InputThread {
    /// Starts reading the terminal. The terminal should already be in raw
    /// mode.
    ///
    /// # Errors
    ///
    /// The thread couldn't be started.
    pub fn spawn(events: Sender<Event>) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = thread::Builder::new()
            .name("nota-tui-input".into())
            .spawn({
                let stop = Arc::clone(&stop);
                move || read_input(&events, &stop)
            })?;
        Ok(Self { stop, handle })
    }

    /// Stops reading and waits for the thread, within about 100 ms.
    ///
    /// # Errors
    ///
    /// Reading the terminal failed, or the thread panicked.
    pub fn stop(self) -> io::Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle
            .join()
            .map_err(|_| io::Error::other("the input thread panicked"))?
    }
}

fn read_input(events: &Sender<Event>, stop: &AtomicBool) -> io::Result<()> {
    while !stop.load(Ordering::Relaxed) {
        if !event::poll(INPUT_POLL)? {
            continue;
        }
        let event = match event::read()? {
            event::Event::Key(key) => Event::Key(key),
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

    use nota_core::{Clock, FakeClock, SessionTime};
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    use super::*;
    use crate::annotation::{Mark, Note};
    use crate::text::Utterance;
    use crate::theme::Theme;

    fn key(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
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
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Resize,
        ] {
            event_tx.send(event).unwrap();
        }
        drop(event_tx);
        run(&mut terminal, &mut screen, &event_rx, &note_tx).unwrap();
        let at = SessionTime::from_nanos(7);
        let sent: Vec<_> = note_rx.try_iter().collect();
        assert_eq!(
            sent,
            [
                Annotation::Mark(Mark { at }),
                Annotation::Note(Note {
                    at,
                    text: "hi".into()
                })
            ]
        );
        // The last draw shows the text with its mark.
        let screen_text = format!("{}", terminal.backend());
        assert!(screen_text.contains("│ ◆ loop text"), "{screen_text}");
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
                at: SessionTime::from_nanos(3)
            })
        );
        assert!(err.to_string().contains("mark"));
    }
}
