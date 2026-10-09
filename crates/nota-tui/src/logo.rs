//! Home's logo: three braille waves beside the wordmark `n o t a`, on the
//! wax panel.
//!
//! The logo isn't settled (UI spec, "Logo" and "Open questions"): the waves
//! are the mockup's sketch, drawn still. It lives in this one widget, so a
//! change to it touches only this file and Home's golden snapshot.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::text::display_width;
use crate::theme::Theme;

/// The logo's rows.
pub(crate) const HEIGHT: u16 = 6;

/// The waves, one string per row, each the same width.
const WAVES: [&str; HEIGHT as usize] = [
    "⠀⣀⣀⣀⣀⣀⣀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠛⠉⠉⠉⠉⠉⠉⠛⠳⠶⣤⣀⡀⠀⠀⠀⠀⠀⢀⣀⣤⡴⠶⠛",
    "⣤⠶⠶⠶⠶⠶⠶⣤⣄⣀⠀⠉⠙⠛⠛⠛⠛⠛⠋⠉⠀⢀⣀⣤",
    "⠀⣀⣀⣀⣀⣀⣀⠀⠈⠉⠛⠶⢦⣤⣤⣤⣤⣤⡴⠶⠛⠋⠉⠀",
    "⠛⠉⠉⠉⠉⠉⠉⠛⠳⠶⣤⣀⡀⠀⠀⠀⠀⠀⢀⣀⣤⡴⠶⠛",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠉⠙⠛⠛⠛⠛⠛⠋⠉⠀⠀⠀⠀",
];

/// The wordmark, beside the waves.
const WORDMARK: &str = "n o t a";
/// The row of the waves the wordmark sits on.
const WORDMARK_ROW: u16 = 3;
/// Columns between the waves and the wordmark.
const GAP: usize = 5;

/// Draws the logo centred across `area`, from its top row.
pub(crate) fn draw(area: Rect, theme: &Theme, buf: &mut Buffer) {
    let waves = display_width(WAVES[0]);
    let width = waves + GAP + display_width(WORDMARK);
    let left =
        area.x + u16::try_from(usize::from(area.width).saturating_sub(width) / 2).unwrap_or(0);
    for (y, wave) in (area.y..area.bottom()).zip(WAVES) {
        buf.set_stringn(left, y, wave, usize::from(area.width), theme.accent);
    }
    if area.height > WORDMARK_ROW {
        let x = left + u16::try_from(waves + GAP).unwrap_or(0);
        let room = usize::from(area.right().saturating_sub(x));
        buf.set_stringn(x, area.y + WORDMARK_ROW, WORDMARK, room, theme.text_bright);
    }
}
