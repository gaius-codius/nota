//! The session page while final processing runs, with an immediately
//! readable transcript and job states supplied by the caller.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::Theme;
use crate::render::{Keep, MIN_HEIGHT, MIN_WIDTH, frame_row, too_small, truncate};
use crate::text::{is_drawn, wrap};

/// What a processing step is waiting for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessingWait {
    /// The recording has not stopped yet.
    Recording,
    /// The recorder still has audio to save.
    AudioSaving,
    /// The saved audio could not be checked.
    AudioUnchecked(
        /// What went wrong when checking the audio.
        String,
    ),
    /// A working speech engine.
    Engine,
    /// Audio that has not been saved yet.
    Audio,
    /// Enough free space to continue.
    Space,
    /// A place in the queue, or the end of a recording.
    Queued,
}

impl ProcessingWait {
    /// The reason as the page shows it.
    fn label(&self) -> String {
        match self {
            Self::Recording => "recording is still underway".into(),
            Self::AudioSaving => "audio still to save".into(),
            Self::AudioUnchecked(reason) => {
                format!("saved audio can't be checked: {}", drawn(reason))
            }
            Self::Engine => "waiting for a working speech engine".into(),
            Self::Audio => "waiting for audio to be saved".into(),
            Self::Space => "waiting for free space".into(),
            Self::Queued => "queued or paused for a recording".into(),
        }
    }
}

/// Why a processing step stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessingFailure {
    /// The explanation supplied by the step that failed.
    reason: String,
}

impl ProcessingFailure {
    /// Keeps the step's explanation.
    #[must_use]
    pub const fn new(reason: String) -> Self {
        Self { reason }
    }

    /// The explanation before it is prepared for drawing.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.reason
    }
}

/// Where one processing step is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessingState {
    /// Queued or paused, keeping how far it got.
    Waiting {
        /// What the step needs before it can run.
        reason: ProcessingWait,
        /// How far the step got, in percent.
        progress: u8,
    },
    /// Work underway.
    Running {
        /// How far the step got, in percent, limited to 100 when drawn.
        progress: u8,
    },
    /// Finished successfully.
    Done,
    /// Stopped with an explanation.
    Failed {
        /// Why the step stopped.
        reason: ProcessingFailure,
        /// How far the step got, in percent.
        progress: u8,
    },
}

/// The result of the steps so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessingStatus {
    /// There is still work to do.
    Processing,
    /// Every step has finished.
    Ready,
    /// A step has failed.
    NeedsYou,
}

impl ProcessingStatus {
    /// The words in the top border.
    const fn label(self) -> &'static str {
        match self {
            Self::Processing => "◐ processing",
            Self::Ready => "✓ ready",
            Self::NeedsYou => "! needs you",
        }
    }

    /// The heading above the steps.
    const fn heading(self) -> &'static str {
        match self {
            Self::Processing => "Finishing up",
            Self::Ready => "Transcript ready",
            Self::NeedsYou => "Processing needs you",
        }
    }
}

/// Which part of the session is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessingView {
    /// The steps and their progress.
    Summary,
    /// The heard words.
    Transcript,
}

impl ProcessingView {
    /// The other view, reached with Tab.
    const fn other(self) -> Self {
        match self {
            Self::Summary => Self::Transcript,
            Self::Transcript => Self::Summary,
        }
    }
}

/// One step, named alongside the engine doing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessingJob {
    /// Human-readable step name.
    pub name: String,
    /// Engine name, or `nota` for a local step.
    pub engine: String,
    /// Where the step is.
    pub state: ProcessingState,
}

/// Navigation requested by the processing page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessingAction {
    /// Start a new recording.
    Record,
    /// Return to the library.
    Home,
    /// Close nota.
    Quit,
}

/// Processing's Summary and Transcript views.
#[derive(Debug)]
pub struct Processing {
    /// The session title.
    title: String,
    /// The engines named in the bottom border.
    engines: String,
    /// A problem shown above the current view.
    notice: Option<String>,
    /// The colours used to draw the page.
    theme: Theme,
    /// The steps and their progress.
    jobs: Vec<ProcessingJob>,
    /// The heard utterances.
    transcript: Vec<String>,
    /// The heard words wrapped to the current width.
    wrapped: Vec<String>,
    /// The width used to wrap the heard words.
    wrap_width: Option<u16>,
    /// The spinner at the current clock time.
    spinner: char,
    /// The part of the session being shown.
    view: ProcessingView,
    /// The first transcript line shown.
    scroll: usize,
    /// How many transcript lines fit on the page.
    page_rows: usize,
}

impl Processing {
    /// Opens the Summary view. Jobs and heard text can be supplied immediately.
    #[must_use]
    pub fn new(mut title: String, mut engines: String, theme: Theme) -> Self {
        title.retain(is_drawn);
        engines.retain(is_drawn);
        Self {
            title,
            engines,
            notice: None,
            theme,
            jobs: Vec::new(),
            transcript: Vec::new(),
            wrapped: Vec::new(),
            wrap_width: None,
            spinner: '◐',
            view: ProcessingView::Summary,
            scroll: 0,
            page_rows: 1,
        }
    }

    /// Advances the Summary spinner using the supplied session clock time.
    pub fn tick(&mut self, now: nota_core::SessionTime) {
        let phase = usize::try_from((now.as_nanos() / 250_000_000) % 4).unwrap_or(0);
        self.spinner = ['◐', '◓', '◑', '◒'][phase];
    }

    /// Updates the session title read from the library.
    pub fn set_title(&mut self, mut title: String) {
        title.retain(is_drawn);
        self.title = title;
    }

    /// Replaces the job states without changing the view.
    pub fn set_jobs(&mut self, jobs: Vec<ProcessingJob>) {
        self.jobs = jobs;
    }

    /// Replaces heard utterances without moving the reader's position.
    pub fn set_transcript(&mut self, transcript: Vec<String>) {
        if self.transcript != transcript {
            self.transcript = transcript;
            self.wrap_width = None;
        }
    }

    /// Shows a refresh failure while keeping the page and transcript usable.
    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice.map(|mut text| {
            text.retain(is_drawn);
            text
        });
    }

    /// Handles navigation and transcript scrolling.
    pub fn key(&mut self, key: KeyEvent) -> Option<ProcessingAction> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            return Some(ProcessingAction::Quit);
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match key.code {
            KeyCode::Char('q') => return Some(ProcessingAction::Quit),
            KeyCode::Char('r' | 'R') => return Some(ProcessingAction::Record),
            KeyCode::Esc => return Some(ProcessingAction::Home),
            KeyCode::Tab | KeyCode::BackTab => self.view = self.view.other(),
            code if self.view == ProcessingView::Transcript => self.scroll_key(code),
            _ => {}
        }
        None
    }

    /// Moves through the transcript with its scrolling keys.
    fn scroll_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_add(1);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(self.page_rows);
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(self.page_rows);
            }
            KeyCode::Home => self.scroll = 0,
            _ => {}
        }
    }

    /// Draws the whole session page, using the same minimum size as Home.
    pub fn draw(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            too_small(area, &self.theme, frame.buffer_mut());
            return;
        }
        let inside = Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2);
        let buf = frame.buffer_mut();
        buf.set_style(inside, self.theme.panel);
        for y in inside.y..inside.bottom() {
            buf.set_string(area.x, y, "│", self.theme.border);
            buf.set_string(area.right() - 1, y, "│", self.theme.border);
        }
        self.draw_top(area, buf);
        self.draw_band(inside, buf);
        self.draw_tabs(inside, buf);
        self.draw_notice(inside, buf);
        self.draw_panel(inside, buf);
        self.draw_bottom(area, buf);
    }

    /// The session title and status in the top border.
    fn draw_top(&self, area: Rect, buf: &mut Buffer) {
        frame_row(
            Rect::new(area.x, area.y, area.width, 1),
            ('╭', '╮'),
            vec![
                Span::styled("≈ nota", self.theme.accent),
                Span::styled(format!(" · {}", self.title), self.theme.text),
            ],
            vec![Span::styled(self.status().label(), self.theme.gold)],
            Keep::Right,
            self.theme.border,
            buf,
        );
    }

    /// The two views, with the current one highlighted.
    fn draw_tabs(&self, inside: Rect, buf: &mut Buffer) {
        let tab_style = |active| {
            if active {
                self.theme.accent
            } else {
                self.theme.text_secondary
            }
        };
        buf.set_line(
            inside.x + 2,
            inside.y + 3,
            &Line::from(vec![
                Span::styled(
                    format!("Summary {}", self.spinner),
                    tab_style(self.view == ProcessingView::Summary),
                ),
                Span::raw("   "),
                Span::styled(
                    "Transcript",
                    tab_style(self.view == ProcessingView::Transcript),
                ),
            ]),
            inside.width - 3,
        );
    }

    /// A refresh problem above the current view, if there is one.
    fn draw_notice(&self, inside: Rect, buf: &mut Buffer) {
        if let Some(notice) = &self.notice {
            buf.set_line(
                inside.x + 2,
                inside.y + 4,
                &Line::from(truncate(
                    vec![Span::styled(format!("! {notice}"), self.theme.gold)],
                    usize::from(inside.width - 4),
                )),
                inside.width - 4,
            );
        }
    }

    /// The current view inside the frame.
    fn draw_panel(&mut self, inside: Rect, buf: &mut Buffer) {
        let panel = Rect::new(
            inside.x + 2,
            inside.y + 5,
            inside.width - 4,
            inside.height - 5,
        );
        self.page_rows = usize::from(panel.height);
        let lines = if self.view == ProcessingView::Transcript {
            self.transcript_lines(panel.width)
        } else {
            self.summary_lines(usize::from(panel.width))
        };
        for (y, line) in (panel.y..panel.bottom()).zip(lines) {
            buf.set_line(panel.x, y, &line, panel.width);
        }
    }

    /// The working keys and engines in the bottom border.
    fn draw_bottom(&self, area: Rect, buf: &mut Buffer) {
        frame_row(
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
            ('╰', '╯'),
            vec![
                Span::styled("tab", self.theme.text),
                Span::styled(" view  ", self.theme.text_hint),
                Span::styled("r", self.theme.text),
                Span::styled(" record new  ", self.theme.text_hint),
                Span::styled("esc", self.theme.text),
                Span::styled(" home", self.theme.text_hint),
            ],
            vec![Span::styled(
                self.engines.as_str(),
                self.theme.text_secondary,
            )],
            Keep::Left,
            self.theme.border,
            buf,
        );
    }

    /// The heard words that fit at the current scroll position.
    fn transcript_lines(&mut self, width: u16) -> Vec<Line<'_>> {
        if self.wrap_width != Some(width) {
            self.wrapped = self
                .transcript
                .iter()
                .flat_map(|text| wrap(text, usize::from(width)))
                .collect();
            self.wrap_width = Some(width);
        }
        self.scroll = self
            .scroll
            .min(self.wrapped.len().saturating_sub(self.page_rows));
        if self.wrapped.is_empty() {
            vec![Line::styled("No speech heard yet.", self.theme.text_hint)]
        } else {
            self.wrapped
                .iter()
                .skip(self.scroll)
                .take(self.page_rows)
                .map(|text| Line::styled(text.as_str(), self.theme.text))
                .collect()
        }
    }

    /// The progress band above the views.
    fn draw_band(&self, inside: Rect, buf: &mut Buffer) {
        let progress = self.progress();
        let columns = inside.width.saturating_sub(2);
        let filled =
            u16::try_from(u32::from(columns) * u32::from(progress) / 100).unwrap_or(columns);
        for x in 0..columns {
            let (glyph, style) = if x < filled {
                ("▰", self.theme.accent)
            } else {
                ("▱", self.theme.text_hint)
            };
            buf.set_string(inside.x + 1 + x, inside.y + 1, glyph, style);
        }
    }

    /// The last unfinished step's progress, or 100 if all have finished.
    fn progress(&self) -> u8 {
        self.jobs
            .iter()
            .rev()
            .find_map(|job| match job.state {
                ProcessingState::Running { progress }
                | ProcessingState::Waiting { progress, .. }
                | ProcessingState::Failed { progress, .. } => Some(progress.min(100)),
                ProcessingState::Done => None,
            })
            .unwrap_or_else(|| {
                if !self.jobs.is_empty()
                    && self.jobs.iter().all(|j| j.state == ProcessingState::Done)
                {
                    100
                } else {
                    0
                }
            })
    }

    /// Whether the steps are running, finished or need the user.
    fn status(&self) -> ProcessingStatus {
        if self
            .jobs
            .iter()
            .any(|job| matches!(job.state, ProcessingState::Failed { .. }))
        {
            ProcessingStatus::NeedsYou
        } else if !self.jobs.is_empty()
            && self
                .jobs
                .iter()
                .all(|job| job.state == ProcessingState::Done)
        {
            ProcessingStatus::Ready
        } else {
            ProcessingStatus::Processing
        }
    }

    /// The heading, steps and summary explanation.
    fn summary_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::styled(self.status().heading(), self.theme.text_bright),
            Line::raw(""),
        ];
        lines.extend(self.jobs.iter().map(|job| self.job_line(job, width)));
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Summaries are not available yet.",
            self.theme.text_hint,
        ));
        lines.extend(
            wrap(
                "The transcript is ready to read now. You can also start another recording.",
                width,
            )
            .into_iter()
            .map(|text| Line::styled(text, self.theme.text_secondary)),
        );
        lines
    }

    /// One step, its engine and its progress.
    fn job_line(&self, job: &ProcessingJob, width: usize) -> Line<'static> {
        let (glyph, style, state) = match &job.state {
            ProcessingState::Waiting { reason, .. } => (
                "○",
                self.theme.text_hint,
                format!("waiting: {}", reason.label()),
            ),
            ProcessingState::Running { progress } => {
                ("◐", self.theme.gold, format!("{}%", progress.min(&100)))
            }
            ProcessingState::Done => ("✓", self.theme.green, "done".to_owned()),
            ProcessingState::Failed { reason, .. } => (
                "!",
                self.theme.gold,
                format!("failed: {}", drawn(reason.as_str())),
            ),
        };
        Line::from(truncate(
            vec![
                Span::styled(format!("  {glyph}  "), style),
                Span::styled(
                    format!("{} · {} · {state}", drawn(&job.name), drawn(&job.engine)),
                    self.theme.text,
                ),
            ],
            width,
        ))
    }
}

/// `text` without characters that would reorder or break a row.
fn drawn(text: &str) -> String {
    text.chars().filter(|&c| is_drawn(c)).collect()
}
