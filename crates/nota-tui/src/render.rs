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

use nota_core::Utterance;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::annotation::Annotation;
use crate::band::column_of;
use crate::screen::Recording;
use crate::text::{display_width, graphemes, wrap};

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
        // The wax panel, inside the frame.
        buf.set_style(
            Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2),
            self.theme.panel,
        );
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
        // The stop question shows here too, so a stop is never confirmed
        // unseen.
        let lines = if self.is_confirming_stop() {
            [
                Line::styled("stop recording?", self.theme.text_bright),
                Line::from(vec![
                    Span::styled("y", self.theme.text),
                    Span::styled(" stop  ", self.theme.text_hint),
                    Span::styled("n", self.theme.text),
                    Span::styled(" keep recording", self.theme.text_hint),
                ]),
            ]
        } else {
            [
                Line::styled("make the window larger", self.theme.text),
                Line::styled(
                    format!("nota needs {MIN_WIDTH}×{MIN_HEIGHT}"),
                    self.theme.text_hint,
                ),
            ]
        };
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
            Rect::new(area.x, area.y, area.width, 1),
            ('╭', '╮'),
            left,
            right,
            Keep::Right,
            self.theme.border,
            buf,
        );
    }

    fn draw_bottom(&self, area: Rect, buf: &mut Buffer) -> Option<Position> {
        let right = vec![Span::styled(
            format!("{} · {}", self.source, megabytes(self.recorded_bytes)),
            self.theme.text_secondary,
        )];
        let row = Rect::new(area.x, area.bottom() - 1, area.width, 1);
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
            let bar = level.map_or(' ', crate::level::bar);
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

/// Which side of a frame row keeps its content when both don't fit.
#[derive(Debug, Clone, Copy)]
enum Keep {
    /// The top row: its right side is the state, which carries warnings.
    Right,
    /// The bottom row: its left side is the keys or the note being typed;
    /// the source and size give way.
    Left,
}

/// Draws a frame's top or bottom row, `╭─ left ───── right ─╮`, across the
/// width of `row`. If both sides don't fit, the side not kept is cut short
/// with `…`, down to nothing.
fn frame_row(
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
fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| display_width(&span.content)).sum()
}

/// The spans cut to at most `room` columns, ending in `…` if anything was
/// cut. Cuts fall between grapheme clusters.
fn truncate(spans: Vec<Span<'_>>, room: usize) -> Vec<Span<'_>> {
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

    use nota_core::recorder::{Event, Level, Mark, Note};
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

    fn draw(screen: &Recording, width: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
        terminal.draw(|frame| screen.draw(frame)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    #[test]
    fn colour_roles() {
        let theme = Theme::default();
        let mut screen = screen("Mic", 8);
        screen
            .annotations
            .push(Annotation::Mark(Mark { at: secs(10) }));
        screen
            .annotations
            .push(Annotation::Note(Note::new(secs(30), "n").unwrap()));
        let buf = draw(&screen, 62);
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
        let screen = Recording::new("T".into(), "S".into(), clock as Arc<dyn Clock>, theme);
        let buf = draw(&screen, 62);
        assert_eq!(buf[(45, 0)].fg, theme.text_hint.fg.unwrap());
    }

    #[test]
    fn a_mark_wins_over_a_note_in_the_same_place() {
        let mut screen = screen("Mic", 2);
        // Both in the second utterance and in the same band column, the note
        // first in time.
        screen
            .annotations
            .push(Annotation::Note(Note::new(secs(21), "n").unwrap()));
        screen
            .annotations
            .push(Annotation::Mark(Mark { at: secs(22) }));
        // And a note alone on the first.
        screen
            .annotations
            .push(Annotation::Note(Note::new(secs(11), "n").unwrap()));
        let buf = draw(&screen, 62);
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
        let bottom = row(&draw(&screen, 60), 19);
        assert_eq!(
            bottom,
            "╰─ m mark  n note  s stop ─ alsa_input.usb-Focusrite_Sca… ─╯"
        );
        screen.draft = Some(crate::screen::Draft {
            at: secs(1),
            text: "typed".into(),
        });
        let bottom = row(&draw(&screen, 60), 19);
        assert_eq!(
            bottom,
            "╰─ ◇ typed  ⏎ save  esc cancel ─ alsa_input.usb-Focusrit… ─╯"
        );
        // The top row keeps its state and cuts the title instead.
        screen.title = "a very long title ".repeat(5);
        let top = row(&draw(&screen, 60), 0);
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
        let buf = draw(&screen, 62);
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
}
