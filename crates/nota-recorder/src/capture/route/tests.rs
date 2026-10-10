use proptest::prelude::*;

use super::*;

/// How often the fake registry's watch looks again with no event: as the
/// backend's tick does.
const TICK: Duration = Duration::from_millis(250);

/// The time a T4 case allows from what happened to the event (development
/// plan, M2's exit criteria).
const SHOWN_WITHIN: Duration = Duration::from_secs(2);

/// A session time `ms` milliseconds in.
const fn ms(ms: u64) -> SessionTime {
    SessionTime::from_nanos(ms * 1_000_000)
}

fn sink(name: &str, description: &str) -> Node {
    Node {
        name: name.to_owned(),
        description: description.to_owned(),
        class: Class::Sink,
    }
}

fn source(name: &str, description: &str) -> Node {
    Node {
        name: name.to_owned(),
        description: description.to_owned(),
        class: Class::Source,
    }
}

fn default(class: Class, name: &str) -> GraphEvent {
    GraphEvent::Default {
        class,
        name: Some(name.to_owned()),
    }
}

/// A registry the test scripts: events at session times, looked at by a
/// [`Route`] after each event and every [`TICK`] between, as the backend's
/// watch looks.
struct FakeRegistry {
    graph: Graph,
    route: Route,
    now: SessionTime,
    seen: Vec<(SessionTime, Changes)>,
}

impl FakeRegistry {
    /// A graph holding `setup`, already looked at once for `source`, at
    /// session time zero.
    fn new(source: Source, setup: Vec<GraphEvent>) -> Self {
        let mut graph = Graph::default();
        for event in setup {
            graph.apply(event);
        }
        let mut route = Route::new(source);
        assert_eq!(route.look(&graph, ms(0)), None, "the first look");
        Self {
            graph,
            route,
            now: ms(0),
            seen: Vec::new(),
        }
    }

    /// Ticks up to `at`, then applies `event` there and looks.
    fn at(&mut self, at: SessionTime, event: GraphEvent) {
        self.run_to(at);
        self.graph.apply(event);
        self.look();
    }

    /// Ticks up to `to`, looking at each.
    fn run_to(&mut self, to: SessionTime) {
        while let Some(next) = self.now.checked_add(TICK).filter(|&next| next <= to) {
            self.now = next;
            self.look();
        }
        self.now = to;
    }

    fn look(&mut self) {
        if let Some(changes) = self.route.look(&self.graph, self.now) {
            self.seen.push((self.now, changes));
        }
    }

    /// The changes seen, each with how long after `from` it came.
    fn seen_after(&self, from: SessionTime) -> Vec<(Duration, Changes)> {
        self.seen
            .iter()
            .map(|(at, changes)| (at.checked_duration_since(from).unwrap(), changes.clone()))
            .collect()
    }
}

fn changed(description: &str) -> Changes {
    Changes {
        change: DeviceChange::Changed(description.to_owned()),
        ends_stream: false,
    }
}

const LOST_FOR_GOOD: Changes = Changes {
    change: DeviceChange::Lost,
    ends_stream: true,
};

/// Two sinks, speakers the default, and a microphone.
fn desk() -> Vec<GraphEvent> {
    vec![
        GraphEvent::Added {
            id: 40,
            node: sink("alsa_output.speakers", "Speakers"),
        },
        GraphEvent::Added {
            id: 41,
            node: sink("alsa_output.headphones", "Headphones"),
        },
        GraphEvent::Added {
            id: 50,
            node: source("alsa_input.builtin", "Built-in Microphone"),
        },
        default(Class::Sink, "alsa_output.speakers"),
        default(Class::Source, "alsa_input.builtin"),
    ]
}

/// T4: the default output switched. The system audio follows it, and says
/// so with the new device's name at once.
#[test]
fn the_default_output_switched_is_a_change() {
    let mut registry = FakeRegistry::new(Source::SystemAudio, desk());
    let switched = ms(5_000);
    registry.at(switched, default(Class::Sink, "alsa_output.headphones"));
    registry.run_to(ms(10_000));
    assert_eq!(
        registry.seen_after(switched),
        [(Duration::ZERO, changed("Headphones"))]
    );
}

/// T4: a pinned device removed is lost at once, and its stream must end,
/// even though the server still has a default to move it to.
#[test]
fn a_pinned_device_removed_is_lost_for_good() {
    let mut registry = FakeRegistry::new(Source::Device("alsa_output.headphones".into()), desk());
    let removed = ms(3_000);
    registry.at(removed, GraphEvent::Removed { id: 41 });
    // Plugged back in: still lost, the stream is gone.
    registry.at(
        ms(6_000),
        GraphEvent::Added {
            id: 42,
            node: sink("alsa_output.headphones", "Headphones"),
        },
    );
    registry.run_to(ms(10_000));
    assert_eq!(
        registry.seen_after(removed),
        [(Duration::ZERO, LOST_FOR_GOOD)]
    );
}

/// A USB microphone and the built-in one, the USB one the default.
fn with_usb_mic() -> Vec<GraphEvent> {
    let mut setup = desk();
    setup.push(GraphEvent::Added {
        id: 60,
        node: source("alsa_input.usb-mic", "USB Microphone"),
    });
    setup.push(default(Class::Source, "alsa_input.usb-mic"));
    setup
}

/// T4: a USB microphone unplugged while the microphone follows the
/// default. The server names the built-in one the default a moment later:
/// a change, within the 2 s, and no "lost" on the way.
#[test]
fn a_followed_usb_mic_unplugged_changes_to_the_next_default() {
    let mut registry = FakeRegistry::new(Source::Microphone, with_usb_mic());
    let unplugged = ms(2_000);
    registry.at(unplugged, GraphEvent::Removed { id: 60 });
    registry.at(ms(2_300), default(Class::Source, "alsa_input.builtin"));
    registry.run_to(ms(8_000));
    let seen = registry.seen_after(unplugged);
    assert_eq!(
        seen,
        [(Duration::from_millis(300), changed("Built-in Microphone"))]
    );
    assert!(seen[0].0 <= SHOWN_WITHIN);
}

/// T4: a USB microphone pinned and unplugged is lost, never moved to the
/// built-in microphone the server falls back to.
#[test]
fn a_pinned_usb_mic_unplugged_is_lost() {
    let mut registry =
        FakeRegistry::new(Source::Device("alsa_input.usb-mic".into()), with_usb_mic());
    let unplugged = ms(2_000);
    registry.at(unplugged, GraphEvent::Removed { id: 60 });
    registry.at(ms(2_300), default(Class::Source, "alsa_input.builtin"));
    registry.run_to(ms(8_000));
    assert_eq!(
        registry.seen_after(unplugged),
        [(Duration::ZERO, LOST_FOR_GOOD)]
    );
}

/// A Bluetooth headset in its music profile (A2DP), the default output and
/// the default input's sibling.
fn with_headset() -> Vec<GraphEvent> {
    let mut setup = desk();
    setup.push(GraphEvent::Added {
        id: 70,
        node: sink("bluez_output.AA_BB.1", "Headset (A2DP)"),
    });
    setup.push(default(Class::Sink, "bluez_output.AA_BB.1"));
    setup
}

/// The headset switching to its call profile (HSP/HFP): its A2DP node goes,
/// a new node comes under another name, and the server names it the
/// default, as `PipeWire` does.
fn profile_change(registry: &mut FakeRegistry, from: SessionTime) {
    registry.at(from, GraphEvent::Removed { id: 70 });
    registry.at(
        from.checked_add(Duration::from_millis(150)).unwrap(),
        GraphEvent::Added {
            id: 71,
            node: sink("bluez_output.AA_BB.0", "Headset (HFP)"),
        },
    );
    registry.at(
        from.checked_add(Duration::from_millis(400)).unwrap(),
        default(Class::Sink, "bluez_output.AA_BB.0"),
    );
}

/// T4: a Bluetooth profile change while the system audio follows the
/// default: a change to the new node, within the 2 s, with no "lost".
#[test]
fn a_bluetooth_profile_change_is_a_change_when_followed() {
    let mut registry = FakeRegistry::new(Source::SystemAudio, with_headset());
    let switched = ms(4_000);
    profile_change(&mut registry, switched);
    registry.run_to(ms(9_000));
    assert_eq!(
        registry.seen_after(switched),
        [(Duration::from_millis(400), changed("Headset (HFP)"))]
    );
}

/// T4: a Bluetooth profile change while the headset is pinned: the node it
/// was pinned to is gone, so the track is lost, never moved to the new one.
#[test]
fn a_bluetooth_profile_change_is_lost_when_pinned() {
    let mut registry = FakeRegistry::new(
        Source::Device("bluez_output.AA_BB.1".into()),
        with_headset(),
    );
    let switched = ms(4_000);
    profile_change(&mut registry, switched);
    registry.run_to(ms(9_000));
    assert_eq!(
        registry.seen_after(switched),
        [(Duration::ZERO, LOST_FOR_GOOD)]
    );
}

/// A followed default with no device for longer than the grace is lost,
/// within the 2 s, and a default after that is a change again. The
/// stream isn't ended: the server moves it to the new default.
#[test]
fn a_followed_default_that_stays_missing_is_lost_then_changed() {
    let mut registry = FakeRegistry::new(Source::Microphone, desk());
    let removed = ms(1_000);
    registry.at(removed, GraphEvent::Removed { id: 50 });
    registry.at(
        ms(5_000),
        GraphEvent::Added {
            id: 51,
            node: source("alsa_input.webcam", "Webcam"),
        },
    );
    registry.at(ms(5_100), default(Class::Source, "alsa_input.webcam"));
    let seen = registry.seen_after(removed);
    let lost = Changes {
        change: DeviceChange::Lost,
        ends_stream: false,
    };
    assert_eq!(seen[0].1, lost, "{seen:?}");
    assert!((LOST_GRACE..=SHOWN_WITHIN).contains(&seen[0].0), "{seen:?}");
    assert_eq!(
        &seen[1..],
        [(Duration::from_millis(4_100), changed("Webcam"))]
    );
}

/// A followed default that goes and comes back as the same device within
/// the grace is nothing.
#[test]
fn a_default_that_blinks_is_nothing() {
    let mut registry = FakeRegistry::new(Source::SystemAudio, desk());
    registry.at(ms(1_000), GraphEvent::Removed { id: 40 });
    registry.at(
        ms(1_500),
        GraphEvent::Added {
            id: 43,
            node: sink("alsa_output.speakers", "Speakers"),
        },
    );
    registry.run_to(ms(6_000));
    assert_eq!(registry.seen, []);
}

/// The microphone ignores the default output changing, and the system
/// audio the default input: each follows its own.
#[test]
fn each_source_follows_its_own_default() {
    let mut mic = FakeRegistry::new(Source::Microphone, desk());
    mic.at(ms(1_000), default(Class::Sink, "alsa_output.headphones"));
    let mut system = FakeRegistry::new(Source::SystemAudio, with_usb_mic());
    system.at(ms(1_000), default(Class::Source, "alsa_input.builtin"));
    assert_eq!((mic.seen, system.seen), (vec![], vec![]));
}

/// A default naming a node of the other kind isn't the track's device: the
/// system audio never records from a source called the default sink.
#[test]
fn a_default_names_only_its_own_kind() {
    let mut graph = Graph::default();
    graph.apply(GraphEvent::Added {
        id: 1,
        node: source("same-name", "A source"),
    });
    graph.apply(default(Class::Sink, "same-name"));
    assert_eq!(graph.device(&Source::SystemAudio), None);
    assert_eq!(
        graph.device(&Source::Device("same-name".into())),
        Some(&source("same-name", "A source"))
    );
}

/// A track whose device is already gone at the first look is lost then.
#[test]
fn a_device_gone_at_the_first_look_is_lost() {
    let mut route = Route::new(Source::Device("gone".into()));
    assert_eq!(route.look(&Graph::default(), ms(0)), Some(LOST_FOR_GOOD));
}

/// The session manager's `default` values give their node name.
#[test]
fn default_values_give_their_node_name() {
    for (value, name) in [
        (r#"{"name":"alsa_output.pci"}"#, Some("alsa_output.pci")),
        (r#"{ "name": "a b" }"#, Some("a b")),
        (r#"{"other":"x", "name":"n"}"#, Some("n")),
        (r#"{"name":"q\"uote\\d"}"#, Some(r#"q"uote\d"#)),
        (r#"{"name":"tab\tbed"}"#, None),
        (r#"{"name":5}"#, None),
        (r#"{"other":"x"}"#, None),
        (r#""name""#, None),
        ("", None),
    ] {
        assert_eq!(default_name(value).as_deref(), name, "{value}");
    }
    let long = format!(r#"{{"name":"{}"}}"#, "n".repeat(MAX_DEFAULT_VALUE));
    assert_eq!(default_name(&long), None);
}

proptest! {
    /// Any value at all reads as a name or nothing, without panicking.
    #[test]
    fn any_value_reads_without_panicking(value in ".{0,64}") {
        let _ = default_name(&value);
    }

    /// Any name the session manager writes reads back as itself.
    #[test]
    fn written_names_read_back(name in "[^\"\\\\]{0,64}") {
        let value = format!(r#"{{"name":"{name}"}}"#);
        prop_assert_eq!(default_name(&value), Some(name));
    }
}
