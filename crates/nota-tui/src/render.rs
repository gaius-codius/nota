//! Drawing the Recording screen.
//!
//! The layout follows the spec's `Main` mockup (62 × 20):
//!
//! ```text
//! ╭─ ≈ nota · <title> ──────────────────────── ● REC 01:12:48 ─╮  frame, state
//! │          ◆            ◇          ◆            ◇        ◆   │  marks and notes
//! │ ▆▇▆▇▄▂▄▃▄▅▂▃▁▁▅▅▆▇▄▄▅▄▇▆▁▃▁▁▄▃▅▅▂▄▄▅█▆▅▄▁▃▃▃▅▃▁▃▂▅▇▆▇▅▃▃▃▅ │  levels, live end right
//! │                                                            │
//! │ ◆ transcript, ◆/◇ in the margin of their line, newest      │
//! │   brightest                                                │
//! │   ░░░                                                      │  still transcribing
//! ╰─ m mark  n note  s stop ─────────────────── <source · size> ─╯  keys, status
//! ```
//!
//! - A warning takes `● REC`'s place in the top border and keeps the clock:
//!   `⚠ mic lost · 01:12:48`.
//! - On the band, `·` marks a stretch where nothing was recorded, and `┊` a
//!   change (a route change, a device lost or back, a fault starting).
//!
//! **Stopped by a full disk:** the top border reads `■ stopped · disk full`
//! with no clock, the transcript moves up for what happened and what was
//! saved, `░░░` isn't drawn, and the footer is `⏎ home` and
//! `<time> saved`:
//!
//! ```text
//! │   …the end of the transcript                               │
//! │                                                            │
//! │   The disk is full, so nota stopped the recording.         │
//! │   Everything up to 01:12:48 is saved.                      │
//! │   Free some space, then open the session from Home to…     │
//! │                                                            │
//! ╰─ ⏎ home ──────────────────────────────────── 1h 12m saved ─╯
//! ```
//!
//! From 100 columns (`MainWide`), the marks-and-notes panel takes the
//! right 32, after a gap of 2; the main panel keeps its layout in the rest:
//!
//! ```text
//! ╭─ marks & notes ────────── 5 ─╮  how many
//! │ ◆ 0:12:06                    │  each in time order, with its time
//! │   care plan changes go to t… │  a mark's text heard then, a note's own
//! │                              │
//! ╰─ j/k move ─────── 3 ◆ · 2 ◇ ─╯
//! ```

use nota_core::{SessionTime, Utterance};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::annotation::Annotation;
use crate::band::column_of;
use crate::screen::{Recording, Shown, Tone};
use crate::text::{display_width, graphemes, wrap};
use crate::theme::Theme;

/// The smallest screen the layout fits (the spec's "about 60×20").
pub(crate) const MIN_WIDTH: u16 = 60;
/// See [`MIN_WIDTH`].
pub(crate) const MIN_HEIGHT: u16 = 20;

/// The narrowest screen with the marks-and-notes panel (the spec's "about
/// 100 columns"): the main panel then has 66.
pub(crate) const WIDE_MIN_WIDTH: u16 = 100;
/// The marks-and-notes panel's width, its frame included.
const PANEL_WIDTH: u16 = 32;
/// The columns between the main panel and the marks-and-notes panel.
const PANEL_GAP: u16 = 2;
/// Rows each entry of the marks-and-notes panel takes: its glyph and time,
/// its text, and a blank between it and the next.
const ENTRY_ROWS: usize = 3;

/// Rows inside the frame above the transcript: marks, levels, a blank.
const BAND_ROWS: u16 = 3;
/// Columns left of the transcript: a space, the ◆/◇ margin, a space.
const MARGIN: u16 = 3;

/// The columns of a frame row besides its content and fill: `╭─ `, a
/// space either side of the fill, and ` ─╮`.
const FRAME_ROW_FIXED: usize = 8;

const MARK: &str = "◆";
const NOTE: &str = "◇";
const TRANSCRIBING: &str = "░░░";
/// A stretch of the band where no track recorded anything.
const GAP: char = '·';
/// A change on the band: a route change, a track lost or back, a fault
/// starting.
const CHANGE: char = '┊';

impl Recording {
    /// Draws the screen over the whole frame: the main panel, and beside it
    /// the marks-and-notes panel from 100 columns.
    pub fn draw(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            self.draw_too_small(area, frame.buffer_mut());
            return;
        }
        if area.width < WIDE_MIN_WIDTH {
            self.draw_main(area, frame);
            return;
        }
        let side = PANEL_WIDTH + PANEL_GAP;
        self.draw_main(
            Rect {
                width: area.width - side,
                ..area
            },
            frame,
        );
        let panel = Rect {
            x: area.right() - PANEL_WIDTH,
            width: PANEL_WIDTH,
            ..area
        };
        self.draw_panel(panel, frame.buffer_mut());
    }

    /// The main panel, over `area`.
    fn draw_main(&self, area: Rect, frame: &mut Frame<'_>) {
        let buf = frame.buffer_mut();
        // The wax panel, inside the frame.
        buf.set_style(
            Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2),
            self.theme.panel,
        );
        self.draw_top(area, buf);
        // Inside the frame, with a column of padding on each side.
        let inner = Rect::new(area.x + 2, area.y + 1, area.width - 4, area.height - 2);
        self.draw_band(inner, buf);
        let mut transcript = Rect::new(
            area.x + 1,
            inner.y + BAND_ROWS,
            area.width - 2,
            inner.height - BAND_ROWS,
        );
        if let Some(at) = self.stopped_by_full_disk() {
            transcript = self.draw_stopped_notice(transcript, at, buf);
        }
        self.draw_transcript(transcript, buf);
        for y in inner.y..inner.bottom() {
            buf.set_string(area.x, y, "│", self.theme.border);
            buf.set_string(area.right() - 1, y, "│", self.theme.border);
        }
        if let Some(cursor) = self.draw_bottom(area, buf) {
            frame.set_cursor_position(cursor);
        }
    }

    fn draw_too_small(&self, area: Rect, buf: &mut Buffer) {
        // The stop question shows here too, so a stop is never confirmed
        // unseen.
        if self.is_confirming_stop() {
            let lines = [
                Line::styled("stop recording?", self.theme.text_bright),
                Line::from(vec![
                    Span::styled("y", self.theme.text),
                    Span::styled(" stop  ", self.theme.text_hint),
                    Span::styled("n", self.theme.text),
                    Span::styled(" keep recording", self.theme.text_hint),
                ]),
            ];
            centred(area, lines, buf);
        } else {
            too_small(area, &self.theme, buf);
        }
    }

    fn draw_top(&self, area: Rect, buf: &mut Buffer) {
        let now = self.clock.now().elapsed().as_secs();
        let (h, m, s) = (now / 3600, now / 60 % 60, now % 60);
        // The dot pulses: bright on even seconds, dim on odd.
        let dot = if now.is_multiple_of(2) {
            self.theme.accent
        } else {
            self.theme.text_hint
        };
        let left = vec![
            Span::styled("≈ nota", self.theme.accent),
            Span::styled(" · ", self.theme.text_hint),
            Span::styled(self.title.as_str(), self.theme.text),
        ];
        let clock = format!("{h:02}:{m:02}:{s:02}");
        // A warning takes `● REC`'s place and keeps the clock: the clock
        // still counting is what shows the recording goes on. A recording
        // that has stopped has no clock to keep.
        let stopped = self.stopped_by_full_disk().is_some();
        let right = match self.warnings.top(self.clock.now()) {
            _ if stopped => vec![
                Span::styled("■ stopped", self.theme.accent),
                Span::styled(" · ", self.theme.text_hint),
                Span::styled("disk full", self.theme.text),
            ],
            Some(shown) => {
                let room = usize::from(area.width).saturating_sub(FRAME_ROW_FIXED + 1);
                self.warning_spans(shown, clock, room)
            }
            None => vec![
                Span::styled("●", dot),
                Span::styled(format!(" REC {clock}"), self.theme.accent),
            ],
        };
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

    /// The top border's right side with `shown` in place of `● REC`:
    /// `⚠ mic lost +1 · 01:12:48`, in `room` columns at most. The words
    /// are cut short before the count and the clock are: the clock still
    /// counting is what shows the recording goes on.
    fn warning_spans(&self, shown: Shown, clock: String, room: usize) -> Vec<Span<'static>> {
        let style = match shown.tone {
            Tone::Accent => self.theme.accent,
            Tone::Gold => self.theme.gold,
            Tone::Plain => self.theme.text,
            Tone::Good => self.theme.green,
        };
        let mut rest = Vec::new();
        if shown.more > 0 {
            rest.push(Span::styled(format!(" +{}", shown.more), style));
        }
        rest.push(Span::styled(" · ", self.theme.text_hint));
        rest.push(Span::styled(clock, self.theme.text));
        let words = truncate(
            vec![Span::styled(shown.text, style)],
            room.saturating_sub(spans_width(&rest)),
        );
        words.into_iter().chain(rest).collect()
    }

    fn draw_bottom(&self, area: Rect, buf: &mut Buffer) -> Option<Position> {
        // The tracks' sources, as route changes leave them, if the screen
        // was told of them.
        let source = self.warnings.sources();
        let source = source.as_deref().unwrap_or(&self.source);
        let right = vec![Span::styled(
            format!("{source} · {}", megabytes(self.recorded_bytes)),
            self.theme.text_secondary,
        )];
        let row = Rect::new(area.x, area.bottom() - 1, area.width, 1);
        if let Some(at) = self.stopped_by_full_disk() {
            // The only key that works is the one that leaves; the source
            // and size give way to how much is saved.
            let left = vec![
                Span::styled("⏎", self.theme.text),
                Span::styled(" home", self.theme.text_hint),
            ];
            let saved = vec![Span::styled(
                format!("{} saved", saved_words(at)),
                self.theme.text_secondary,
            )];
            let border = self.theme.border;
            frame_row(row, ('╰', '╯'), left, saved, Keep::Left, border, buf);
            return None;
        }
        let key = |key: &'static str, what: &'static str| {
            [
                Span::styled(key, self.theme.text),
                Span::styled(what, self.theme.text_hint),
            ]
        };
        if self.is_confirming_stop() {
            // The question takes the keys' place, and the note's if one is
            // being typed: it comes back if the answer is no.
            let left = [
                vec![Span::styled("stop recording?  ", self.theme.text_bright)],
                key("y", " stop  ").to_vec(),
                key("n", " keep recording").to_vec(),
            ]
            .concat();
            frame_row(
                row,
                ('╰', '╯'),
                left,
                right,
                Keep::Left,
                self.theme.border,
                buf,
            );
            return None;
        }
        let Some(draft) = &self.draft else {
            // Only keys that work: `?` joins once the keys overlay exists
            // (UI spec, "Not yet designed").
            let left = [key("m", " mark  "), key("n", " note  "), key("s", " stop")].concat();
            frame_row(
                row,
                ('╰', '╯'),
                left,
                right,
                Keep::Left,
                self.theme.border,
                buf,
            );
            return None;
        };
        // While typing: the note on the left, its end kept in view. It takes
        // the room the source and size would have, if it needs it.
        let hint = "  ⏎ save  esc cancel";
        let room = usize::from(area.width)
            // The frame, its fill, the ◇ and its space, the hint.
            .saturating_sub(FRAME_ROW_FIXED + 1 + 2 + display_width(hint));
        let shown = tail_within(&draft.text, room);
        let left = vec![
            Span::styled(NOTE, self.theme.gold),
            Span::raw(" "),
            Span::styled(shown, self.theme.text_bright),
            Span::styled(hint, self.theme.text_hint),
        ];
        frame_row(
            row,
            ('╰', '╯'),
            left,
            right,
            Keep::Left,
            self.theme.border,
            buf,
        );
        let cursor_x = area.x + 5 + u16::try_from(display_width(shown)).unwrap_or(u16::MAX);
        Some(Position::new(cursor_x.min(area.right() - 1), row.y))
    }

    /// The marks row and the level row, each `area.width` columns.
    fn draw_band(&self, area: Rect, buf: &mut Buffer) {
        // A stopped recording's band ends where it stopped, rather than
        // filling with time nothing was recorded in.
        let now = self.clock.now();
        let now = self.stopped_by_full_disk().map_or(now, |at| at.min(now));
        let width = usize::from(area.width);
        // Marks over notes where both fall in one column: a mark is the
        // stronger signal.
        let mut slots: Vec<Option<&Annotation>> = vec![None; width];
        for annotation in &self.annotations {
            let slot = &mut slots[column_of(annotation.at(), now, width).min(width - 1)];
            if slot.is_none() || matches!(annotation, Annotation::Mark(_)) {
                *slot = Some(annotation);
            }
        }
        for (x, slot) in (area.x..).zip(&slots) {
            match slot {
                Some(Annotation::Mark(_)) => buf.set_string(x, area.y, MARK, self.theme.accent),
                Some(Annotation::Note(_)) => buf.set_string(x, area.y, NOTE, self.theme.gold),
                None => {}
            }
        }
        let columns = self.levels.columns(now, width);
        let live = columns.len().saturating_sub(1);
        let mut changed = vec![false; width];
        for &at in self.warnings.changes() {
            changed[column_of(at, now, width).min(width - 1)] = true;
        }
        let heard_from = columns.iter().position(Option::is_some);
        // Early on several columns share the bin `now` is in, and its first
        // level may not have come yet: none of them is a gap.
        let live_from = column_of(now, now, width);
        for (i, (x, level)) in (area.x..).zip(&columns).enumerate() {
            let (glyph, style) = match level {
                Some(_) if changed[i] => (CHANGE, self.theme.text_secondary),
                Some(level) if i == live => (crate::level::bar(*level), self.theme.accent),
                Some(level) => (crate::level::bar(*level), self.theme.text_secondary),
                // Nothing recorded here, since the first level: a gap,
                // which wins over a change in the same column. The live
                // end may be waiting for its first level, so it's left
                // blank.
                None if heard_from.is_some_and(|first| i > first) && i < live_from => {
                    (GAP, self.theme.border)
                }
                None => (' ', self.theme.text_secondary),
            };
            buf.set_string(x, area.y + 1, glyph.encode_utf8(&mut [0; 4]), style);
        }
    }

    /// The words of a recording stopped by a full disk, drawn at the bottom
    /// of `area` with a blank row above and below. Returns what's left of
    /// `area` for the transcript.
    fn draw_stopped_notice(&self, area: Rect, at: SessionTime, buf: &mut Buffer) -> Rect {
        let text_width = usize::from(area.width.saturating_sub(MARGIN + 1));
        let sentences = [
            "The disk is full, so nota stopped the recording.".to_owned(),
            format!("Everything up to {} is saved.", clock_hms(at)),
            "Free some space, then open the session from Home to finish it.".to_owned(),
        ];
        let lines: Vec<String> = sentences
            .iter()
            .flat_map(|sentence| wrap(sentence, text_width))
            .collect();
        let rows = u16::try_from(lines.len() + 2)
            .unwrap_or(u16::MAX)
            .min(area.height);
        let top = area.bottom() - rows;
        // The blank rows are the first and the last of the notice.
        for (y, text) in (top + 1..area.bottom() - 1).zip(lines) {
            let line = Line::from(vec![
                Span::raw(" ".repeat(usize::from(MARGIN))),
                Span::styled(text, self.theme.text),
            ]);
            buf.set_line(area.x, y, &line, area.width);
        }
        Rect {
            height: area.height - rows,
            ..area
        }
    }

    /// The transcript, newest at the bottom of what fits. `area` spans the
    /// frame's inside, margins included.
    fn draw_transcript(&self, area: Rect, buf: &mut Buffer) {
        let lines = self.transcript_lines(
            self.utterances.iter().rev(),
            usize::from(area.height),
            usize::from(area.width.saturating_sub(MARGIN + 1)),
        );
        let skip = lines.len().saturating_sub(usize::from(area.height));
        for (y, line) in (area.y..area.bottom()).zip(lines.iter().skip(skip)) {
            buf.set_line(area.x, y, line, area.width);
        }
    }

    /// The transcript's last lines, oldest first: at least `rows` of them
    /// if there are that many, with text wrapped to `text_width`. Built from
    /// `newest_first`, the utterances newest first, taking only as many as
    /// fill the rows, so a draw costs what's on screen, not the length of
    /// the lecture.
    fn transcript_lines<'u>(
        &self,
        newest_first: impl Iterator<Item = &'u Utterance>,
        rows: usize,
        text_width: usize,
    ) -> Vec<Line<'static>> {
        // Built bottom up, then turned over.
        let mut lines: Vec<Line<'static>> = Vec::new();
        // A stopped recording has nothing left to transcribe live.
        if self.transcribing && self.stopped_by_full_disk().is_none() {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(usize::from(MARGIN))),
                Span::styled(TRANSCRIBING, self.theme.text_secondary),
            ]));
        }
        let mut next_start = None;
        let mut newest_first = newest_first.enumerate();
        while lines.len() < rows {
            let Some((age, utterance)) = newest_first.next() else {
                break;
            };
            let style = self.age_style(age);
            let mut margin = self.margin_of(utterance.start(), next_start);
            let mut wrapped: Vec<Line<'static>> = wrap(utterance.text(), text_width)
                .into_iter()
                .map(|text| {
                    // Only the first line of the utterance carries the glyph.
                    let glyph = margin.take().unwrap_or_else(|| Span::raw(" "));
                    Line::from(vec![
                        Span::raw(" "),
                        glyph,
                        Span::raw(" "),
                        Span::styled(text, style),
                    ])
                })
                .collect();
            wrapped.reverse();
            lines.append(&mut wrapped);
            next_start = Some(utterance.start());
        }
        lines.reverse();
        lines
    }

    /// The ◆ or ◇ for the line of the utterance that starts at `start`: a
    /// mark or note belongs to the latest utterance that started at or before
    /// it, so it lands on the speech being heard when the key was pressed.
    /// A mark wins over a note.
    fn margin_of(
        &self,
        start: SessionTime,
        next_start: Option<SessionTime>,
    ) -> Option<Span<'static>> {
        // The annotations are in time order: find this utterance's by
        // halving, not by checking them all.
        let from = self.annotations.partition_point(|a| a.at() < start);
        let to = next_start.map_or(self.annotations.len(), |next| {
            self.annotations.partition_point(|a| a.at() < next)
        });
        let mine = self.annotations.get(from..to).unwrap_or_default();
        if mine.iter().any(|a| matches!(a, Annotation::Mark(_))) {
            Some(Span::styled(MARK, self.theme.accent))
        } else if mine.is_empty() {
            None
        } else {
            Some(Span::styled(NOTE, self.theme.gold))
        }
    }

    /// The marks-and-notes panel (`MainWide`), over `area`: each in time
    /// order, the selected one highlighted and kept in view, or the newest
    /// in view while none is selected.
    fn draw_panel(&mut self, area: Rect, buf: &mut Buffer) {
        let marks = self
            .annotations
            .iter()
            .filter(|a| matches!(a, Annotation::Mark(_)))
            .count();
        let notes = self.annotations.len() - marks;
        frame_row(
            Rect { height: 1, ..area },
            ('╭', '╮'),
            vec![Span::styled("marks & notes", self.theme.text_secondary)],
            vec![Span::styled(
                self.annotations.len().to_string(),
                self.theme.text_hint,
            )],
            Keep::Left,
            self.theme.border,
            buf,
        );
        frame_row(
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
            ('╰', '╯'),
            vec![
                Span::styled("j/k", self.theme.text),
                Span::styled(" move", self.theme.text_hint),
            ],
            vec![Span::styled(
                format!("{marks} {MARK} · {notes} {NOTE}"),
                self.theme.text_hint,
            )],
            Keep::Left,
            self.theme.border,
            buf,
        );
        let inside = Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2);
        for y in inside.y..inside.bottom() {
            buf.set_string(area.x, y, "│", self.theme.border);
            buf.set_string(area.right() - 1, y, "│", self.theme.border);
        }
        // The last entry needs no blank row after it.
        let shown = (usize::from(inside.height) + 1) / ENTRY_ROWS;
        let last_top = self.annotations.len().saturating_sub(shown);
        self.panel_top = match self.selected {
            None => last_top,
            Some(selected) if selected < self.panel_top => selected,
            Some(selected) if selected >= self.panel_top + shown => selected + 1 - shown,
            Some(_) => self.panel_top,
        }
        .min(last_top);
        // A column of padding each side, and the text indented by two.
        let text_width = usize::from(inside.width.saturating_sub(4));
        let rows = (inside.y..inside.bottom()).step_by(ENTRY_ROWS);
        for (y, index) in rows.zip(self.panel_top..self.annotations.len()) {
            let Some(annotation) = self.annotations.get(index) else {
                break;
            };
            let (glyph, text) = match annotation {
                Annotation::Mark(mark) => (
                    Span::styled(MARK, self.theme.accent),
                    Span::styled(self.heard_at(mark.at), self.theme.text_hint),
                ),
                Annotation::Note(note) => (
                    Span::styled(NOTE, self.theme.gold),
                    Span::styled(one_line(note.text()), self.theme.text_secondary),
                ),
            };
            if self.selected == Some(index) {
                let rows = 2.min(inside.bottom() - y);
                buf.set_style(
                    Rect::new(inside.x, y, inside.width, rows),
                    self.theme.highlight,
                );
            }
            // ▸ marks the selection too, for when the highlight has no
            // colour (`NO_COLOR`).
            let pointer = if self.selected == Some(index) {
                Span::styled("▸", self.theme.accent)
            } else {
                Span::raw(" ")
            };
            let time = Line::from(vec![
                pointer,
                glyph,
                Span::styled(format!(" {}", clock_time(annotation.at())), self.theme.text),
            ]);
            buf.set_line(inside.x, y, &time, inside.width);
            if y + 1 < inside.bottom() {
                let text = Line::from(truncate(vec![text], text_width));
                buf.set_line(inside.x + 3, y + 1, &text, inside.width - 3);
            }
        }
    }

    /// What was being heard at `at`, as one line: the text of the latest
    /// utterance that started at or before it, or nothing if none had.
    fn heard_at(&self, at: SessionTime) -> String {
        let index = self.utterances.partition_point(|u| u.start() <= at);
        index
            .checked_sub(1)
            .and_then(|index| self.utterances.get(index))
            .map_or_else(String::new, |utterance| one_line(utterance.text()))
    }

    /// The text style for the utterance `age` places from the newest (0).
    fn age_style(&self, age: usize) -> Style {
        match age {
            0 => self.theme.text_bright,
            1..=2 => self.theme.text,
            3..=5 => self.theme.text_secondary,
            _ => self.theme.text_hint,
        }
    }
}

/// The "make the window larger" message, centred in `area`.
pub(crate) fn too_small(area: Rect, theme: &Theme, buf: &mut Buffer) {
    let lines = [
        Line::styled("make the window larger", theme.text),
        Line::styled(
            format!("nota needs {MIN_WIDTH}×{MIN_HEIGHT}"),
            theme.text_hint,
        ),
    ];
    centred(area, lines, buf);
}

/// Two lines, each centred, in the middle rows of `area`.
fn centred(area: Rect, lines: [Line<'_>; 2], buf: &mut Buffer) {
    let top = area.y + area.height.saturating_sub(2) / 2;
    for (row, line) in (top..area.bottom()).zip(lines) {
        let x = area.x + area.width.saturating_sub(width_u16(&line)) / 2;
        buf.set_line(x, row, &line, area.right().saturating_sub(x));
    }
}

/// Which side of a frame row keeps its content when both don't fit.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Keep {
    /// The top row: its right side is the state, which carries warnings.
    Right,
    /// The bottom row: its left side is the keys or the note being typed;
    /// the source and size give way.
    Left,
}

/// Draws a frame's top or bottom row, `╭─ left ───── right ─╮`, across the
/// width of `row`. If both sides don't fit, the side not kept is cut short
/// with `…`, down to nothing.
pub(crate) fn frame_row(
    row: Rect,
    (left_corner, right_corner): (char, char),
    left: Vec<Span<'_>>,
    right: Vec<Span<'_>>,
    keep: Keep,
    border: Style,
    buf: &mut Buffer,
) {
    let width = usize::from(row.width);
    // "╭─ " + left + " " + fill + " " + right + " ─╮", with at least one
    // "─" of fill.
    let room = width.saturating_sub(FRAME_ROW_FIXED + 1);
    let (left, right) = match keep {
        Keep::Right => {
            let right = truncate(right, room);
            let left = truncate(left, room - spans_width(&right));
            (left, right)
        }
        Keep::Left => {
            let left = truncate(left, room);
            let right = truncate(right, room - spans_width(&left));
            (left, right)
        }
    };
    let fill = width
        .saturating_sub(spans_width(&left) + spans_width(&right) + FRAME_ROW_FIXED)
        .max(1);
    let mut spans = vec![Span::styled(format!("{left_corner}─ "), border)];
    spans.extend(left);
    spans.push(Span::styled(format!(" {} ", "─".repeat(fill)), border));
    spans.extend(right);
    spans.push(Span::styled(format!(" ─{right_corner}"), border));
    buf.set_line(row.x, row.y, &Line::from(spans), row.width);
}

/// The columns `spans` take when drawn.
pub(crate) fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| display_width(&span.content)).sum()
}

/// The spans cut to at most `room` columns, ending in `…` if anything was
/// cut. Cuts fall between grapheme clusters.
pub(crate) fn truncate(spans: Vec<Span<'_>>, room: usize) -> Vec<Span<'_>> {
    if spans_width(&spans) <= room {
        return spans;
    }
    let mut left = room.saturating_sub(1);
    let mut out = Vec::new();
    for span in spans {
        let mut text = String::new();
        for (grapheme, width) in graphemes(&span.content) {
            if width > left {
                left = 0;
                break;
            }
            text.push_str(grapheme);
            left -= width;
        }
        out.push(Span::styled(text, span.style));
        if left == 0 {
            break;
        }
    }
    if room > 0 {
        out.push(Span::raw("…"));
    }
    out
}

/// The end of `text` that fits in `room` columns, cut between grapheme
/// clusters.
fn tail_within(text: &str, room: usize) -> &str {
    let mut width = 0;
    let mut start = text.len();
    for (grapheme, grapheme_width) in graphemes(text).rev() {
        width += grapheme_width;
        if width > room {
            break;
        }
        start -= grapheme.len();
    }
    &text[start..]
}

/// `text` on one line: whitespace collapsed, and only what is drawn kept.
fn one_line(text: &str) -> String {
    wrap(text, usize::MAX).concat()
}

/// A session time as the top border shows it, `HH:MM:SS`.
fn clock_hms(at: SessionTime) -> String {
    let secs = at.elapsed().as_secs();
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// How much a stopped recording saved, for its footer: `1h 12m` from an
/// hour, `12m` from a minute, else `45s`.
fn saved_words(at: SessionTime) -> String {
    let secs = at.elapsed().as_secs();
    match (secs / 3600, secs / 60 % 60) {
        (0, 0) => format!("{secs}s"),
        (0, m) => format!("{m}m"),
        (h, m) => format!("{h}h {m}m"),
    }
}

/// A session time as the panel shows it, `H:MM:SS`.
fn clock_time(at: SessionTime) -> String {
    let secs = at.elapsed().as_secs();
    format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// Bytes as whole megabytes (10⁶), or gigabytes to one place from 1 GB.
fn megabytes(bytes: u64) -> String {
    const MB: u64 = 1_000_000;
    let mb = bytes / MB;
    if mb < 1_000 {
        format!("{mb} MB")
    } else {
        format!("{}.{} GB", mb / 1_000, mb % 1_000 / 100)
    }
}

fn width_u16(line: &Line<'_>) -> u16 {
    u16::try_from(line.width()).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use nota_core::recorder::{
        Cause, DeviceChange, Event, Level, Mark, Note, Warning, WarningState,
    };
    use nota_core::{Clock, FakeClock, SessionTime, TrackId};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::text::heard;
    use crate::theme::Theme;

    fn secs(s: u64) -> SessionTime {
        SessionTime::from_elapsed(Duration::from_secs(s)).unwrap()
    }

    /// A screen at 100 s with `source`, a level every second, and
    /// utterances starting at 10 s, 20 s, … `count` of them.
    fn screen(source: &str, utterances: u64) -> Recording {
        let clock = Arc::new(FakeClock::new(secs(100)));
        let mut screen = Recording::new(
            "Styles".into(),
            source.into(),
            clock as Arc<dyn Clock>,
            Theme::default(),
        );
        for s in 0..=100 {
            screen.update(Event::Level {
                track: TrackId::new(0),
                at: secs(s),
                level: Level::from_peak(1_000),
            });
        }
        for i in 1..=utterances {
            let text = format!("utterance {i}");
            screen.update(Event::Text(heard(i * 10, i * 10 + 5, &text)));
        }
        screen
    }

    fn draw(screen: &mut Recording, width: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
        terminal.draw(|frame| screen.draw(frame)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    /// A screen with a level every 100 ms up to `levels_to`, its clock at
    /// `now`, both in milliseconds.
    fn screen_at(levels_to: u64, now: u64) -> (Recording, Arc<FakeClock>) {
        let ms = |ms: u64| SessionTime::from_nanos(ms * 1_000_000);
        let clock = Arc::new(FakeClock::new(ms(now)));
        let mut screen = Recording::new(
            "Styles".into(),
            "mic".into(),
            Arc::clone(&clock) as Arc<dyn Clock>,
            Theme::default(),
        );
        for at in (0..=levels_to).step_by(100) {
            screen.update(Event::Level {
                track: TrackId::new(0),
                at: ms(at),
                level: Level::from_peak(1_000),
            });
        }
        (screen, clock)
    }

    /// A long device name is cut short before the clock is: the clock
    /// still counting is what shows the recording goes on.
    #[test]
    fn a_long_warning_keeps_the_clock() {
        let (mut screen, _clock) = screen_at(100_000, 100_000);
        let name = "Family 17h/19h HD Audio Controller Analog Stereo";
        screen.update(Event::Device {
            track: TrackId::new(0),
            change: DeviceChange::Changed(name.into()),
            at: secs(100),
        });
        let top = row(&draw(&mut screen, 60), 0);
        assert!(top.ends_with("… · 00:01:40 ─╮"), "{top}");
        assert!(top.contains("↪ track 0: Family"), "{top}");
    }

    /// Early in a recording several columns share the bin the clock is in;
    /// one whose level hasn't come yet isn't drawn as a gap.
    #[test]
    fn the_live_end_is_never_a_gap() {
        let (mut screen, _clock) = screen_at(500, 800);
        let band = row(&draw(&mut screen, 60), 2);
        assert!(!band.contains(GAP), "{band}");
        assert!(band.contains('▄'), "{band}");
    }

    /// Once a full disk has stopped the recording, the band ends where it
    /// stopped, rather than filling with time nothing was recorded in.
    #[test]
    fn a_stopped_band_ends_where_it_stopped() {
        let (mut screen, clock) = screen_at(100_000, 100_000);
        screen.update(Event::Warning(Warning {
            cause: Cause::DiskFull,
            track: None,
            at: secs(100),
            state: WarningState::Raised,
        }));
        let before = row(&draw(&mut screen, 60), 2);
        clock.advance(Duration::from_secs(100));
        let after = row(&draw(&mut screen, 60), 2);
        assert_eq!(after, before);
        assert!(!after.contains(GAP), "{after}");
    }

    #[test]
    fn colour_roles() {
        let theme = Theme::default();
        let mut screen = screen("Mic", 8);
        screen.add(Annotation::Mark(Mark { at: secs(10) }));
        screen.add(Annotation::Note(Note::new(secs(30), "n").unwrap()));
        let buf = draw(&mut screen, 62);
        // REC (100 s is even, so the dot is bright) and the live end.
        assert_eq!(buf[(45, 0)].symbol(), "●");
        assert_eq!(buf[(45, 0)].fg, theme.accent.fg.unwrap());
        assert_eq!(buf[(59, 2)].fg, theme.accent.fg.unwrap());
        assert_eq!(buf[(58, 2)].fg, theme.text_secondary.fg.unwrap());
        // Marks in the accent, notes in gold, on the band and in the margin.
        let mark_column = 2 + 5;
        assert_eq!(buf[(mark_column, 1)].symbol(), "◆");
        assert_eq!(buf[(mark_column, 1)].fg, theme.accent.fg.unwrap());
        assert_eq!(buf[(2, 4)].symbol(), "◆");
        assert_eq!(buf[(2, 4)].fg, theme.accent.fg.unwrap());
        assert_eq!(buf[(2, 6)].symbol(), "◇");
        assert_eq!(buf[(2, 6)].fg, theme.gold.fg.unwrap());
        // Text fades with age: newest bright, then normal, secondary, hint.
        let text_fg = |y| buf[(4, y)].fg;
        assert_eq!(text_fg(11), theme.text_bright.fg.unwrap());
        assert_eq!(text_fg(10), theme.text.fg.unwrap());
        assert_eq!(text_fg(9), theme.text.fg.unwrap());
        assert_eq!(text_fg(8), theme.text_secondary.fg.unwrap());
        assert_eq!(text_fg(6), theme.text_secondary.fg.unwrap());
        assert_eq!(text_fg(5), theme.text_hint.fg.unwrap());
        assert_eq!(text_fg(4), theme.text_hint.fg.unwrap());
    }

    #[test]
    fn the_rec_dot_pulses() {
        let theme = Theme::default();
        let clock = Arc::new(FakeClock::new(secs(101)));
        let mut screen = Recording::new("T".into(), "S".into(), clock as Arc<dyn Clock>, theme);
        let buf = draw(&mut screen, 62);
        assert_eq!(buf[(45, 0)].fg, theme.text_hint.fg.unwrap());
    }

    #[test]
    fn a_mark_wins_over_a_note_in_the_same_place() {
        let mut screen = screen("Mic", 2);
        // Both in the second utterance and in the same band column, the note
        // first in time.
        screen.add(Annotation::Note(Note::new(secs(21), "n").unwrap()));
        screen.add(Annotation::Mark(Mark { at: secs(22) }));
        // And a note alone on the first.
        screen.add(Annotation::Note(Note::new(secs(11), "n").unwrap()));
        let buf = draw(&mut screen, 62);
        assert_eq!(row(&buf, 1).matches('◆').count(), 1);
        assert_eq!(row(&buf, 1).matches('◇').count(), 1);
        assert!(
            row(&buf, 4).starts_with("│ ◇ utterance 1"),
            "{}",
            row(&buf, 4)
        );
        assert!(
            row(&buf, 5).starts_with("│ ◆ utterance 2"),
            "{}",
            row(&buf, 5)
        );
    }

    #[test]
    fn a_long_source_gives_way_to_the_keys_and_the_note() {
        let source = "alsa_input.usb-Focusrite_Scarlett_2i2_USB_Y8ABCDEF-00.analog-stereo";
        let mut screen = screen(source, 0);
        let bottom = row(&draw(&mut screen, 60), 19);
        assert_eq!(
            bottom,
            "╰─ m mark  n note  s stop ─ alsa_input.usb-Focusrite_Sca… ─╯"
        );
        screen.draft = Some(crate::screen::Draft {
            at: secs(1),
            text: "typed".into(),
        });
        let bottom = row(&draw(&mut screen, 60), 19);
        assert_eq!(
            bottom,
            "╰─ ◇ typed  ⏎ save  esc cancel ─ alsa_input.usb-Focusrit… ─╯"
        );
        // The top row keeps its state and cuts the title instead.
        screen.title = "a very long title ".repeat(5);
        let top = row(&draw(&mut screen, 60), 0);
        assert!(top.ends_with("─ ● REC 00:01:40 ─╮"), "{top}");
        assert!(
            top.starts_with("╭─ ≈ nota · a very long title a very lo… ─ ●"),
            "{top}"
        );
    }

    #[test]
    fn the_stop_question_takes_the_footer() {
        let theme = Theme::default();
        let mut screen = screen("Mic", 0);
        screen.update(Event::Recorded(14_200_000));
        screen.stop = crate::screen::Stop::Asking {
            since: SessionTime::ZERO,
        };
        let buf = draw(&mut screen, 62);
        assert_eq!(
            row(&buf, 19),
            "╰─ stop recording?  y stop  n keep recording ── Mic · 14 MB ─╯"
        );
        // The key letters in the keys' style, what they do as a hint.
        let y = 3 + "stop recording?  ".len();
        let y = u16::try_from(y).unwrap();
        assert_eq!(buf[(y, 19)].symbol(), "y");
        assert_eq!(buf[(y, 19)].fg, theme.text.fg.unwrap());
        assert_eq!(buf[(y + 2, 19)].fg, theme.text_hint.fg.unwrap());

        // It takes the note's place too, with no cursor, and the source gives
        // way.
        screen.source = "Brave".into();
        screen.draft = Some(crate::screen::Draft {
            at: secs(1),
            text: "typed".into(),
        });
        let mut terminal = Terminal::new(TestBackend::new(62, 20)).unwrap();
        terminal.draw(|frame| screen.draw(frame)).unwrap();
        assert_eq!(
            row(terminal.backend().buffer(), 19),
            "╰─ stop recording?  y stop  n keep recording ─ Brave · 14 … ─╯"
        );
        assert!(!terminal.backend().cursor_visible());
        // Answering no brings the note and its cursor back.
        screen.stop = crate::screen::Stop::No;
        terminal.draw(|frame| screen.draw(frame)).unwrap();
        assert!(terminal.backend().cursor_visible());
    }

    #[test]
    fn megabytes_are_decimal_and_switch_to_gigabytes() {
        assert_eq!(megabytes(0), "0 MB");
        assert_eq!(megabytes(14_999_999), "14 MB");
        assert_eq!(megabytes(999_999_999), "999 MB");
        assert_eq!(megabytes(1_000_000_000), "1.0 GB");
        assert_eq!(megabytes(2_345_000_000), "2.3 GB");
    }

    #[test]
    fn truncate_keeps_what_fits_and_marks_the_cut() {
        let spans = || vec![Span::raw("≈ nota"), Span::raw(" · "), Span::raw("Title")];
        let text = |spans: Vec<Span<'_>>| {
            spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        assert_eq!(text(truncate(spans(), 14)), "≈ nota · Title");
        assert_eq!(text(truncate(spans(), 13)), "≈ nota · Tit…");
        assert_eq!(text(truncate(spans(), 6)), "≈ not…");
        assert_eq!(text(truncate(spans(), 1)), "…");
        assert_eq!(text(truncate(spans(), 0)), "");
    }

    #[test]
    fn tail_within_keeps_the_end() {
        assert_eq!(tail_within("abcdef", 3), "def");
        assert_eq!(tail_within("abc", 3), "abc");
        assert_eq!(tail_within("ab日本", 3), "本");
        assert_eq!(tail_within("abc", 0), "");
    }

    /// The transcript's lines as they were built before the draw walked
    /// back from the newest: every utterance wrapped, every annotation
    /// checked against each, then the last `rows` kept.
    fn every_line(screen: &Recording, rows: usize, text_width: usize) -> Vec<Line<'static>> {
        let newest = screen.utterances.len();
        let mut lines = Vec::new();
        for (index, utterance) in screen.utterances.iter().enumerate() {
            let style = screen.age_style(newest - 1 - index);
            let next = screen.utterances.get(index + 1).map(Utterance::start);
            let mine = |at| at >= utterance.start() && next.is_none_or(|next| at < next);
            let mut glyph = None;
            for annotation in screen.annotations.iter().filter(|a| mine(a.at())) {
                glyph = match annotation {
                    Annotation::Mark(_) => Some(Span::styled(MARK, screen.theme.accent)),
                    Annotation::Note(_) => {
                        glyph.or_else(|| Some(Span::styled(NOTE, screen.theme.gold)))
                    }
                };
                if matches!(annotation, Annotation::Mark(_)) {
                    break;
                }
            }
            for text in wrap(utterance.text(), text_width) {
                let glyph = glyph.take().unwrap_or_else(|| Span::raw(" "));
                lines.push(Line::from(vec![
                    Span::raw(" "),
                    glyph,
                    Span::raw(" "),
                    Span::styled(text, style),
                ]));
            }
        }
        if screen.transcribing {
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(TRANSCRIBING, screen.theme.text_secondary),
            ]));
        }
        let skip = lines.len().saturating_sub(rows);
        lines.split_off(skip)
    }

    #[test]
    fn a_draw_reads_only_the_utterances_it_shows() {
        let mut screen = screen("Mic", 10_000);
        screen.add(Annotation::Mark(Mark { at: secs(100_001) }));
        let read = std::cell::Cell::new(0);
        let newest_first = screen
            .utterances
            .iter()
            .rev()
            .inspect(|_| read.set(read.get() + 1));
        let lines = screen.transcript_lines(newest_first, 14, 58);
        // One line each, so 14 fill the rows.
        assert_eq!(read.get(), 14);
        assert_eq!(lines, every_line(&screen, 14, 58));
        // The whole screen draws the same, at any width.
        for width in [62, 100, 160] {
            let rows = (0..20)
                .map(|y| row(&draw(&mut screen, width), y))
                .collect::<Vec<_>>();
            assert!(rows[18].contains("◆ utterance 10000"), "{}", rows[18]);
            assert!(rows[4].contains("  utterance 9986 "), "{}", rows[4]);
        }
    }

    proptest::proptest! {
        /// Walking back from the newest gives the lines the whole transcript
        /// would, whatever the utterances, their marks and notes, and the
        /// room.
        #[test]
        fn walking_back_gives_the_same_lines(
            utterances in proptest::collection::vec(
                (0_u64..400, 0_usize..40),
                0..30,
            ),
            annotations in proptest::collection::vec((0_u64..420, proptest::bool::ANY), 0..12),
            transcribing in proptest::bool::ANY,
            rows in 1_usize..20,
            text_width in 1_usize..40,
        ) {
            let clock = Arc::new(FakeClock::new(secs(500)));
            let mut screen = Recording::new(
                "T".into(),
                "S".into(),
                clock as Arc<dyn Clock>,
                Theme::default(),
            );
            for (start, words) in utterances {
                let text = (0..words).map(|w| "w".repeat(w % 7 + 1)).collect::<Vec<_>>().join(" ");
                screen.update(Event::Text(heard(start, start + 1, &text)));
            }
            for (at, mark) in annotations {
                screen.add(if mark {
                    Annotation::Mark(Mark { at: secs(at) })
                } else {
                    Annotation::Note(Note::new(secs(at), "n").unwrap())
                });
            }
            screen.update(Event::Transcribing(transcribing));
            let walked = screen.transcript_lines(screen.utterances.iter().rev(), rows, text_width);
            let skip = walked.len().saturating_sub(rows);
            proptest::prop_assert_eq!(&walked[skip..], &every_line(&screen, rows, text_width)[..]);
        }
    }

    #[test]
    fn the_panel_appears_at_100_columns() {
        let mut screen = screen("Mic", 3);
        screen.add(Annotation::Mark(Mark { at: secs(12) }));
        let narrow = draw(&mut screen, WIDE_MIN_WIDTH - 1);
        assert!(!row(&narrow, 0).contains("marks & notes"));
        assert!(
            row(&narrow, 0).ends_with("REC 00:01:40 ─╮"),
            "{}",
            row(&narrow, 0)
        );
        assert!(row(&narrow, 10).ends_with('│'));
        let wide = draw(&mut screen, WIDE_MIN_WIDTH);
        // The main panel ends at 66 columns, two blank, then the panel.
        let top = row(&wide, 0);
        assert!(top.starts_with("╭─ ≈ nota · Styles"), "{top}");
        assert_eq!(top.chars().nth(65), Some('╮'), "{top}");
        assert!(
            top.ends_with("─╮  ╭─ marks & notes ────────── 1 ─╮"),
            "{top}"
        );
        assert_eq!(
            row(&wide, 19).chars().skip(68).collect::<String>(),
            "╰─ j/k move ─────── 1 ◆ · 0 ◇ ─╯"
        );
        // The main panel's layout is the narrow one's, at its width.
        let main_only = draw(&mut screen, 66);
        for y in 0..20 {
            let wide_row: String = row(&wide, y).chars().take(66).collect();
            assert_eq!(wide_row, row(&main_only, y), "row {y}");
        }
    }

    #[test]
    fn the_panel_lists_each_with_its_time_and_text() {
        let theme = Theme::default();
        let mut screen = screen("Mic", 3);
        screen.utterances.clear();
        screen.update(Event::Text(heard(
            10,
            15,
            "a mark shows what was being said when it was made, cut short",
        )));
        screen.add(Annotation::Mark(Mark { at: secs(5) }));
        screen.add(Annotation::Mark(Mark { at: secs(3_725) }));
        screen.add(Annotation::Note(
            Note::new(secs(12), "bring\u{202e} clamps").unwrap(),
        ));
        screen.add(Annotation::Mark(Mark { at: secs(13) }));
        let buf = draw(&mut screen, 100);
        let panel = |y| row(&buf, y).chars().skip(68).collect::<String>();
        assert_eq!(panel(0), "╭─ marks & notes ────────── 4 ─╮");
        // Before any text: the time alone.
        assert_eq!(panel(1), "│ ◆ 0:00:05                    │");
        assert_eq!(panel(2), "│                              │");
        assert_eq!(panel(4), "│ ◇ 0:00:12                    │");
        assert_eq!(panel(5), "│   bring clamps               │");
        assert_eq!(panel(7), "│ ◆ 0:00:13                    │");
        assert_eq!(panel(8), "│   a mark shows what was bei… │");
        assert_eq!(panel(10), "│ ◆ 1:02:05                    │");
        assert_eq!(panel(11), "│   a mark shows what was bei… │");
        assert_eq!(panel(19), "╰─ j/k move ─────── 3 ◆ · 1 ◇ ─╯");
        assert_eq!(buf[(70, 1)].fg, theme.accent.fg.unwrap());
        assert_eq!(buf[(70, 4)].fg, theme.gold.fg.unwrap());
        assert_eq!(buf[(72, 4)].fg, theme.text.fg.unwrap());
        assert_eq!(buf[(72, 5)].fg, theme.text_secondary.fg.unwrap());
        assert_eq!(buf[(72, 8)].fg, theme.text_hint.fg.unwrap());
        // Nothing selected, nothing highlighted.
        let reversed = ratatui::style::Modifier::REVERSED;
        assert_eq!(theme.highlight.add_modifier, reversed);
        assert!((0..20).all(|y| !buf[(70, y)].modifier.contains(reversed)));
    }

    #[test]
    fn the_selection_is_highlighted_and_kept_in_view() {
        let theme = Theme::from_colors_toml("selection = \"#272539\"\n").unwrap();
        let mut screen = screen("Mic", 0);
        screen.theme = theme;
        for s in 1..=10 {
            screen.add(Annotation::Mark(Mark { at: secs(s) }));
        }
        let shown = |screen: &mut Recording| {
            let buf = draw(screen, 100);
            let times: Vec<String> = (1..19)
                .map(|y| row(&buf, y).chars().skip(72).take(7).collect::<String>())
                .filter(|time| time.starts_with("0:"))
                .collect();
            let lit: Vec<String> = (1..19)
                .filter(|&y| buf[(70, y)].bg == theme.highlight.bg.unwrap())
                .map(|y| row(&buf, y).chars().skip(72).take(7).collect::<String>())
                .collect();
            (times, lit)
        };
        // Six fit; with none selected, the newest are shown.
        let (times, lit) = shown(&mut screen);
        assert_eq!(times.first().map(String::as_str), Some("0:00:05"));
        assert_eq!(times.len(), 6);
        assert!(lit.is_empty());
        // `k` selects the newest, both its rows lit.
        screen.handle_key(ratatui::crossterm::event::KeyEvent::from(
            ratatui::crossterm::event::KeyCode::Char('k'),
        ));
        let (_, lit) = shown(&mut screen);
        assert_eq!(lit, ["0:00:10", ""].map(|s| format!("{s:7}")));
        // Up to the top: the view follows the selection, and stays put
        // while it's in view.
        screen.selected = Some(0);
        let (times, _) = shown(&mut screen);
        assert_eq!(times.first().map(String::as_str), Some("0:00:01"));
        screen.selected = Some(5);
        let (times, lit) = shown(&mut screen);
        assert_eq!(times.first().map(String::as_str), Some("0:00:01"));
        assert_eq!(lit[0], "0:00:06");
        screen.selected = Some(6);
        let (times, _) = shown(&mut screen);
        assert_eq!(times.first().map(String::as_str), Some("0:00:02"));
    }

    /// The footer's saved time is in hours and minutes from an hour, in
    /// minutes from a minute, and in seconds below that.
    #[test]
    fn saved_time_is_worded_by_its_size() {
        let words = |s| saved_words(secs(s));
        assert_eq!(words(4_368), "1h 12m");
        assert_eq!(words(3_600), "1h 0m");
        assert_eq!(words(3_599), "59m");
        assert_eq!(words(720), "12m");
        assert_eq!(words(60), "1m");
        assert_eq!(words(59), "59s");
        assert_eq!(words(0), "0s");
    }

    /// The stopped screen's words give the time as the top border does, with
    /// two digits for each part.
    #[test]
    fn the_stopped_clock_has_two_digits_for_each_part() {
        assert_eq!(clock_hms(secs(4_368)), "01:12:48");
        assert_eq!(clock_hms(secs(5)), "00:00:05");
    }
}
