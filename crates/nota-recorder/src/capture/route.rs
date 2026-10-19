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
//!   and a default after that is a change again. A default's node swapped
//!   for another within the grace is a change with no loss on the way; a
//!   default cleared and named again within it, its node never gone, is
//!   nothing.
//! - **Pinned** ([`Source::Device`]): the device going away is
//!   [`DeviceChange::Lost`], at once and for good. A pinned track is never
//!   moved to another device quietly: the backend ends its stream instead
//!   ([`Changes::ends_stream`]).
//!
//! A node is known by its serial ([`Node::serial`]), not its name. A
//! Bluetooth headset that changes profile is a device going away and a new
//! one appearing, under another node name or the same one: lost if
//! pinned, a change if followed. A USB microphone unplugged is the same.
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
    /// The server's serial for it, which it never gives another node (it
    /// does reuse ids), or the id on a server too old to give one (the
    /// watch's `serial` says so). A node removed and made again under the
    /// same name, as a Bluetooth headset's is when it changes profile, has
    /// a new one: it's another device, whose audio starts a new epoch.
    pub(super) serial: u64,
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
    /// a node not (or no longer) in the graph is none. The server doesn't
    /// keep node names unique: of several under the name, the one with the
    /// serial `on` (the node the track is on) is it while it's there.
    pub(super) fn device(&self, source: &Source, on: Option<u64>) -> Option<&Node> {
        let (name, class) = match source {
            Source::SystemAudio => (self.default_sink.as_deref()?, Some(Class::Sink)),
            Source::Microphone => (self.default_source.as_deref()?, Some(Class::Source)),
            Source::Device(name) => (name.as_str(), None),
        };
        let named = || {
            self.nodes
                .values()
                .filter(move |node| node.name == name && class.is_none_or(|c| node.class == c))
        };
        named()
            .find(|node| Some(node.serial) == on)
            .or_else(|| named().next())
    }

    /// The sinks and sources the graph holds, and its defaults, as Setup
    /// lists them. Other audio nodes ([`Class::Other`]) aren't offered:
    /// nothing says which way they'd record. A default is named only while
    /// it's listed: the server removes a node a moment before the session
    /// manager names another default, or none.
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
            default_output: self
                .device(&Source::SystemAudio, None)
                .map(|n| n.name.clone()),
            default_input: self
                .device(&Source::Microphone, None)
                .map(|n| n.name.clone()),
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

impl On {
    /// The serial of the node the track is on, or was on before it went
    /// missing.
    const fn serial(&self) -> Option<u64> {
        match self {
            Self::Device(node)
            | Self::Missing {
                was: Some(node), ..
            } => Some(node.serial),
            Self::Unseen | Self::Missing { was: None, .. } | Self::Lost => None,
        }
    }
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
        let serial = self.on.serial();
        let on = std::mem::replace(&mut self.on, On::Unseen);
        let (on, change) = match graph.device(&self.source, serial).cloned() {
            Some(node) => {
                let (on, change) = self.found(on, node);
                (on, change.map(|change| (change, now.at)))
            }
            None => self.missing(on, now),
        };
        self.on = on;
        change.map(|(change, at)| Changes {
            ends_stream: change == DeviceChange::Lost && !self.follows(),
            change,
            at,
        })
    }

    /// What the track is on, and what changed, now that it was `on` and
    /// the graph has `node` for it. The same node is known by its serial:
    /// one made again under the same name is another device.
    fn found(&self, on: On, node: Node) -> (On, Option<DeviceChange>) {
        let follows = self.follows();
        match on {
            On::Lost if !follows => (On::Lost, None),
            On::Unseen | On::Missing { was: None, .. } => (On::Device(node), None),
            On::Device(was) | On::Missing { was: Some(was), .. } if was.serial == node.serial => {
                (On::Device(node), None)
            }
            // The pinned node is gone, and another has its name: one made
            // again between two looks, as only the poll would see it, or
            // another node under the same name. Either way the node the
            // stream was on is gone.
            On::Device(_) if !follows => (On::Lost, Some(DeviceChange::Lost)),
            On::Device(_) | On::Missing { .. } | On::Lost => {
                let change = DeviceChange::Changed(node.description.clone());
                (On::Device(node), Some(change))
            }
        }
    }

    /// What the track is on, and what changed and when, now that it was
    /// `on` and the graph has nothing for it at `now`. A followed default
    /// has [`LOST_GRACE`] to come back; a pinned device is lost at once.
    fn missing(&self, on: On, now: Moment) -> (On, Option<(DeviceChange, SessionTime)>) {
        let follows = self.follows();
        match on {
            On::Device(was) if follows => (
                On::Missing {
                    since: now,
                    was: Some(was),
                },
                None,
            ),
            On::Unseen if follows => (
                On::Missing {
                    since: now,
                    was: None,
                },
                None,
            ),
            On::Missing { since, was } => {
                if now.awake_since(since) < LOST_GRACE {
                    (On::Missing { since, was }, None)
                } else {
                    (On::Lost, Some((DeviceChange::Lost, since.at)))
                }
            }
            On::Lost => (On::Lost, None),
            On::Unseen | On::Device(_) => (On::Lost, Some((DeviceChange::Lost, now.at))),
        }
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
