//! The colour roles from the UI spec, and where they come from: the active
//! Omarchy theme (`~/.local/state/omarchy/current/theme/colors.toml`), or
//! none at all with `NO_COLOR`. Meaning is never carried by colour alone:
//! every coloured state also has a glyph, so [`Theme::no_color`] loses
//! nothing.

use std::fmt;
use std::path::Path;

use ratatui::style::{Color, Modifier, Style};

/// Where the active Omarchy theme keeps its colours, under the home
/// directory.
const COLORS_TOML: &str = ".local/state/omarchy/current/theme/colors.toml";

/// The largest `colors.toml` read. Omarchy's are about 1 KiB; anything
/// much bigger isn't one.
const MAX_COLORS_TOML: u64 = 64 * 1024;

/// The styles each colour role draws with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// REC, marks ◆, the selected item ▸, the live end of the waveform.
    pub accent: Style,
    /// Notes ◇, "needs you" `!`.
    pub gold: Style,
    /// ✓ ready.
    pub green: Style,
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
    /// The wax panel: the background behind the main content.
    pub panel: Style,
    /// A step above the panel: the selected row.
    pub highlight: Style,
}

impl Theme {
    /// No colours or emphasis at all, for `NO_COLOR`.
    #[must_use]
    pub fn no_color() -> Self {
        let plain = Style::new();
        Self {
            accent: plain,
            gold: plain,
            green: plain,
            text_bright: plain,
            text: plain,
            text_secondary: plain,
            text_hint: plain,
            border: plain,
            panel: plain,
            highlight: plain,
        }
    }

    /// The theme the screens draw with: none if `NO_COLOR` is set (to
    /// anything but the empty string), else the Omarchy theme's colours,
    /// else [`Theme::default`]. A missing or broken `colors.toml` never
    /// fails: it gives the default.
    #[must_use]
    pub fn load() -> Self {
        Self::from_env(
            std::env::var_os("NO_COLOR").as_deref(),
            std::env::var_os("HOME").as_deref().map(Path::new),
        )
    }

    /// [`Theme::load`] with `NO_COLOR`'s value and the home directory
    /// given.
    fn from_env(no_color: Option<&std::ffi::OsStr>, home: Option<&Path>) -> Self {
        if no_color.is_some_and(|v| !v.is_empty()) {
            return Self::no_color();
        }
        home.and_then(|home| read_small(&home.join(COLORS_TOML)))
            .and_then(|text| Self::from_colors_toml(&text).ok())
            .unwrap_or_default()
    }

    /// The theme an Omarchy `colors.toml` describes. Each role takes its
    /// key as the UI spec's "Colour roles" says; a role whose key is
    /// missing or isn't a `#rrggbb` colour keeps the default's style.
    ///
    /// # Errors
    ///
    /// The text isn't TOML.
    pub fn from_colors_toml(text: &str) -> Result<Self, ThemeError> {
        let table: toml::Table = text.parse().map_err(|_| ThemeError)?;
        let colour = |key: &str| table.get(key)?.as_str().and_then(parse_hex);
        let base = Self::default();
        let fg = |key: &str, fallback: Style| colour(key).map_or(fallback, |c| Style::new().fg(c));
        let bg = |c: Option<Color>, fallback: Style| c.map_or(fallback, |c| Style::new().bg(c));
        // The border is a dark neutral between `muted` and
        // `dark_foreground`: `muted` alone is too close to the panel to see.
        let border = match (colour("muted"), colour("dark_foreground")) {
            (Some(a), Some(b)) => Style::new().fg(mix(a, b)),
            (Some(c), None) | (None, Some(c)) => Style::new().fg(c),
            (None, None) => base.border,
        };
        let panel = colour("lighter_background");
        // A step above the panel: the theme's selection colour, else the
        // panel lifted toward the text.
        let highlight = colour("selection")
            .or_else(|| panel.zip(colour("foreground")).map(|(p, f)| step(p, f)));
        Ok(Self {
            accent: fg("accent", base.accent),
            gold: fg("yellow", base.gold),
            green: fg("green", base.green),
            text_bright: fg("bright_foreground", base.text_bright),
            text: fg("foreground", base.text),
            text_secondary: fg("light_foreground", base.text_secondary),
            text_hint: fg("dark_foreground", base.text_hint),
            border,
            panel: bg(panel, base.panel),
            highlight: bg(highlight, base.highlight),
        })
    }
}

impl Default for Theme {
    /// The spec's accent, gold and green from Batroun Noir, and the
    /// terminal's own greys and background for the rest: without a theme
    /// there's no knowing whether the terminal is dark or light.
    fn default() -> Self {
        Self {
            accent: Style::new().fg(Color::Rgb(0xE4, 0x74, 0x4A)),
            gold: Style::new().fg(Color::Rgb(0xE8, 0xB2, 0x5C)),
            green: Style::new().fg(Color::Rgb(0xA8, 0xB3, 0x6A)),
            text_bright: Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            text: Style::new().fg(Color::Reset),
            text_secondary: Style::new().fg(Color::Gray),
            text_hint: Style::new().fg(Color::DarkGray),
            border: Style::new().fg(Color::DarkGray),
            panel: Style::new(),
            highlight: Style::new().add_modifier(Modifier::REVERSED),
        }
    }
}

/// A `colors.toml` that isn't TOML.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeError;

impl fmt::Display for ThemeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the theme's colors.toml isn't TOML")
    }
}

impl std::error::Error for ThemeError {}

/// The file's text, if it can be read, is UTF-8 and is no bigger than
/// [`MAX_COLORS_TOML`].
fn read_small(path: &Path) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut text = String::new();
    file.take(MAX_COLORS_TOML + 1)
        .read_to_string(&mut text)
        .ok()?;
    u64::try_from(text.len())
        .is_ok_and(|len| len <= MAX_COLORS_TOML)
        .then_some(text)
}

/// `#rrggbb` as a colour. Anything else (no `#`, another length, a
/// non-hex digit, `rgba(…)`) is `None`.
fn parse_hex(text: &str) -> Option<Color> {
    let hex = text.trim().strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    Some(Color::Rgb(byte(0)?, byte(2)?, byte(4)?))
}

/// Halfway between two colours. Only RGB colours mix; otherwise `a`.
fn mix(a: Color, b: Color) -> Color {
    blend(a, b, 1, 2)
}

/// `panel` lifted an eighth of the way toward `text`.
fn step(panel: Color, text: Color) -> Color {
    blend(panel, text, 1, 8)
}

/// `a` moved `num/den` of the way toward `b`, per channel. Only RGB
/// colours blend; otherwise `a`.
fn blend(a: Color, b: Color, num: u16, den: u16) -> Color {
    let (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) = (a, b) else {
        return a;
    };
    let channel = |a: u8, b: u8| {
        let (a, b) = (u16::from(a), u16::from(b));
        let moved = if b >= a {
            a + (b - a) * num / den
        } else {
            a - (a - b) * num / den
        };
        u8::try_from(moved).unwrap_or(u8::MAX)
    };
    Color::Rgb(channel(ar, br), channel(ag, bg), channel(ab, bb))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// Batroun Noir's `colors.toml`, as Omarchy ships it.
    const BATROUN_NOIR: &str = r##"
# Batroun Noir — from the Levantine Sunset collection.
mode = "dark"

accent = "#E4744A"
selection = "#272539"
muted = "#2B2B3B"

background = "#080A0E"
lighter_background = "#10121A"

foreground = "#EDE3D6"
dark_foreground = "#6E6478"
light_foreground = "#C9BFB4"
bright_foreground = "#FFF8EE"

hyprland_active_border = "rgba(e4744aee) rgba(e8b25cee) 45deg"

red = "#E4744A"
yellow = "#E8B25C"
green = "#A8B36A"
"##;

    fn fg(r: u8, g: u8, b: u8) -> Style {
        Style::new().fg(Color::Rgb(r, g, b))
    }

    fn bg(r: u8, g: u8, b: u8) -> Style {
        Style::new().bg(Color::Rgb(r, g, b))
    }

    #[test]
    fn a_colors_toml_gives_every_role() {
        let theme = Theme::from_colors_toml(BATROUN_NOIR).unwrap();
        assert_eq!(
            theme,
            Theme {
                accent: fg(0xE4, 0x74, 0x4A),
                gold: fg(0xE8, 0xB2, 0x5C),
                green: fg(0xA8, 0xB3, 0x6A),
                text_bright: fg(0xFF, 0xF8, 0xEE),
                text: fg(0xED, 0xE3, 0xD6),
                text_secondary: fg(0xC9, 0xBF, 0xB4),
                text_hint: fg(0x6E, 0x64, 0x78),
                // Halfway between muted #2B2B3B and dark_foreground #6E6478.
                border: fg(0x4C, 0x47, 0x59),
                panel: bg(0x10, 0x12, 0x1A),
                highlight: bg(0x27, 0x25, 0x39),
            }
        );
    }

    #[test]
    fn a_missing_or_bad_key_keeps_the_default_role() {
        let text = "accent = \"#123456\"\nyellow = \"gold\"\ngreen = 7\n\
                    lighter_background = \"#101010\"\nforeground = \"#909090\"\n";
        let theme = Theme::from_colors_toml(text).unwrap();
        let default = Theme::default();
        assert_eq!(theme.accent, fg(0x12, 0x34, 0x56));
        assert_eq!(theme.gold, default.gold);
        assert_eq!(theme.green, default.green);
        assert_eq!(theme.text_bright, default.text_bright);
        assert_eq!(theme.text, fg(0x90, 0x90, 0x90));
        assert_eq!(theme.border, default.border);
        assert_eq!(theme.panel, bg(0x10, 0x10, 0x10));
        // No selection: the panel lifted an eighth toward the text.
        assert_eq!(theme.highlight, bg(0x20, 0x20, 0x20));
    }

    #[test]
    fn text_that_isnt_toml_is_refused() {
        assert_eq!(Theme::from_colors_toml("accent = "), Err(ThemeError));
        assert_eq!(Theme::from_colors_toml("[[[\n"), Err(ThemeError));
    }

    #[test]
    fn an_empty_file_is_the_default() {
        assert_eq!(Theme::from_colors_toml(""), Ok(Theme::default()));
    }

    #[test]
    fn hex_colours_parse_and_nothing_else_does() {
        assert_eq!(parse_hex("#E4744A"), Some(Color::Rgb(0xE4, 0x74, 0x4A)));
        assert_eq!(parse_hex(" #e4744a "), Some(Color::Rgb(0xE4, 0x74, 0x4A)));
        for bad in [
            "E4744A",
            "#E4744",
            "#E4744AA",
            "#G4744A",
            "rgba(e4744aee)",
            "#+1+2+3",
            "#ééé",
            "",
        ] {
            assert_eq!(parse_hex(bad), None, "{bad}");
        }
    }

    #[test]
    fn blending_moves_part_of_the_way() {
        let (black, white) = (Color::Rgb(0, 0, 0), Color::Rgb(255, 255, 255));
        assert_eq!(mix(black, white), Color::Rgb(127, 127, 127));
        assert_eq!(mix(white, black), Color::Rgb(128, 128, 128));
        assert_eq!(step(black, white), Color::Rgb(31, 31, 31));
        assert_eq!(mix(Color::Reset, white), Color::Reset);
    }

    #[test]
    fn a_missing_file_reads_as_nothing() {
        assert_eq!(read_small(Path::new("/nonexistent/nota/colors.toml")), None);
    }

    /// A home directory with `colors` as its Omarchy theme's file, removed
    /// when dropped.
    struct Home(std::path::PathBuf);

    impl Home {
        #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
        fn with(name: &str, colors: Option<&str>) -> Self {
            let home =
                std::env::temp_dir().join(format!("nota-theme-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&home);
            let dir = home.join(COLORS_TOML);
            std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
            if let Some(colors) = colors {
                std::fs::write(&dir, colors).unwrap();
            }
            Self(home)
        }
    }

    impl Drop for Home {
        #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// What the environment chooses: `NO_COLOR` (when not empty) over the
    /// theme; a missing or malformed file, or no home, gives the default.
    #[test]
    fn the_environment_chooses_the_theme() {
        let themed = Home::with("themed", Some(BATROUN_NOIR));
        let batroun = Theme::from_colors_toml(BATROUN_NOIR).unwrap();
        let no = |v: &str| Some(std::ffi::OsString::from(v));
        assert_eq!(Theme::from_env(None, Some(&themed.0)), batroun);
        assert_eq!(Theme::from_env(no("").as_deref(), Some(&themed.0)), batroun);
        assert_eq!(
            Theme::from_env(no("1").as_deref(), Some(&themed.0)),
            Theme::no_color()
        );
        let missing = Home::with("missing", None);
        assert_eq!(Theme::from_env(None, Some(&missing.0)), Theme::default());
        let broken = Home::with("broken", Some("accent = "));
        assert_eq!(Theme::from_env(None, Some(&broken.0)), Theme::default());
        assert_eq!(Theme::from_env(None, None), Theme::default());
    }

    proptest! {
        /// Whatever the file holds, reading it never panics.
        #[test]
        fn any_text_gives_a_theme_or_an_error(text in ".{0,200}") {
            let _ = Theme::from_colors_toml(&text);
        }

        /// A key with any value gives a theme: the value's colour, or the
        /// default's.
        #[test]
        fn any_colour_value_gives_a_theme(value in "#?[0-9a-fA-FxX ]{0,8}") {
            let text = format!("accent = \"{value}\"\nmuted = \"{value}\"\n");
            let theme = Theme::from_colors_toml(&text).unwrap();
            prop_assert!(
                theme.accent == Theme::default().accent
                    || matches!(theme.accent.fg, Some(Color::Rgb(..)))
            );
        }
    }
}
