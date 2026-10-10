//! Which device each track's stream is on, and what changed.
//!
//! An audio server moves a stream that follows the default whenever the
//! default changes, and may move a stream whose device goes away to some
//! other device without a word. So the backend watches the server's graph
//! itself ([`Graph`]: its sinks and sources, and which are the defaults)
//! and a [`Route`] per track says what the track's [`Source`] is on, and
//! when that changes:
//! - **Follow the default** ([`Source::SystemAudio`],
//!   [`Source::Microphone`]): a new default is
//!   [`DeviceChange::Changed`], with the device's name as the user knows
//!   it. No default at all for [`LOST_GRACE`] is [`DeviceChange::Lost`],
//!   and a default after that is a change again. A default that goes
//!   away and comes back within the grace, as the server swaps one node
//!   for another, is nothing.
//! - **Pinned** ([`Source::Device`]): the device going away is
//!   [`DeviceChange::Lost`], at once and for good. A pinned track is never
//!   moved to another device quietly: the backend ends its stream instead
//!   ([`Changes::ends_stream`]).
//!
//! A Bluetooth headset that changes profile is a device going away and a
//! new one (another node name) appearing: lost if pinned, a change if
//! followed. A USB microphone unplugged is the same.
//!
//! Nothing here reads the server: [`GraphEvent`]s come from the backend's
//! watch (`PipeWire`'s registry and its `default` metadata), and from its
//! poll, which reads the whole graph again in case an event was missed.

use std::collections::BTreeMap;
use std::time::Duration;

use nota_core::SessionTime;
use nota_core::recorder::DeviceChange;

use super::Source;
use super::devices::{Device, Devices};

/// How long a followed default may be missing before it's lost: 1 s. When
/// a device is swapped for another (a Bluetooth profile change, a USB
/// device re-enumerated), the old node goes before the server names the
/// new default, a moment later; that isn't worth "lost". It's well inside
/// the 2 s in which a change must show (development plan, T4).
pub(super) const LOST_GRACE: Duration = Duration::from_secs(1);

/// What a node of the graph is, as far as a track cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Class {
    /// It plays audio: a sink, whose monitor the system audio captures.
    Sink,
    /// It records audio: a source, such as a microphone.
    Source,
    /// Some other audio node, a duplex device say: it can be pinned, but
    /// is never the default a track follows.
    Other,
}

/// A sink or source in the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Node {
    /// The server's name for it, as a [`Source::Device`] names it.
    pub(super) name: String,
    /// Its name as the user knows it ("Headphones"); its node name if it
    /// has none.
    pub(super) description: String,
    /// What it is.
    pub(super) class: Class,
}

/// Something that changed in the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GraphEvent {
    /// A node appeared, under the server's id `id`.
    Added {
        /// The server's id for it.
        id: u32,
        /// The node.
        node: Node,
    },
    /// The object with the server's id `id` went away. It may not be a
    /// node this graph knows.
    Removed {
        /// The server's id for it.
        id: u32,
    },
    /// The default sink or source is now the node named `name`, or there's
    /// none.
    Default {
        /// Which default.
        class: Class,
        /// The node it names, if any.
        name: Option<String>,
    },
}

/// The audio server's sinks and sources, and its defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Graph {
    /// Each node, by the server's id.
    nodes: BTreeMap<u32, Node>,
    /// The default sink's node name.
    default_sink: Option<String>,
    /// The default source's node name.
    default_source: Option<String>,
}

impl Graph {
    /// Takes in `event`.
    pub(super) fn apply(&mut self, event: GraphEvent) {
        match event {
            GraphEvent::Added { id, node } => {
                self.nodes.insert(id, node);
            }
            GraphEvent::Removed { id } => {
                self.nodes.remove(&id);
            }
            GraphEvent::Default { class, name } => match class {
                Class::Sink => self.default_sink = name,
                Class::Source => self.default_source = name,
                Class::Other => {}
            },
        }
    }

    /// The node `source` captures now, if there is one: the default sink or
    /// source it follows, or the node it's pinned to. A default that names
    /// a node not (or no longer) in the graph is none.
    pub(super) fn device(&self, source: &Source) -> Option<&Node> {
        let (name, class) = match source {
            Source::SystemAudio => (self.default_sink.as_deref()?, Some(Class::Sink)),
            Source::Microphone => (self.default_source.as_deref()?, Some(Class::Source)),
            Source::Device(name) => (name.as_str(), None),
        };
        self.nodes
            .values()
            .find(|node| node.name == name && class.is_none_or(|class| node.class == class))
    }

    /// The sinks and sources the graph holds, and its defaults, as Setup
    /// lists them. Other audio nodes ([`Class::Other`]) aren't offered:
    /// nothing says which way they'd record.
    pub(super) fn devices(&self) -> Devices {
        let of = |class| {
            let mut list: Vec<Device> = self
                .nodes
                .values()
                .filter(|node| node.class == class)
                .map(|node| Device {
                    name: node.name.clone(),
                    description: node.description.clone(),
                })
                .collect();
            list.sort_by(|a, b| (&a.description, &a.name).cmp(&(&b.description, &b.name)));
            list
        };
        Devices {
            outputs: of(Class::Sink),
            inputs: of(Class::Source),
            default_output: self.default_sink.clone(),
            default_input: self.default_source.clone(),
        }
    }
}

/// What a track's source is on, as its [`Route`] last saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum On {
    /// Not looked at yet.
    Unseen,
    /// This device.
    Device(Node),
    /// A followed default that went missing at `since`; it was `was`, or
    /// had none yet at the first look.
    Missing {
        /// When it went missing.
        since: Moment,
        /// The device it was on.
        was: Option<Node>,
    },
    /// No device: lost. For a pinned track, for good.
    Lost,
}

/// A moment a route looks at: the session time, and the clock's count of
/// time spent suspended then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Moment {
    /// The session time.
    pub(super) at: SessionTime,
    /// The time spent suspended, by then.
    pub(super) asleep: Duration,
}

impl Moment {
    /// How long the machine was awake from `earlier` to this moment.
    fn awake_since(self, earlier: Self) -> Duration {
        let waited = self
            .at
            .checked_duration_since(earlier.at)
            .unwrap_or_default();
        waited.saturating_sub(self.asleep.saturating_sub(earlier.asleep))
    }
}

/// One track's source, followed through the graph.
#[derive(Debug)]
pub(super) struct Route {
    /// What the track captures.
    source: Source,
    /// What it's on.
    on: On,
}

/// What a [`Route`] saw change, and what the backend must do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Changes {
    /// What happened to the track's device.
    pub(super) change: DeviceChange,
    /// Whether the track's stream must end: its pinned device went away,
    /// and whatever the server moves it to isn't the track's.
    pub(super) ends_stream: bool,
    /// When it happened: the look's time, or for a followed default lost
    /// after its grace, when it went missing.
    pub(super) at: SessionTime,
}

impl Route {
    /// A route for `source`, not yet looked at.
    pub(super) const fn new(source: Source) -> Self {
        Self {
            source,
            on: On::Unseen,
        }
    }

    /// What the track captures.
    pub(super) const fn source(&self) -> &Source {
        &self.source
    }

    /// Whether `source` follows a default rather than one device.
    const fn follows(&self) -> bool {
        !matches!(self.source, Source::Device(_))
    }

    /// Looks at `graph` at `now`, and says what changed for the track since
    /// the last look. The first look says nothing unless the device is
    /// already gone: the stream has just opened on it. Time spent suspended
    /// doesn't count towards a missing default's grace: a device swapped
    /// as the machine slept is named again as it wakes.
    pub(super) fn look(&mut self, graph: &Graph, now: Moment) -> Option<Changes> {
        let device = graph.device(&self.source).cloned();
        let follows = self.follows();
        let mut at = now.at;
        let (on, change) = match (std::mem::replace(&mut self.on, On::Unseen), device) {
            (On::Lost, _) if !follows => (On::Lost, None),
            (On::Unseen | On::Missing { was: None, .. }, Some(node)) => (On::Device(node), None),
            (On::Device(was) | On::Missing { was: Some(was), .. }, Some(node))
                if was.name == node.name =>
            {
                (On::Device(node), None)
            }
            (On::Device(_) | On::Missing { .. } | On::Lost, Some(node)) => {
                let change = DeviceChange::Changed(node.description.clone());
                (On::Device(node), Some(change))
            }
            (On::Device(was), None) if follows => {
                let missing = On::Missing {
                    since: now,
                    was: Some(was),
                };
                (missing, None)
            }
            (On::Unseen, None) if follows => (
                On::Missing {
                    since: now,
                    was: None,
                },
                None,
            ),
            (On::Missing { since, was }, None) => {
                if now.awake_since(since) < LOST_GRACE {
                    (On::Missing { since, was }, None)
                } else {
                    at = since.at;
                    (On::Lost, Some(DeviceChange::Lost))
                }
            }
            (On::Lost, None) => (On::Lost, None),
            (On::Unseen | On::Device(_), None) => (On::Lost, Some(DeviceChange::Lost)),
        };
        self.on = on;
        change.map(|change| Changes {
            ends_stream: change == DeviceChange::Lost && !follows,
            change,
            at,
        })
    }
}

/// The longest `default` metadata value read: 4 KiB. A value names one
/// node, well under that.
const MAX_DEFAULT_VALUE: usize = 4_096;

/// The node name in a `default` metadata value, `{"name":"…"}` as
/// `PipeWire`'s session manager writes it. `None` if the value isn't one,
/// or is longer than [`MAX_DEFAULT_VALUE`].
pub(super) fn default_name(value: &str) -> Option<String> {
    if value.len() > MAX_DEFAULT_VALUE {
        return None;
    }
    let mut rest = value.trim_start().strip_prefix('{')?;
    loop {
        let (key, after) = json_string(rest.trim_start())?;
        let after = after.trim_start().strip_prefix(':')?;
        let (text, after) = json_string(after.trim_start())?;
        if key == "name" {
            return Some(text);
        }
        rest = after.trim_start().strip_prefix(',')?;
    }
}

/// A JSON string at the start of `text`, unescaped, and what follows it.
/// Only the escapes a node name could hold are read (`\"`, `\\`, `/`);
/// any other, such as `\t` or `\u`, makes it unreadable.
fn json_string(text: &str) -> Option<(String, &str)> {
    let mut chars = text.strip_prefix('"')?.char_indices();
    let body = &text[1..];
    let mut out = String::new();
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => return Some((out, &body[at + 1..])),
            '\\' => match chars.next()?.1 {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                _ => return None,
            },
            c => out.push(c),
        }
    }
    None
}

#[cfg(test)]
mod tests;
