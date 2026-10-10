//! The Setup page: the Setup screen on the app's terminal, with the
//! recorder listening to both sources so each one's meter moves before a
//! session exists.
//!
//! Nothing here records. A [`Preview`] opens a stream per source and
//! reports levels; it has no session, journal or database. Only `⏎` ends
//! the page with a [`Done::Start`], and the recording starts after the
//! preview's streams are closed.
//!
//! What the user chose is kept (see `remembered`) before the recording
//! starts, for `R` to use next time. If it can't be kept, the recording
//! still starts and the closing report says so.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use nota_core::recorder::{Input, Setup};
use nota_core::{Clock, SessionTime};
use nota_recorder::capture::{CaptureBackend, Devices, Preview, PreviewEvent, Source as Capture};
use nota_recorder::disk::{CHECK_INTERVAL, Usage};
use nota_recorder::fs::{Fs, StdFs};
use nota_tui::{Choice, InputThread, RunError, SetupAction, Theme};

use super::remembered::{self, Remembered};
use crate::library::Library;
use crate::record::{
    BoxError, MIC, QuitSignals, RATE, RecordArgs, SYSTEM, last_setup, segment_length, source_of,
};
use crate::terminal::Screen;

/// How the Setup page ended.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Done {
    /// `⏎`: record with this setup. The note says why the choice couldn't
    /// be kept, if it couldn't.
    Start(Setup, Option<String>),
    /// `esc`: back to Home.
    Back,
    /// Ctrl+C, or a signal: close nota.
    Quit,
}

/// What the page needs from the app.
pub(super) struct Page<'a> {
    /// Where the library is, and whether to record a tone.
    pub(super) args: &'a RecordArgs,
    /// The library, for the last recording's settings.
    pub(super) library: &'a Library,
    /// The screen colours.
    pub(super) theme: Theme,
    /// The app's clock.
    pub(super) clock: &'a Arc<dyn Clock>,
    /// Signals that close the app.
    pub(super) quit: &'a QuitSignals,
    /// The line under Engines.
    pub(super) engines: &'a str,
    /// The title a recording starts with unless it's changed.
    pub(super) title: String,
}

/// Shows Setup on `screen` until it asks for something, with the preview
/// on the backend the app records with.
///
/// # Errors
///
/// If the terminal fails or the keyboard is lost, or the preview's thread
/// can't be started.
pub(super) fn show(screen: &mut Screen, page: &Page<'_>) -> Result<Done, BoxError> {
    #[cfg(feature = "fake-capture")]
    if page.args.tone {
        let tone =
            crate::tone::Tone::new(Arc::clone(page.clock), crate::tone::Happenings::default());
        return show_with(screen, page, tone);
    }
    #[cfg(target_os = "linux")]
    return show_with(screen, page, nota_recorder::capture::PipeWireBackend);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (screen, page);
        Err("recording needs Linux for now".into())
    }
}

/// Shows Setup, previewing through `backend`.
fn show_with<B: CaptureBackend + Send + 'static>(
    screen: &mut Screen,
    page: &Page<'_>,
    backend: B,
) -> Result<Done, BoxError> {
    let (preview, heard) = Preview::start(backend, RATE, Arc::clone(page.clock))?;
    let mut meters = Meters::new(
        preview,
        heard,
        page.args.data.clone(),
        Arc::clone(page.clock),
    );
    let remembered = last_choice(page.library, &page.args.data);
    let mut setup = screen_for(page, remembered.as_ref());
    let (ui, events) = mpsc::channel();
    page.quit.show_home(Some(ui.clone()));
    let ran = if page.quit.asked() {
        Ok(SetupAction::Quit)
    } else {
        let input = InputThread::spawn(ui, Arc::clone(page.clock))?;
        let ran = screen.clear().map_err(RunError::Terminal).and_then(|()| {
            nota_tui::run_setup(screen.terminal(), &mut setup, &events, &mut |setup| {
                meters.refresh(setup);
            })
        });
        let _ = input.stop();
        ran
    };
    page.quit.show_home(None);
    // The preview's streams close before anything else opens them.
    drop(meters);
    match ran {
        Ok(SetupAction::Start) => Ok(start(&setup.chosen(), &page.args.data)),
        Ok(SetupAction::Back) => Ok(Done::Back),
        Ok(SetupAction::Quit) | Err(RunError::CommandsClosed(_)) => Ok(Done::Quit),
        Err(RunError::Terminal(e)) => Err(format!("the screen failed: {e}").into()),
        Err(RunError::InputLost(kind)) => Err(format!("the keyboard was lost: {kind}").into()),
    }
}

/// The screen as the page opens it: the title, the engines, and the last
/// settings if there are any.
fn screen_for(page: &Page<'_>, remembered: Option<&Remembered>) -> nota_tui::Setup {
    let setup = nota_tui::Setup::new(&page.title, page.engines, page.theme);
    match remembered {
        Some(last) => setup.remembering(last.listen, last.system.clone(), last.microphone.clone()),
        None => setup,
    }
}

/// Keeps `choice` for `R`, and says what to record.
fn start(choice: &Choice, data: &std::path::Path) -> Done {
    let kept = Remembered {
        listen: choice.listen,
        system: choice.system.clone(),
        microphone: choice.microphone.clone(),
    };
    let note = remembered::write(&StdFs, data, &kept)
        .err()
        .map(|e| format!("the choices couldn't be kept for next time: {e}"));
    Done::Start(kept.setup(choice.title.clone()), note)
}

/// What Setup chose last: the file Setup keeps, else the last recording's
/// tracks.
pub(super) fn last_choice(library: &Library, data: &std::path::Path) -> Option<Remembered> {
    if let Ok(Some(kept)) = remembered::read(&StdFs, data) {
        return Some(kept);
    }
    let setup = last_setup(library)?;
    let input = |input: Option<Input>| input.unwrap_or(Input::Default);
    Some(Remembered {
        listen: nota_tui::Listen::Both,
        system: input(setup.system),
        microphone: input(setup.mic),
    })
}

/// The recording's setup for `R`: the last recording's title, and what
/// Setup last chose, or else the sources the last recording had.
pub(super) fn last_setup_for(library: &Library, data: &std::path::Path) -> Setup {
    let rows = last_setup(library);
    match (remembered::read(&StdFs, data), rows) {
        (Ok(Some(kept)), rows) => {
            kept.setup(rows.map_or_else(|| "Recording".to_owned(), |r| r.title))
        }
        (_, Some(rows)) => rows,
        (_, None) => Remembered::both().setup("Recording".to_owned()),
    }
}

/// The preview, and what the screen takes from it.
struct Meters {
    preview: Preview,
    heard: Receiver<PreviewEvent>,
    /// What the preview listens to now, if it's been told.
    listening: Option<[(nota_tui::Source, Input); 2]>,
    /// Where the recording would go.
    data: PathBuf,
    clock: Arc<dyn Clock>,
    /// When the space was last measured, and for how many tracks.
    measured: Option<(SessionTime, usize)>,
}

impl Meters {
    /// Meters for `preview`, whose events come on `heard`, measuring the
    /// space left where `data` is.
    fn new(
        preview: Preview,
        heard: Receiver<PreviewEvent>,
        data: PathBuf,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            preview,
            heard,
            listening: None,
            data,
            clock,
            measured: None,
        }
    }

    /// Gives `setup` what the preview has heard, points the preview at
    /// what `setup` now says, and measures the space if it's due.
    fn refresh(&mut self, setup: &mut nota_tui::Setup) {
        for event in self.heard.try_iter() {
            apply(setup, event);
        }
        let inputs = setup.inputs();
        if self.listening.as_ref() != Some(&inputs) {
            self.preview.listen(&sources(&inputs));
            self.listening = Some(inputs);
        }
        self.measure(setup);
    }

    /// Tells `setup` how much recording the disk has room for: when it's
    /// first drawn, every [`CHECK_INTERVAL`], and when the number of tracks
    /// changes.
    fn measure(&mut self, setup: &mut nota_tui::Setup) {
        let now = self.clock.now();
        let tracks = setup.tracks();
        let due = self.measured.is_none_or(|(at, was)| {
            was != tracks || now.checked_duration_since(at).unwrap_or_default() >= CHECK_INTERVAL
        });
        if due {
            self.measured = Some((now, tracks));
            setup.set_space(room(&self.data, tracks));
        }
    }
}

/// How much recording `tracks` tracks would fit in what the disk has left,
/// or nothing if it can't say.
fn room(data: &std::path::Path, tracks: usize) -> Option<Duration> {
    let free = StdFs.free_space(data).ok()?;
    Some(Usage::new(tracks, RATE, segment_length()).time_left(free))
}

/// Gives `setup` what the preview reported.
fn apply(setup: &mut nota_tui::Setup, event: PreviewEvent) {
    let screen_source = |track| match track {
        SYSTEM => Some(nota_tui::Source::System),
        MIC => Some(nota_tui::Source::Microphone),
        _ => None,
    };
    match event {
        PreviewEvent::Level { track, level } => {
            if let Some(source) = screen_source(track) {
                setup.set_level(source, level);
            }
        }
        PreviewEvent::Failed { track, .. } => {
            if let Some(source) = screen_source(track) {
                setup.set_failed(source);
            }
        }
        PreviewEvent::Devices(devices) => setup.set_devices(listed(devices)),
    }
}

/// The devices as the screen lists them.
fn listed(devices: Devices) -> nota_tui::Devices {
    let device = |d: nota_recorder::capture::Device| nota_tui::Device {
        name: d.name,
        description: d.description,
    };
    nota_tui::Devices {
        outputs: devices.outputs.into_iter().map(device).collect(),
        inputs: devices.inputs.into_iter().map(device).collect(),
        default_output: devices.default_output,
        default_input: devices.default_input,
    }
}

/// The tracks and sources the preview listens to for what `setup` chose.
fn sources(inputs: &[(nota_tui::Source, Input); 2]) -> Vec<(nota_core::TrackId, Capture)> {
    inputs
        .iter()
        .map(|(source, input)| match source {
            nota_tui::Source::System => (SYSTEM, source_of(input, Capture::SystemAudio)),
            nota_tui::Source::Microphone => (MIC, source_of(input, Capture::Microphone)),
        })
        .collect()
}

#[cfg(test)]
mod tests;
