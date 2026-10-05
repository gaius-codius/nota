//! The colour roles from the UI spec. Meaning is never carried by colour
//! alone: every coloured state also has a glyph, so [`Theme::no_color`]
//! loses nothing.

use ratatui::style::{Color, Modifier, Style};

/// The styles each colour role draws with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// REC, marks ◆, the live end of the waveform.
    pub accent: Style,
    /// Notes ◇.
    pub gold: Style,
    /// Text, newest or selected.
    pub text_bright: Style,
    /// Text, normal.
    pub text: Style,
    /// Text, secondary.
    pub text_secondary: Style,
    /// Hints and older transcript.
    pub text_hint: Style,
    /// Frames and dividers.
    pub border: Style,
}

impl Theme {
    /// No colours or emphasis at all, for `NO_COLOR`.
    #[must_use]
    pub fn no_color() -> Self {
        let plain = Style::new();
        Self {
            accent: plain,
            gold: plain,
            text_bright: plain,
            text: plain,
            text_secondary: plain,
            text_hint: plain,
            border: plain,
        }
    }
}

impl Default for Theme {
    /// The spec's accent and gold from Batroun Noir, and the terminal's own
    /// greys for the rest, until the Omarchy theme is loaded.
    fn default() -> Self {
        Self {
            accent: Style::new().fg(Color::Rgb(0xE4, 0x74, 0x4A)),
            gold: Style::new().fg(Color::Rgb(0xE8, 0xB2, 0x5C)),
            text_bright: Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            text: Style::new().fg(Color::Reset),
            text_secondary: Style::new().fg(Color::Gray),
            text_hint: Style::new().fg(Color::DarkGray),
            border: Style::new().fg(Color::DarkGray),
        }
    }
}
