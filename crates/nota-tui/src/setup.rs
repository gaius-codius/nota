//! The Setup screen (the UI spec's `SetupM2`): the title, what to listen
//! to with a live level meter for each source, the engines, and the space
//! left.
//!
//! ```text
//! ╭─ ≈ nota · new recording ─────────────────── last settings ─╮
//! │                                                            │
//! │   Title      9 Oct, 14:05                                  │
//! │                                                            │
//! │ ▸ Listen to  pick the one that moves                       │
//! │              ● System audio  Speakers · default  ▅▂▁▄▇▆▂▁  │
//! │              ○ Microphone    Seiren Mini         ▁▁▁▁▁▁▁▁  │
//! │              ○ Both                                        │
//! │                                                            │
//! │              Records everything playing on Speakers,       │
//! │              notifications included.                       │
//! │                                                            │
//! │   Engines    parakeet, live and after the stop             │
//! │              ✓ everything stays on this machine            │
//! │                                                            │
//! │              ⚠ space for about 2h 10m of recording         │
//! ╰─ ⏎ start  tab next  ↑↓ source  ←→ device  esc back ─ R skips this ─╯
//! ```
//!
//! Both sources' meters move whichever is chosen: the recorder listens to
//! each in a preview, with nothing written. The screen only draws what it's
//! given ([`Setup::set_level`], [`Setup::set_devices`]) and says what the
//! user chose ([`Setup::chosen`]); it opens no stream itself.
//!
//! A source names its device, never an app. It follows the system's
//! default until `←→` pins a named device; "default" is one of the choices.
//!
//! `⏎` starts, `esc` goes back to Home, and Ctrl+C closes nota. The footer
//! offers only keys that work in the field the cursor is on.

use std::time::Duration;

use nota_core::recorder::{Input, Level};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::home::duration;
use crate::level::bar;
use crate::render::{Keep, MIN_HEIGHT, MIN_WIDTH, frame_row, too_small, truncate};
use crate::text::{display_width, is_drawn, wrap};
use crate::theme::Theme;

/// The longest title, in characters. A title is one line; this only keeps a
/// stuck key from growing it without bound.
const MAX_TITLE_CHARS: usize = 120;

/// How many recent levels a meter shows, one bar each.
const METER_BARS: usize = 8;

/// The space line shows only below this much recording time left.
const SPACE_SHOWN_BELOW: Duration = Duration::from_hours(4);

/// Below this much time left the space line is in the accent colour
/// rather than gold.
const SPACE_URGENT_BELOW: Duration = Duration::from_hours(1);

/// The column inside the frame where each field's content starts.
const CONTENT_COL: u16 = 14;

/// Columns a source's name takes in its row: `● System audio`.
const NAME_WIDTH: usize = 14;

/// The columns of a meter, the gap before it, and the frame's padding after
/// it.
const METER_ROOM: usize = METER_BARS + 2 + 2;

/// A source the recorder can listen to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// What the output plays.
    System,
    /// The room, through an input.
    Microphone,
}

/// What to listen to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listen {
    /// The system audio only.
    System,
    /// The microphone only.
    Microphone,
    /// Both, as two tracks.
    Both,
}

impl Listen {
    /// The three choices, in the order they're listed.
    const ALL: [Self; 3] = [Self::System, Self::Microphone, Self::Both];

    /// The source whose device `←→` changes while this is chosen: none for
    /// both, which has no device of its own.
    const fn source(self) -> Option<Source> {
        match self {
            Self::System => Some(Source::System),
            Self::Microphone => Some(Source::Microphone),
            Self::Both => None,
        }
    }

    /// Whether `source` is recorded when this is chosen.
    #[must_use]
    pub const fn records(self, source: Source) -> bool {
        matches!(
            (self, source),
            (Self::Both, _)
                | (Self::System, Source::System)
                | (Self::Microphone, Source::Microphone)
        )
    }
}

/// A device the audio server has, as the screen lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// The audio server's name for it, which pins it.
    pub name: String,
    /// Its name as the user knows it (`Speakers`).
    pub description: String,
}

/// The audio server's devices, and which are its defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Devices {
    /// The outputs: capturing one records what plays on it.
    pub outputs: Vec<Device>,
    /// The inputs, such as microphones.
    pub inputs: Vec<Device>,
    /// The default output's name.
    pub default_output: Option<String>,
    /// The default input's name.
    pub default_input: Option<String>,
}

impl Devices {
    /// The devices a source can be pinned to.
    fn of(&self, source: Source) -> &[Device] {
        match source {
            Source::System => &self.outputs,
            Source::Microphone => &self.inputs,
        }
    }

    /// The name of the device `source` follows by default.
    fn default_of(&self, source: Source) -> Option<&str> {
        match source {
            Source::System => self.default_output.as_deref(),
            Source::Microphone => self.default_input.as_deref(),
        }
    }
}

/// What the user chose, once `⏎` starts the recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// The session's title: what was typed, or the pre-filled one if it was
    /// cleared.
    pub title: String,
    /// What to listen to.
    pub listen: Listen,
    /// Where the system audio is recorded from.
    pub system: Input,
    /// Where the microphone is recorded from.
    pub microphone: Input,
}

/// What Setup asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupAction {
    /// `⏎`: start recording as [`Setup::chosen`] says.
    Start,
    /// `esc`: back to Home.
    Back,
    /// Ctrl+C: close nota.
    Quit,
}

/// The field the cursor is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    /// The title, which takes typing.
    Title,
    /// The sources: `↑↓` picks one, `←→` its device.
    Listen,
}

/// The last few levels one source's preview heard.
#[derive(Debug, Clone, Copy, Default)]
struct Meter {
    /// Oldest first; none before the first level arrives.
    bars: [Option<Level>; METER_BARS],
    /// Whether the source's stream couldn't be opened or failed.
    failed: bool,
}

impl Meter {
    /// Adds `level` as the newest bar, dropping the oldest. A level means
    /// the stream is running.
    fn push(&mut self, level: Level) {
        self.bars.rotate_left(1);
        self.bars[METER_BARS - 1] = Some(level);
        self.failed = false;
    }
}

/// The Setup screen.
#[derive(Debug)]
pub struct Setup {
    /// The title as typed.
    title: String,
    /// The title it was pre-filled with, kept for a cleared one.
    default_title: String,
    field: Field,
    listen: Listen,
    /// The device each source is pinned to, or the default.
    system: Input,
    microphone: Input,
    devices: Devices,
    meters: [Meter; 2],
    engines: String,
    /// How much recording the disk has room for, if known.
    space: Option<Duration>,
    /// Whether the choices came from the last recording.
    remembered: bool,
    theme: Theme,
}

impl Setup {
    /// A Setup screen pre-filled with `title`, listening to the system audio
    /// on the default devices, naming `engines` (the line under Engines).
    /// Control and bidirectional formatting characters are dropped from
    /// both, so they can't reorder or break a row.
    #[must_use]
    pub fn new(title: &str, engines: &str, theme: Theme) -> Self {
        let title = drawn(title);
        Self {
            default_title: title.clone(),
            title,
            field: Field::Listen,
            listen: Listen::System,
            system: Input::Default,
            microphone: Input::Default,
            devices: Devices::default(),
            meters: [Meter::default(); 2],
            engines: drawn(engines),
            space: None,
            remembered: false,
            theme,
        }
    }

    /// Starts from the last recording's choices, which the top border says.
    #[must_use]
    pub fn remembering(mut self, listen: Listen, system: Input, microphone: Input) -> Self {
        self.listen = listen;
        self.system = system;
        self.microphone = microphone;
        self.remembered = true;
        self
    }

    /// Lists `devices` as what `←→` chooses between.
    pub fn set_devices(&mut self, devices: Devices) {
        self.devices = devices;
    }

    /// Shows `level` as `source`'s newest bar.
    pub fn set_level(&mut self, source: Source, level: Level) {
        self.meters[meter_index(source)].push(level);
    }

    /// Says that `source`'s stream couldn't be opened, or failed: its meter
    /// shows that instead of bars, until a level arrives.
    pub fn set_failed(&mut self, source: Source) {
        self.meters[meter_index(source)].failed = true;
    }

    /// Shows how much recording the disk has room for, or nothing if it
    /// isn't known. It shows only under four hours.
    pub fn set_space(&mut self, left: Option<Duration>) {
        self.space = left;
    }

    /// What the user has chosen so far.
    #[must_use]
    pub fn chosen(&self) -> Choice {
        let title = self.title.trim();
        Choice {
            title: if title.is_empty() {
                self.default_title.clone()
            } else {
                title.to_owned()
            },
            listen: self.listen,
            system: self.system.clone(),
            microphone: self.microphone.clone(),
        }
    }

    /// The input each source listens to now, for the preview: both, whichever
    /// is chosen, so every meter moves.
    #[must_use]
    pub fn inputs(&self) -> [(Source, Input); 2] {
        [
            (Source::System, self.system.clone()),
            (Source::Microphone, self.microphone.clone()),
        ]
    }

    /// How many tracks the choice records: one for a source, two for both.
    #[must_use]
    pub const fn tracks(&self) -> usize {
        match self.listen {
            Listen::Both => 2,
            Listen::System | Listen::Microphone => 1,
        }
    }

    /// Handles a key press. Returns what Setup asks for, if anything.
    ///
    /// - `⏎` starts, `esc` goes back and Ctrl+C closes nota, from either
    ///   field.
    /// - `tab` (and `shift+tab`) moves between the title and the sources.
    /// - On the title, typing fills it and `backspace` deletes.
    /// - On the sources, `↑↓` picks System audio, Microphone or Both, and
    ///   `←→` picks the chosen source's device, the default among them.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<SetupAction> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c' | 'C')) {
            return Some(SetupAction::Quit);
        }
        // A key held with Ctrl or Alt is another key.
        if ctrl || key.modifiers.contains(KeyModifiers::ALT) {
            return None;
        }
        match key.code {
            KeyCode::Enter => return Some(SetupAction::Start),
            KeyCode::Esc => return Some(SetupAction::Back),
            KeyCode::Tab | KeyCode::BackTab => self.field = self.field.other(),
            _ => match self.field {
                Field::Title => self.type_key(key.code),
                Field::Listen => self.listen_key(key.code),
            },
        }
        None
    }

    /// Adds pasted `text` to the title, as a single line: breaks and tabs
    /// become spaces, other control characters are dropped, and what passes
    /// the length limit is cut off. Dropped unless the title has the cursor.
    pub fn paste(&mut self, text: &str) {
        if self.field != Field::Title {
            return;
        }
        for c in text.chars() {
            let c = if c.is_whitespace() { ' ' } else { c };
            if is_drawn(c) {
                self.push_title(c);
            }
        }
    }

    /// Types `code` into the title.
    fn type_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Backspace => {
                self.title.pop();
            }
            KeyCode::Char(c) if is_drawn(c) => self.push_title(c),
            _ => {}
        }
    }

    /// Adds `c` to the title, unless it's at its limit.
    fn push_title(&mut self, c: char) {
        if self.title.chars().count() < MAX_TITLE_CHARS {
            self.title.push(c);
        }
    }

    /// Moves between the sources, or between the chosen source's devices.
    fn listen_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.move_listen(Step::Back),
            KeyCode::Down | KeyCode::Char('j') => self.move_listen(Step::Forward),
            KeyCode::Left | KeyCode::Char('h') => self.move_device(Step::Back),
            KeyCode::Right | KeyCode::Char('l') => self.move_device(Step::Forward),
            _ => {}
        }
    }

    /// Chooses the source above or below, without wrapping.
    fn move_listen(&mut self, step: Step) {
        let at = Listen::ALL
            .iter()
            .position(|&l| l == self.listen)
            .unwrap_or(0);
        let to = match step {
            Step::Back => at.saturating_sub(1),
            Step::Forward => (at + 1).min(Listen::ALL.len() - 1),
        };
        self.listen = Listen::ALL[to];
    }

    /// Pins the chosen source to the device before or after its current
    /// one in the list, with "default" first, wrapping round.
    fn move_device(&mut self, step: Step) {
        let Some(source) = self.listen.source() else {
            return;
        };
        let choices = self.choices(source);
        let at = choices
            .iter()
            .position(|choice| *choice == *self.input(source))
            .unwrap_or(0);
        let last = choices.len() - 1;
        let to = match step {
            Step::Back if at == 0 => last,
            Step::Back => at - 1,
            Step::Forward if at == last => 0,
            Step::Forward => at + 1,
        };
        *self.input_mut(source) = choices[to].clone();
    }

    /// What `←→` chooses between for `source`: the default, then each
    /// device, then the device it's pinned to if the list no longer has it
    /// (so it can be chosen away from).
    fn choices(&self, source: Source) -> Vec<Input> {
        let mut choices = vec![Input::Default];
        choices.extend(
            self.devices
                .of(source)
                .iter()
                .map(|device| Input::Device(device.name.clone())),
        );
        if !choices.contains(self.input(source)) {
            choices.push(self.input(source).clone());
        }
        choices
    }

    /// Where `source` is recorded from.
    const fn input(&self, source: Source) -> &Input {
        match source {
            Source::System => &self.system,
            Source::Microphone => &self.microphone,
        }
    }

    /// Where `source` is recorded from, to change it.
    const fn input_mut(&mut self, source: Source) -> &mut Input {
        match source {
            Source::System => &mut self.system,
            Source::Microphone => &mut self.microphone,
        }
    }

    /// The device `source` records from now, as the user knows it: the
    /// description of the default or pinned device, or its name if the list
    /// doesn't have it, or nothing if there's no such device.
    fn description(&self, source: Source) -> Option<String> {
        let name = match self.input(source) {
            Input::Default => self.devices.default_of(source)?,
            Input::Device(name) => name,
        };
        let known = self
            .devices
            .of(source)
            .iter()
            .find(|device| device.name == *name);
        Some(known.map_or_else(|| drawn(name), |device| drawn(&device.description)))
    }

    /// `Speakers · default`, or the pinned device's name.
    fn device_label(&self, source: Source) -> String {
        let name = self.description(source).unwrap_or_else(|| match source {
            Source::System => "no output".to_owned(),
            Source::Microphone => "no microphone".to_owned(),
        });
        match self.input(source) {
            Input::Default if self.description(source).is_some() => format!("{name} · default"),
            Input::Default | Input::Device(_) => name,
        }
    }

    /// The plain words about what the chosen source records, one line of
    /// text per source.
    fn plain_words(&self) -> Vec<String> {
        let words = |source| match source {
            Source::System => format!(
                "Records everything playing on {}, notifications included.",
                self.description(source)
                    .unwrap_or_else(|| "the output".to_owned())
            ),
            Source::Microphone => format!(
                "Records the room through {}.",
                self.description(source)
                    .unwrap_or_else(|| "the microphone".to_owned())
            ),
        };
        match self.listen {
            Listen::System => vec![words(Source::System)],
            Listen::Microphone => vec![words(Source::Microphone)],
            Listen::Both => vec![words(Source::System), words(Source::Microphone)],
        }
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
        self.draw_fields(inside, buf);
        self.draw_bottom(area, buf);
    }

    fn draw_top(&self, area: Rect, buf: &mut Buffer) {
        let right = if self.remembered {
            vec![Span::styled("last settings", self.theme.text_secondary)]
        } else {
            Vec::new()
        };
        frame_row(
            Rect::new(area.x, area.y, area.width, 1),
            ('╭', '╮'),
            vec![Span::styled("≈ nota · new recording", self.theme.accent)],
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
        let mut left = Vec::new();
        left.extend(key("⏎", " start"));
        left.extend(key("  tab", " next"));
        if self.field == Field::Listen {
            left.extend(key("  ↑↓", " source"));
            if self.listen.source().is_some() {
                left.extend(key("  ←→", " device"));
            }
        }
        left.extend(key("  esc", " back"));
        // The hint about Home's key is dropped rather than cut short.
        let hint = "R skips this";
        let room = usize::from(area.width).saturating_sub(crate::render::FRAME_ROW_FIXED + 1);
        let right = if crate::render::spans_width(&left) + display_width(hint) < room {
            vec![Span::styled(hint, self.theme.text_hint)]
        } else {
            Vec::new()
        };
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

    /// Every field, top to bottom, from the frame's inside.
    fn draw_fields(&self, inside: Rect, buf: &mut Buffer) {
        let width = usize::from(inside.width);
        let mut y = inside.y + 1;
        let mut row = |y: &mut u16, line: Line<'_>| {
            if *y < inside.bottom() {
                buf.set_line(inside.x, *y, &line, inside.width);
            }
            *y += 1;
        };
        row(&mut y, self.title_line(width));
        y += 1;
        row(&mut y, self.listen_heading());
        for listen in Listen::ALL {
            row(&mut y, self.listen_row(listen, width));
        }
        y += 1;
        let room = width.saturating_sub(usize::from(CONTENT_COL) + 2);
        for words in self.plain_words() {
            for text in wrap(&words, room) {
                row(&mut y, Self::indented(text, self.theme.text_secondary));
            }
        }
        y += 1;
        row(&mut y, self.engines_line(width));
        row(
            &mut y,
            Line::from(vec![
                Span::raw(" ".repeat(usize::from(CONTENT_COL))),
                Span::styled("✓ ", self.theme.green),
                Span::styled(
                    "everything stays on this machine",
                    self.theme.text_secondary,
                ),
            ]),
        );
        if let Some(line) = self.space_line() {
            y += 1;
            row(&mut y, line);
        }
    }

    /// `text` at the content column, in `style`.
    fn indented(text: String, style: ratatui::style::Style) -> Line<'static> {
        Line::from(vec![
            Span::raw(" ".repeat(usize::from(CONTENT_COL))),
            Span::styled(text, style),
        ])
    }

    /// The marker column: `▸` on the field the cursor is on.
    fn marker(&self, field: Field) -> Span<'static> {
        if self.field == field {
            Span::styled(" ▸ ", self.theme.accent)
        } else {
            Span::raw("   ")
        }
    }

    /// `  Title      9 Oct, 14:05`, with a cursor while it takes typing.
    fn title_line(&self, width: usize) -> Line<'static> {
        let cursor = if self.field == Field::Title {
            "▏"
        } else {
            ""
        };
        let room = width.saturating_sub(usize::from(CONTENT_COL) + 2);
        let shown = truncate(vec![Span::raw(format!("{}{cursor}", self.title))], room);
        let mut spans = vec![
            self.marker(Field::Title),
            Span::styled("Title", self.theme.text_secondary),
            Span::raw("      "),
        ];
        spans.extend(
            shown
                .into_iter()
                .map(|s| Span::styled(s.content.into_owned(), self.theme.text_bright)),
        );
        Line::from(spans)
    }

    /// `▸ Listen to  pick the one that moves`.
    fn listen_heading(&self) -> Line<'static> {
        Line::from(vec![
            self.marker(Field::Listen),
            Span::styled("Listen to", self.theme.text_secondary),
            Span::raw("  "),
            Span::styled("pick the one that moves", self.theme.text_hint),
        ])
    }

    /// `● System audio  Speakers · default  ▅▂▁▄▇▆▂▁`, `width` columns
    /// wide.
    fn listen_row(&self, listen: Listen, width: usize) -> Line<'static> {
        let chosen = self.listen == listen;
        let (name, source) = match listen {
            Listen::System => ("System audio", Some(Source::System)),
            Listen::Microphone => ("Microphone", Some(Source::Microphone)),
            Listen::Both => ("Both", None),
        };
        let radio = if chosen { "●" } else { "○" };
        let name_style = if chosen {
            self.theme.text_bright
        } else {
            self.theme.text
        };
        let mut spans = vec![
            Span::raw(" ".repeat(usize::from(CONTENT_COL))),
            Span::styled(
                format!("{radio} {name:<pad$}", pad = NAME_WIDTH - 2),
                name_style,
            ),
        ];
        if let Some(source) = source {
            spans.extend(self.device_and_meter(source, width));
        }
        Line::from(spans)
    }

    /// The device `source` records from and its meter. The device is cut to
    /// leave the meter its room.
    fn device_and_meter(&self, source: Source, width: usize) -> Vec<Span<'static>> {
        let room = width.saturating_sub(usize::from(CONTENT_COL) + NAME_WIDTH + 2 + METER_ROOM);
        let label = truncate(vec![Span::raw(self.device_label(source))], room);
        let label_width: usize = label.iter().map(|s| display_width(&s.content)).sum();
        // Both rows' labels are padded alike, so the meters line up.
        let column = [Source::System, Source::Microphone]
            .map(|s| display_width(&self.device_label(s)))
            .into_iter()
            .max()
            .unwrap_or(0)
            .min(room);
        let mut spans = vec![Span::raw("  ")];
        spans.extend(
            label
                .into_iter()
                .map(|s| Span::styled(s.content.into_owned(), self.theme.text_secondary)),
        );
        spans.push(Span::raw(
            " ".repeat(column.saturating_sub(label_width) + 2),
        ));
        spans.push(self.meter(source));
        spans
    }

    /// The bars of `source`'s recent levels, or why there are none.
    fn meter(&self, source: Source) -> Span<'static> {
        let meter = &self.meters[meter_index(source)];
        if meter.failed {
            return Span::styled("⚠ no signal", self.theme.gold);
        }
        let bars: String = meter
            .bars
            .iter()
            .map(|level| level.map_or('▁', bar))
            .collect();
        let style = if self.listen.records(source) {
            self.theme.accent
        } else {
            self.theme.text_hint
        };
        Span::styled(bars, style)
    }

    /// `  Engines    parakeet, live and after the stop`.
    fn engines_line(&self, width: usize) -> Line<'static> {
        let room = width.saturating_sub(usize::from(CONTENT_COL) + 2);
        let shown = truncate(vec![Span::raw(self.engines.clone())], room);
        let mut spans = vec![
            Span::raw("   "),
            Span::styled("Engines", self.theme.text_secondary),
            Span::raw("    "),
        ];
        spans.extend(
            shown
                .into_iter()
                .map(|s| Span::styled(s.content.into_owned(), self.theme.text)),
        );
        Line::from(spans)
    }

    /// `⚠ space for about 2h 10m of recording`, when the disk has less than
    /// four hours of room: gold, and the accent colour under an hour.
    fn space_line(&self) -> Option<Line<'static>> {
        let left = self.space.filter(|left| *left < SPACE_SHOWN_BELOW)?;
        let style = if left < SPACE_URGENT_BELOW {
            self.theme.accent
        } else {
            self.theme.gold
        };
        Some(Self::indented(
            format!("⚠ space for about {} of recording", duration(left)),
            style,
        ))
    }
}

/// Which way `←→` or `↑↓` moves.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// To the one before.
    Back,
    /// To the one after.
    Forward,
}

impl Field {
    /// The other field: there are two.
    const fn other(self) -> Self {
        match self {
            Self::Title => Self::Listen,
            Self::Listen => Self::Title,
        }
    }
}

/// Where `source`'s meter is.
const fn meter_index(source: Source) -> usize {
    match source {
        Source::System => 0,
        Source::Microphone => 1,
    }
}

/// `text` without the characters that would reorder or break a row.
fn drawn(text: &str) -> String {
    text.chars().filter(|&c| is_drawn(c)).collect()
}

#[cfg(test)]
mod tests;
