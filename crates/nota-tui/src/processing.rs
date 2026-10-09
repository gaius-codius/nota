//! The session page while final processing runs, with an immediately
//! readable transcript and persisted job states supplied by the caller.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::Theme;
use crate::render::{Keep, MIN_HEIGHT, MIN_WIDTH, frame_row, too_small, truncate};
use crate::text::{is_drawn, wrap};

/// Where one processing step is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessingState {
    /// Not yet started.
    Waiting { reason: String },
    /// Work underway, with progress in percent (clamped when drawn).
    Running { progress: u8 },
    /// Finished successfully.
    Done,
    /// Stopped, with an actionable explanation.
    Failed { reason: String },
}

/// One step, named alongside the engine doing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessingJob {
    /// Human-readable step name.
    pub name: String,
    /// Engine name, or `nota` for a local step.
    pub engine: String,
    /// Current durable state.
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
    title: String,
    engines: String,
    notice: Option<String>,
    theme: Theme,
    jobs: Vec<ProcessingJob>,
    transcript: Vec<String>,
    wrapped: Vec<String>,
    wrap_width: Option<u16>,
    show_transcript: bool,
    scroll: usize,
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
            show_transcript: false,
            scroll: 0,
            page_rows: 1,
        }
    }

    /// Updates the session title read from the library.
    pub fn set_title(&mut self, mut title: String) {
        title.retain(is_drawn);
        self.title = title;
    }

    /// Replaces job states after a durable update, without changing the view.
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
            KeyCode::Tab | KeyCode::BackTab => self.show_transcript = !self.show_transcript,
            KeyCode::Down | KeyCode::Char('j') if self.show_transcript => {
                self.scroll = self.scroll.saturating_add(1);
            }
            KeyCode::Up | KeyCode::Char('k') if self.show_transcript => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::PageDown if self.show_transcript => {
                self.scroll = self.scroll.saturating_add(self.page_rows);
            }
            KeyCode::PageUp if self.show_transcript => {
                self.scroll = self.scroll.saturating_sub(self.page_rows);
            }
            KeyCode::Home if self.show_transcript => self.scroll = 0,
            _ => {}
        }
        None
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
        frame_row(
            Rect::new(area.x, area.y, area.width, 1),
            ('╭', '╮'),
            vec![
                Span::styled("≈ nota", self.theme.accent),
                Span::styled(format!(" · {}", self.title), self.theme.text),
            ],
            vec![Span::styled("◐ processing", self.theme.gold)],
            Keep::Right,
            self.theme.border,
            buf,
        );
        self.draw_band(inside, buf);
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
                Span::styled("Summary ◐", tab_style(!self.show_transcript)),
                Span::raw("   "),
                Span::styled("Transcript", tab_style(self.show_transcript)),
            ]),
            inside.width - 3,
        );
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
        let panel = Rect::new(
            inside.x + 2,
            inside.y + 5,
            inside.width - 4,
            inside.height - 5,
        );
        self.page_rows = usize::from(panel.height);
        let lines = if self.show_transcript {
            self.transcript_lines(panel.width)
        } else {
            self.summary_lines(usize::from(panel.width))
        };
        for (y, line) in (panel.y..panel.bottom()).zip(lines) {
            buf.set_line(panel.x, y, &line, panel.width);
        }
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

    fn progress(&self) -> u8 {
        self.jobs
            .iter()
            .find_map(|job| match job.state {
                ProcessingState::Running { progress } => Some(progress.min(100)),
                _ => None,
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

    fn summary_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::styled("Finishing up", self.theme.text_bright),
            Line::raw(""),
        ];
        for job in &self.jobs {
            let (glyph, style, state) = match &job.state {
                ProcessingState::Waiting { reason } => (
                    "○",
                    self.theme.text_hint,
                    format!("waiting: {}", drawn(reason)),
                ),
                ProcessingState::Running { progress } => {
                    ("◐", self.theme.gold, format!("{}%", progress.min(&100)))
                }
                ProcessingState::Done => ("✓", self.theme.green, "done".to_owned()),
                ProcessingState::Failed { reason } => {
                    ("!", self.theme.gold, format!("failed: {}", drawn(reason)))
                }
            };
            lines.push(Line::from(truncate(
                vec![
                    Span::styled(format!("  {glyph}  "), style),
                    Span::styled(
                        format!("{} · {} · {state}", drawn(&job.name), drawn(&job.engine)),
                        self.theme.text,
                    ),
                ],
                width,
            )));
        }
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
}

fn drawn(text: &str) -> String {
    text.chars().filter(|&c| is_drawn(c)).collect()
}
