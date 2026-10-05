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
//! ╰─ m mark  n note  ? keys ─────────────────── <source · size> ─╯  keys, status
//! ```

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::annotation::Annotation;
use crate::band::column_of;
use crate::screen::Recording;
use crate::text::{Utterance, wrap};

/// The smallest screen the layout fits (the spec's "about 60×20").
pub(crate) const MIN_WIDTH: u16 = 60;
/// See [`MIN_WIDTH`].
pub(crate) const MIN_HEIGHT: u16 = 20;

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

impl Recording {
    /// Draws the screen over the whole frame.
    pub fn draw(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            self.draw_too_small(area, frame.buffer_mut());
            return;
        }
        let buf = frame.buffer_mut();
        self.draw_top(area, buf);
        // Inside the frame, with a column of padding on each side.
        let inner = Rect::new(area.x + 2, area.y + 1, area.width - 4, area.height - 2);
        self.draw_band(inner, buf);
        let transcript = Rect::new(
            area.x + 1,
            inner.y + BAND_ROWS,
            area.width - 2,
            inner.height - BAND_ROWS,
        );
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
        let message = "make the window larger";
        let lines = [
            Line::styled(message, self.theme.text),
            Line::styled(
                format!("nota needs {MIN_WIDTH}×{MIN_HEIGHT}"),
                self.theme.text_hint,
            ),
        ];
        let top = area.y + area.height.saturating_sub(2) / 2;
        for (row, line) in (top..area.bottom()).zip(lines) {
            let x = area.x + area.width.saturating_sub(width_u16(&line)) / 2;
            buf.set_line(x, row, &line, area.right().saturating_sub(x));
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
        let right = vec![
            Span::styled("●", dot),
            Span::styled(format!(" REC {h:02}:{m:02}:{s:02}"), self.theme.accent),
        ];
        frame_row(
            area.x,
            area.y,
            area.width,
            ('╭', '╮'),
            left,
            right,
            self.theme.border,
            buf,
        );
    }

    fn draw_bottom(&self, area: Rect, buf: &mut Buffer) -> Option<Position> {
        let right = vec![Span::styled(
            format!("{} · {}", self.source, megabytes(self.recorded_bytes)),
            self.theme.text_secondary,
        )];
        let y = area.bottom() - 1;
        let Some(draft) = &self.draft else {
            let key = |key: &'static str, what: &'static str| {
                [
                    Span::styled(key, self.theme.text),
                    Span::styled(what, self.theme.text_hint),
                ]
            };
            let left = [key("m", " mark  "), key("n", " note  "), key("?", " keys")].concat();
            frame_row(
                area.x,
                y,
                area.width,
                ('╰', '╯'),
                left,
                right,
                self.theme.border,
                buf,
            );
            return None;
        };
        // While typing: the note on the left, its end kept in view.
        let hint = "  ⏎ save  esc cancel";
        let room = usize::from(area.width).saturating_sub(
            // The frame, its fill, the ◇ and its space, the hint, the status.
            FRAME_ROW_FIXED + 1 + 2 + hint.width() + right.iter().map(Span::width).sum::<usize>(),
        );
        let shown = tail_within(&draft.text, room);
        let left = vec![
            Span::styled(NOTE, self.theme.gold),
            Span::raw(" "),
            Span::styled(shown, self.theme.text_bright),
            Span::styled(hint, self.theme.text_hint),
        ];
        frame_row(
            area.x,
            y,
            area.width,
            ('╰', '╯'),
            left,
            right,
            self.theme.border,
            buf,
        );
        let cursor_x = area.x + 5 + u16::try_from(shown.width()).unwrap_or(u16::MAX);
        Some(Position::new(cursor_x.min(area.right() - 1), y))
    }

    /// The marks row and the level row, each `area.width` columns.
    fn draw_band(&self, area: Rect, buf: &mut Buffer) {
        let now = self.clock.now();
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
        for (i, (x, level)) in (area.x..).zip(&columns).enumerate() {
            let style = if i == live {
                self.theme.accent
            } else {
                self.theme.text_secondary
            };
            let bar = level.map_or(' ', crate::level::Level::bar);
            buf.set_string(x, area.y + 1, bar.encode_utf8(&mut [0; 4]), style);
        }
    }

    /// The transcript, newest at the bottom of what fits. `area` spans the
    /// frame's inside, margins included.
    fn draw_transcript(&self, area: Rect, buf: &mut Buffer) {
        let text_width = usize::from(area.width.saturating_sub(MARGIN + 1));
        let newest = self.utterances.len();
        let mut lines: Vec<Line<'_>> = Vec::new();
        for (index, utterance) in self.utterances.iter().enumerate() {
            let style = self.age_style(newest - 1 - index);
            let next_start = self.utterances.get(index + 1).map(Utterance::start);
            let mut margin = self.margin_of(utterance.start(), next_start);
            for text in wrap(utterance.text(), text_width) {
                // Only the first line of the utterance carries the glyph.
                let glyph = margin.take().unwrap_or_else(|| Span::raw(" "));
                lines.push(Line::from(vec![
                    Span::raw(" "),
                    glyph,
                    Span::raw(" "),
                    Span::styled(text, style),
                ]));
            }
        }
        if self.transcribing {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(usize::from(MARGIN))),
                Span::styled(TRANSCRIBING, self.theme.text_secondary),
            ]));
        }
        let skip = lines.len().saturating_sub(usize::from(area.height));
        for (y, line) in (area.y..area.bottom()).zip(lines.iter().skip(skip)) {
            buf.set_line(area.x, y, line, area.width);
        }
    }

    /// The ◆ or ◇ for the line of the utterance that starts at `start`: a
    /// mark or note belongs to the latest utterance that started at or before
    /// it, so it lands on the speech being heard when the key was pressed.
    /// A mark wins over a note.
    fn margin_of(
        &self,
        start: nota_core::SessionTime,
        next_start: Option<nota_core::SessionTime>,
    ) -> Option<Span<'static>> {
        let mine = |at| at >= start && next_start.is_none_or(|next| at < next);
        let mut found = None;
        for annotation in self.annotations.iter().filter(|a| mine(a.at())) {
            match annotation {
                Annotation::Mark(_) => return Some(Span::styled(MARK, self.theme.accent)),
                Annotation::Note(_) => found = Some(Span::styled(NOTE, self.theme.gold)),
            }
        }
        found
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

/// Draws a frame's top or bottom row: `╭─ left ───── right ─╮`. The left
/// side is cut short if both don't fit; the right (state) never is, since it
/// carries warnings.
#[expect(
    clippy::too_many_arguments,
    reason = "one row's geometry, corners, content and border style; a struct would only rename them"
)]
fn frame_row(
    x: u16,
    y: u16,
    width: u16,
    (left_corner, right_corner): (char, char),
    left: Vec<Span<'_>>,
    right: Vec<Span<'_>>,
    border: Style,
    buf: &mut Buffer,
) {
    let width = usize::from(width);
    let right_width: usize = right.iter().map(Span::width).sum();
    // "╭─ " + left + " " + fill + " " + right + " ─╮", with at least one
    // "─" of fill.
    let room = width.saturating_sub(right_width + FRAME_ROW_FIXED + 1);
    let left = truncate(left, room);
    let left_width: usize = left.iter().map(Span::width).sum();
    let fill = width
        .saturating_sub(left_width + right_width + FRAME_ROW_FIXED)
        .max(1);
    let mut spans = vec![Span::styled(format!("{left_corner}─ "), border)];
    spans.extend(left);
    spans.push(Span::styled(format!(" {} ", "─".repeat(fill)), border));
    spans.extend(right);
    spans.push(Span::styled(format!(" ─{right_corner}"), border));
    let line = Line::from(spans);
    buf.set_line(x, y, &line, u16::try_from(width).unwrap_or(u16::MAX));
}

/// The spans cut to at most `room` columns, ending in `…` if anything was
/// cut.
fn truncate(spans: Vec<Span<'_>>, room: usize) -> Vec<Span<'_>> {
    let total: usize = spans.iter().map(Span::width).sum();
    if total <= room {
        return spans;
    }
    let mut left = room.saturating_sub(1);
    let mut out = Vec::new();
    for span in spans {
        let mut text = String::new();
        for c in span.content.chars() {
            let c_width = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if c_width > left {
                left = 0;
                break;
            }
            text.push(c);
            left -= c_width;
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

/// The end of `text` that fits in `room` columns.
fn tail_within(text: &str, room: usize) -> &str {
    let mut width = 0;
    let mut start = text.len();
    for (index, c) in text.char_indices().rev() {
        width += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if width > room {
            break;
        }
        start = index;
    }
    &text[start..]
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
    use super::*;

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
}
