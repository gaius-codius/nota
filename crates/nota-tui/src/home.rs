//! The Home screen (the UI spec's `Home`): the logo, the recent sessions
//! with their status, duration and date, and the engines on the bottom
//! border.
//!
//! ```text
//! ╭─ ≈ nota ─────────────────────────────────── ! 1 needs you ─╮
//! │                                                            │
//! │            <the logo: braille waves and n o t a>           │
//! │                                                            │
//! │ recent                                                     │
//! │ ▸ ! Pharmacy law · scheduling                  58m  2 Oct  │  selected, pinned
//! │     stopped early: disk full · free space to finish        │  what it needs
//! │   ✓ Pharmacy workshop · chronic conditions  2h 42m  today  │
//! ╰─ r record  R last settings  ⏎ open ──────────────── parakeet ─╯
//! ```
//!
//! Sessions that need you (`!`) are pinned to the top, however old; the
//! rest follow newest first. The selected session's detail (what it needs,
//! or how it was recovered) shows on the line below it.
//!
//! A problem (a recording that couldn't start, the sessions not listed)
//! shows on the line above `recent` until a key is pressed once it has been
//! drawn there.
//!
//! The footer offers only keys that work: `/` joins it with search and `?`
//! with the keys overlay. `⏎` asks the app to open the
//! selected session on its Processing screen.

use std::time::Duration;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::logo;
use crate::render::{Keep, MIN_HEIGHT, MIN_WIDTH, frame_row, too_small, truncate};
use crate::text::{display_width, is_drawn};
use crate::theme::Theme;

/// Rows inside the frame above the logo, and between it and the list.
const LOGO_GAP: u16 = 2;
/// Columns left of a session's title: a space, `▸`, a space, its status
/// glyph, a space.
const ROW_LEAD: usize = 5;

/// Where a session is, as Home shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// `✓`: recorded and done with.
    Ready,
    /// `◐`: work on it is still running.
    Processing,
    /// `!`: the user must act.
    NeedsYou,
}

/// One session in Home's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// Its number. Numbers count up, so a higher one is newer.
    pub id: u64,
    /// Its title.
    pub title: String,
    /// Its status.
    pub status: Status,
    /// How long it recorded, if known.
    pub duration: Option<Duration>,
    /// When it was recorded, as Home words it (`today`, `2 Oct`), if known.
    pub date: Option<String>,
    /// What it needs, or how it was recovered, in words: shown under it
    /// when it's selected.
    pub detail: Option<String>,
}

/// What Home asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `⏎`: open the selected session.
    Open(u64),
    /// `r`: set up a recording first.
    Setup,
    /// `R`: record with the last settings.
    Record,
    /// `q`, `esc` or Ctrl+C: close nota.
    Quit,
}

/// The Home screen.
#[derive(Debug)]
pub struct Home {
    sessions: Vec<Session>,
    selected: usize,
    /// The first session row shown, so the selected one stays in view.
    scroll: usize,
    engines: String,
    theme: Theme,
    /// Something nota is doing that the user should wait for, shown in
    /// the top border.
    busy: Option<String>,
    /// Something that went wrong, shown above the list until dismissed.
    notice: Option<String>,
    /// Whether the notice has been drawn, so a key can dismiss it: one
    /// pressed in a terminal too small to show it doesn't.
    notice_drawn: bool,
}

impl Home {
    /// Home listing `sessions`, with `engines` named on the bottom border.
    /// Sessions that need the user come first, then the rest, each group
    /// newest first. Control and bidirectional formatting characters in the
    /// text are dropped, so they can't reorder or break a row.
    #[must_use]
    pub fn new(sessions: Vec<Session>, engines: &str, theme: Theme) -> Self {
        Self {
            sessions: listed(sessions),
            selected: 0,
            scroll: 0,
            engines: drawn(engines),
            theme,
            busy: None,
            notice: None,
            notice_drawn: false,
        }
    }

    /// Lists `sessions` instead, in the order [`Home::new`] gives them. The
    /// selection stays on the same session; if that session has gone, the
    /// selection stays at the same place in the list (or the last row).
    pub fn set_sessions(&mut self, sessions: Vec<Session>) {
        let selected = self.selected();
        self.sessions = listed(sessions);
        let at = |id| self.sessions.iter().position(|s| s.id == id);
        self.selected = selected
            .and_then(at)
            .unwrap_or_else(|| self.selected.min(self.sessions.len().saturating_sub(1)));
    }

    /// The sessions in the order they're listed.
    #[must_use]
    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    /// The selected session's number, if there are any.
    #[must_use]
    pub fn selected(&self) -> Option<u64> {
        self.sessions.get(self.selected).map(|s| s.id)
    }

    /// Shows `what` in the top border while nota does it, or nothing.
    pub fn set_busy(&mut self, what: Option<String>) {
        self.busy = what;
    }

    /// Shows `problem` above the list, until a key is pressed once it has
    /// been drawn, or nothing.
    pub fn set_notice(&mut self, problem: Option<String>) {
        self.notice = problem.map(|text| drawn(&text));
        self.notice_drawn = false;
    }

    /// The problem shown above the list, if any, as it's drawn.
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Handles a key press. Returns what Home asks for, if anything.
    ///
    /// - `r` asks for Setup, and `R` records with the last settings.
    /// - `↑`/`↓` (or `k`/`j`) move the selection.
    /// - `⏎` asks to open the selected session.
    /// - `q`, `esc` and Ctrl+C close nota.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        // Any key dismisses the notice, once it has been drawn: it has been
        // seen.
        if self.notice_drawn {
            self.notice = None;
            self.notice_drawn = false;
        }
        let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'));
        if ctrl_c {
            return Some(Action::Quit);
        }
        // A key held with Ctrl or Alt is another key: Alt+R doesn't record.
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match key.code {
            KeyCode::Char('r') => return Some(Action::Setup),
            KeyCode::Char('R') => return Some(Action::Record),
            KeyCode::Char('q') | KeyCode::Esc => return Some(Action::Quit),
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.sessions.len().saturating_sub(1));
            }
            KeyCode::Enter => return self.selected().map(Action::Open),
            _ => {}
        }
        None
    }

    /// Draws the screen over the whole frame.
    pub fn draw(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let buf = frame.buffer_mut();
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            too_small(area, &self.theme, buf);
            return;
        }
        let inside = Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2);
        buf.set_style(inside, self.theme.panel);
        for y in inside.y..inside.bottom() {
            buf.set_string(area.x, y, "│", self.theme.border);
            buf.set_string(area.right() - 1, y, "│", self.theme.border);
        }
        self.draw_top(area, buf);
        self.draw_list(inside, buf);
        self.draw_bottom(area, buf);
    }

    fn draw_top(&self, area: Rect, buf: &mut Buffer) {
        let needing = self
            .sessions
            .iter()
            .filter(|s| s.status == Status::NeedsYou)
            .count();
        let right = match (&self.busy, needing) {
            (Some(what), _) => vec![
                Span::styled("◐ ", self.theme.gold),
                Span::styled(what.clone(), self.theme.text),
            ],
            (None, 0) => vec![
                Span::styled("✓ ", self.theme.green),
                Span::styled("ready", self.theme.text_secondary),
            ],
            (None, n) => vec![
                Span::styled("! ", self.theme.gold),
                Span::styled(format!("{n} needs you"), self.theme.text),
            ],
        };
        let left = vec![Span::styled("≈ nota", self.theme.accent)];
        frame_row(
            Rect::new(area.x, area.y, area.width, 1),
            ('╭', '╮'),
            left,
            right,
            Keep::Right,
            self.theme.border,
            buf,
        );
    }

    fn draw_bottom(&self, area: Rect, buf: &mut Buffer) {
        let key = |key: &'static str, what: &'static str| {
            [
                Span::styled(key, self.theme.text),
                Span::styled(what, self.theme.text_hint),
            ]
        };
        // Only keys that work: `⏎` needs a session to open, and while nota
        // is busy (finishing a recording) no keys are read at all.
        let mut left = Vec::new();
        if self.busy.is_none() {
            left.extend(key("r", " record"));
            left.extend(key("  R", " last settings"));
            if !self.sessions.is_empty() {
                left.extend(key("  ⏎", " open"));
            }
        }
        let right = vec![Span::styled(
            self.engines.as_str(),
            self.theme.text_secondary,
        )];
        frame_row(
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
            ('╰', '╯'),
            left,
            right,
            Keep::Left,
            self.theme.border,
            buf,
        );
    }

    /// The logo, then `recent` and the sessions, in the frame's inside.
    fn draw_list(&mut self, inside: Rect, buf: &mut Buffer) {
        let logo_top = inside.y + LOGO_GAP;
        logo::draw(
            Rect::new(inside.x, logo_top, inside.width, logo::HEIGHT),
            &self.theme,
            buf,
        );
        let label_y = logo_top + logo::HEIGHT + LOGO_GAP;
        if let Some(notice) = &self.notice {
            self.notice_drawn = true;
            let room = usize::from(inside.width.saturating_sub(5));
            let mut spans = vec![Span::styled("! ", self.theme.gold)];
            spans.extend(truncate(
                vec![Span::styled(notice.as_str(), self.theme.text)],
                room,
            ));
            buf.set_line(
                inside.x + 1,
                label_y - 1,
                &Line::from(spans),
                inside.width - 1,
            );
        }
        buf.set_string(inside.x + 1, label_y, "recent", self.theme.text_secondary);
        let list = Rect::new(
            inside.x,
            label_y + 1,
            inside.width,
            inside.bottom().saturating_sub(label_y + 1),
        );
        if self.sessions.is_empty() {
            buf.set_string(
                list.x + 3,
                list.y,
                "nothing recorded yet · r to record",
                self.theme.text_hint,
            );
            return;
        }
        let lines = self.list_lines(usize::from(list.width));
        // Keep the selected row and its detail in view.
        let rows = usize::from(list.height);
        let (top, bottom) = lines
            .iter()
            .enumerate()
            .filter(|(_, (index, _))| *index == self.selected)
            .fold((usize::MAX, 0), |(lo, hi), (at, _)| {
                (lo.min(at), hi.max(at))
            });
        if top < self.scroll {
            self.scroll = top;
        } else if bottom >= self.scroll + rows {
            self.scroll = bottom + 1 - rows;
        }
        for (y, (index, line)) in (list.y..list.bottom()).zip(lines.iter().skip(self.scroll)) {
            if *index == self.selected {
                buf.set_style(Rect::new(list.x, y, list.width, 1), self.theme.highlight);
            }
            buf.set_line(list.x, y, line, list.width);
        }
    }

    /// Every row of the list, each with the index of its session: a row per
    /// session, and the selected one's detail under it.
    fn list_lines(&self, width: usize) -> Vec<(usize, Line<'static>)> {
        let mut lines = Vec::new();
        for (index, session) in self.sessions.iter().enumerate() {
            let selected = index == self.selected;
            lines.push((index, self.session_line(session, selected, width)));
            if let Some(detail) = session.detail.as_ref().filter(|_| selected) {
                let room = width.saturating_sub(ROW_LEAD + 2);
                let detail = truncate(vec![Span::raw(detail.clone())], room);
                let mut spans = vec![Span::raw(" ".repeat(ROW_LEAD))];
                spans.extend(
                    detail
                        .into_iter()
                        .map(|s| Span::styled(s.content.into_owned(), self.theme.text_secondary)),
                );
                lines.push((index, Line::from(spans)));
            }
        }
        lines
    }

    /// `▸ ! title ……… 58m  2 Oct`, `width` columns wide.
    fn session_line(&self, session: &Session, selected: bool, width: usize) -> Line<'static> {
        let (glyph, glyph_style) = match session.status {
            Status::Ready => ("✓", self.theme.green),
            Status::Processing => ("◐", self.theme.gold),
            Status::NeedsYou => ("!", self.theme.gold),
        };
        let duration = session.duration.map(duration).unwrap_or_default();
        let date = session.date.clone().unwrap_or_default();
        // The duration right-aligned, the date after it, then a space and
        // the frame's padding column.
        let right = format!("{duration:>6}  {date:<6} ");
        let room = width.saturating_sub(ROW_LEAD + display_width(&right) + 1);
        let title_style = if selected {
            self.theme.text_bright
        } else {
            self.theme.text
        };
        let title = truncate(vec![Span::raw(session.title.clone())], room);
        let title_width: usize = title.iter().map(|s| display_width(&s.content)).sum();
        let mut spans = vec![
            Span::raw(" "),
            if selected {
                Span::styled("▸", self.theme.accent)
            } else {
                Span::raw(" ")
            },
            Span::raw(" "),
            Span::styled(glyph, glyph_style),
            Span::raw(" "),
        ];
        spans.extend(
            title
                .into_iter()
                .map(|s| Span::styled(s.content.into_owned(), title_style)),
        );
        spans.push(Span::raw(" ".repeat(room - title_width + 1)));
        spans.push(Span::styled(right, self.theme.text_secondary));
        Line::from(spans)
    }
}

/// `sessions` as Home lists them: those that need the user first, then
/// the rest, each group newest first, with control and bidirectional
/// formatting characters dropped from their text.
fn listed(mut sessions: Vec<Session>) -> Vec<Session> {
    for session in &mut sessions {
        session.title = drawn(&session.title);
        session.date = session.date.as_deref().map(drawn);
        session.detail = session.detail.as_deref().map(drawn);
    }
    sessions.sort_by_key(|s| (s.status != Status::NeedsYou, std::cmp::Reverse(s.id)));
    sessions
}

/// `text` without the characters that would reorder or break a row.
fn drawn(text: &str) -> String {
    text.chars().filter(|&c| is_drawn(c)).collect()
}

/// A session's length as Home shows it: `58m`, `2h 42m`, `1h 05m`; under a
/// minute, `<1m`.
pub(crate) fn duration(length: Duration) -> String {
    let minutes = length.as_secs() / 60;
    match (minutes / 60, minutes % 60) {
        (0, 0) => "<1m".to_owned(),
        (0, m) => format!("{m}m"),
        (h, m) => format!("{h}h {m:02}m"),
    }
}

#[cfg(test)]
mod tests;
