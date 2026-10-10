//! The watch on `PipeWire`'s graph for one track's stream: which device
//! its [`Source`] is on, reported as [`DeviceChange`]s ([`super::super::route`]
//! decides what counts as one).
//!
//! The watch runs a `PipeWire` main loop on a thread of its own, with its
//! own connection. It reads the graph from the registry (the sinks,
//! sources and other audio nodes) and from the `default` metadata the
//! session manager keeps (`default.audio.sink`, `default.audio.source`),
//! and the registry's events keep that up to date as devices come and go.
//! As a backstop, every [`POLL`] it subscribes afresh and, once that
//! fresh copy has caught up, takes it in place of the old one, so an event
//! the old subscription missed is caught within about a second. The route
//! is looked at after every event and every [`TICK`], which is what turns
//! a default missing for longer than its grace into a loss.
//!
//! A watch that can't connect, or whose connection fails, says so once as a
//! [`CaptureNotice::Warning`]; the stream records on without it, and the
//! stalled detector still notices a stream that stops.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use pipewire as pw;
use pw::context::ContextRc;
use pw::core::{CoreRc, PW_ID_CORE};
use pw::main_loop::MainLoopRc;
use pw::metadata::{Metadata, MetadataListener};
use pw::registry::{GlobalObject, Listener, RegistryRc};
use pw::spa::utils::dict::DictRef;
use pw::spa::utils::result::AsyncSeq;
use pw::types::ObjectType;

use super::super::route::{self, Class, Graph, GraphEvent, Node, Route};
use super::super::{CaptureError, CaptureNotice, CaptureSender, Source};

/// How often the route is looked at with no event: 250 ms, so a default
/// missing for its 1 s grace is lost within about 1.25 s, inside the 2 s a
/// change must show in.
const TICK: Duration = Duration::from_millis(250);

/// How often the graph is read afresh, in case the registry's events
/// missed something: 1 s. Reading it is a few local messages.
const POLL: Duration = Duration::from_secs(1);

/// The `default` metadata's name, and its keys for the default sink and
/// source.
const DEFAULT_METADATA: &str = "default";
const DEFAULT_SINK: &str = "default.audio.sink";
const DEFAULT_SOURCE: &str = "default.audio.source";

/// A running watch. Dropping it stops the watch and waits for its thread.
pub(super) struct Watch {
    /// Tells the watch's loop to quit; `None` if it never started.
    quit: Option<pw::channel::Sender<()>>,
    /// The watch's thread.
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Watch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Watch")
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        if let Some(quit) = self.quit.take() {
            // A loop that already quit has dropped its end: nothing to tell.
            let _ = quit.send(());
        }
        if let Some(thread) = self.thread.take() {
            // A watch that panicked has nothing left to report.
            let _ = thread.join();
        }
    }
}

/// Starts watching which device `source` is on, reporting changes through
/// `events`: [`CaptureSender::device`], and for a pinned device that went
/// away, [`CaptureSender::failed`] too, which ends the track.
pub(super) fn watch(source: Source, events: CaptureSender) -> Watch {
    let (quit, quit_rx) = pw::channel::channel::<()>();
    let warn = events.clone();
    let spawned = thread::Builder::new()
        .name("nota-route".into())
        .spawn(move || {
            if let Err(error) = run(source, &events, quit_rx) {
                events.notice(unwatched(&error));
            }
        });
    match spawned {
        Ok(thread) => Watch {
            quit: Some(quit),
            thread: Some(thread),
        },
        Err(error) => {
            warn.notice(unwatched(&error));
            Watch {
                quit: None,
                thread: None,
            }
        }
    }
}

/// The warning for a watch that couldn't run, for `error`.
fn unwatched(error: &dyn std::fmt::Display) -> CaptureNotice {
    CaptureNotice::Warning(format!("device changes aren't watched: {error}"))
}

/// Runs the watch until `quit` says to stop, or its connection fails.
fn run(
    source: Source,
    events: &CaptureSender,
    quit: pw::channel::Receiver<()>,
) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = MainLoopRc::new(None)?;
    let context = ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let watcher = Rc::new(RefCell::new(Watcher {
        core: core.clone(),
        route: Route::new(source),
        events: events.clone(),
        current: None,
        fresh: None,
    }));
    let _quit = quit.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    let _core = core
        .add_listener_local()
        .done({
            let watcher = Rc::downgrade(&watcher);
            move |id, seq| {
                if id == PW_ID_CORE {
                    Watcher::done(&watcher, seq);
                }
            }
        })
        .error({
            let mainloop = mainloop.clone();
            let events = events.clone();
            move |id, _seq, _res, message| {
                // Only the connection's own errors end the watch.
                if id == PW_ID_CORE {
                    events.notice(CaptureNotice::Warning(format!(
                        "device changes are no longer watched: {message}"
                    )));
                    mainloop.quit();
                }
            }
        })
        .register();
    Watcher::subscribe(&watcher)?;
    let tick = mainloop.loop_().add_timer({
        let watcher = Rc::downgrade(&watcher);
        let since_poll = RefCell::new(Duration::ZERO);
        move |_| {
            let mut since = since_poll.borrow_mut();
            *since += TICK;
            if *since >= POLL {
                *since = Duration::ZERO;
                if let Some(watcher) = watcher.upgrade() {
                    // A poll that can't subscribe tries again next time;
                    // the current subscription still holds.
                    let _ = Watcher::subscribe(&watcher);
                }
            }
            Watcher::look(&watcher);
        }
    });
    // Can't fail for a timer the loop just made.
    let _ = tick.update_timer(Some(TICK), Some(TICK));
    mainloop.run();
    Ok(())
}

/// The watch's state, on its loop's thread.
struct Watcher {
    /// The connection.
    core: CoreRc,
    /// The track's route.
    route: Route,
    /// Where changes are reported.
    events: CaptureSender,
    /// The subscription whose graph is looked at, once one has caught up.
    current: Option<Rc<RefCell<Subscription>>>,
    /// A newer subscription catching up, to replace it.
    fresh: Option<Rc<RefCell<Subscription>>>,
}

impl Watcher {
    /// Subscribes to the graph afresh: the first time, or as the poll. A
    /// fresh subscription already catching up is left to finish.
    fn subscribe(this: &Rc<RefCell<Self>>) -> Result<(), pw::Error> {
        if this.borrow().fresh.is_some() {
            return Ok(());
        }
        let core = this.borrow().core.clone();
        let subscription = Subscription::start(&core, Rc::downgrade(this))?;
        this.borrow_mut().fresh = Some(subscription);
        Ok(())
    }

    /// Takes in the server's answer to sync `seq`: a subscription that has
    /// had all its answers has caught up, and replaces the current one.
    fn done(this: &Weak<RefCell<Self>>, seq: AsyncSeq) {
        let Some(this) = this.upgrade() else {
            return;
        };
        let caught_up = {
            let watcher = this.borrow();
            let mut caught_up = false;
            for subscription in watcher.current.iter().chain(&watcher.fresh) {
                caught_up |= subscription.borrow_mut().pending.answered(seq);
            }
            caught_up
        };
        if caught_up {
            let mut watcher = this.borrow_mut();
            if watcher
                .fresh
                .as_ref()
                .is_some_and(|s| s.borrow().pending.caught_up())
            {
                watcher.current = watcher.fresh.take();
            }
        }
        Self::look(&Rc::downgrade(&this));
    }

    /// Looks at the current graph, and reports what changed for the track.
    fn look(this: &Weak<RefCell<Self>>) {
        let Some(this) = this.upgrade() else {
            return;
        };
        let mut watcher = this.borrow_mut();
        let Some(current) = watcher.current.clone() else {
            return;
        };
        let now = watcher.events.clock.now();
        let Some(changes) = watcher.route.look(&current.borrow().graph, now) else {
            return;
        };
        watcher.events.device(changes.change);
        // A followed default with nothing to follow keeps its stream: the
        // server moves it to the next default there is. A pinned device's
        // stream ends, so it records nothing the server moves it to.
        if changes.ends_stream {
            let source = watcher.route.source().clone();
            watcher
                .events
                .failed(CaptureError::DeviceNotAvailable(source));
        }
    }
}

/// One subscription to the graph: the registry and the `default` metadata,
/// with the graph they've given so far.
struct Subscription {
    /// The registry.
    registry: RegistryRc,
    /// Its listener; dropping it ends the subscription.
    _registry_listener: Listener,
    /// The `default` metadata and its listener, once bound.
    metadata: Option<(Metadata, MetadataListener)>,
    /// The graph so far.
    graph: Graph,
    /// Syncs not yet answered: until they are, the graph may be missing
    /// what the server has.
    pending: Pending,
}

/// The syncs a subscription has sent and the server hasn't yet answered.
#[derive(Debug, Default)]
struct Pending(Vec<AsyncSeq>);

impl Pending {
    /// Notes sync `seq`, sent.
    fn sent(&mut self, seq: AsyncSeq) {
        self.0.push(seq);
    }

    /// Takes in the answer to sync `seq`. `true` if that was the last one
    /// unanswered; an answer to another's sync is nothing.
    fn answered(&mut self, seq: AsyncSeq) -> bool {
        let before = self.0.len();
        self.0.retain(|&pending| pending != seq);
        before != self.0.len() && self.0.is_empty()
    }

    /// Whether every sync sent has been answered.
    fn caught_up(&self) -> bool {
        self.0.is_empty()
    }
}

impl Subscription {
    /// Subscribes on `core`, telling `watcher` of each change once caught
    /// up.
    fn start(
        core: &CoreRc,
        watcher: Weak<RefCell<Watcher>>,
    ) -> Result<Rc<RefCell<Self>>, pw::Error> {
        let registry = core.get_registry_rc()?;
        let seq = core.sync(0)?;
        Ok(Rc::new_cyclic(|this: &Weak<RefCell<Self>>| {
            let listener = registry
                .add_listener_local()
                .global({
                    let this = Weak::clone(this);
                    let core = core.clone();
                    let watcher = Weak::clone(&watcher);
                    move |global| {
                        if let Some(this) = this.upgrade() {
                            Self::global(&this, &core, &watcher, global);
                            Watcher::look(&watcher);
                        }
                    }
                })
                .global_remove({
                    let this = Weak::clone(this);
                    move |id| {
                        if let Some(this) = this.upgrade() {
                            this.borrow_mut().graph.apply(GraphEvent::Removed { id });
                            Watcher::look(&watcher);
                        }
                    }
                })
                .register();
            RefCell::new(Self {
                registry: registry.clone(),
                _registry_listener: listener,
                metadata: None,
                graph: Graph::default(),
                // The registry announces what it has before it answers.
                pending: Pending(vec![seq]),
            })
        }))
    }

    /// Takes in a global the registry announced: an audio node, or the
    /// `default` metadata, which it binds to hear its values.
    fn global(
        this: &Rc<RefCell<Self>>,
        core: &CoreRc,
        watcher: &Weak<RefCell<Watcher>>,
        global: &GlobalObject<&DictRef>,
    ) {
        match global.type_ {
            ObjectType::Node => {
                if let Some(node) = global.props.and_then(audio_node) {
                    let id = global.id;
                    this.borrow_mut()
                        .graph
                        .apply(GraphEvent::Added { id, node });
                }
            }
            ObjectType::Metadata => {
                let named_default = global
                    .props
                    .and_then(|props| props.get("metadata.name"))
                    .is_some_and(|name| name == DEFAULT_METADATA);
                if named_default && this.borrow().metadata.is_none() {
                    Self::bind_defaults(this, core, watcher, global);
                }
            }
            _ => {}
        }
    }

    /// Binds the `default` metadata `global`, and syncs so the subscription
    /// counts as caught up only once its values have come.
    fn bind_defaults(
        this: &Rc<RefCell<Self>>,
        core: &CoreRc,
        watcher: &Weak<RefCell<Watcher>>,
        global: &GlobalObject<&DictRef>,
    ) {
        let registry = this.borrow().registry.clone();
        let Ok(metadata) = registry.bind::<Metadata, _>(global) else {
            // Without it, defaults can't be followed; a pinned device still
            // can, and the next poll tries again.
            return;
        };
        let listener = metadata
            .add_listener_local()
            .property({
                let this = Rc::downgrade(this);
                let watcher = Weak::clone(watcher);
                move |_subject, key, _type, value| {
                    if let Some(this) = this.upgrade()
                        && let Some(event) = default_event(key, value)
                    {
                        this.borrow_mut().graph.apply(event);
                        Watcher::look(&watcher);
                    }
                    0
                }
            })
            .register();
        let mut subscription = this.borrow_mut();
        subscription.metadata = Some((metadata, listener));
        if let Ok(seq) = core.sync(0) {
            subscription.pending.sent(seq);
        }
    }
}

/// The audio node a registry global's `props` describe, if it is one.
fn audio_node(props: &DictRef) -> Option<Node> {
    let class = match props.get(*pw::keys::MEDIA_CLASS)? {
        "Audio/Sink" => Class::Sink,
        "Audio/Source" | "Audio/Source/Virtual" => Class::Source,
        other if other.starts_with("Audio/") => Class::Other,
        _ => return None,
    };
    let name = props.get(*pw::keys::NODE_NAME)?.to_owned();
    let description = props
        .get(*pw::keys::NODE_DESCRIPTION)
        .or_else(|| props.get(*pw::keys::NODE_NICK))
        .unwrap_or(&name)
        .to_owned();
    Some(Node {
        name,
        description,
        class,
    })
}

/// What a `default` metadata property says, if it's the default sink or
/// source: the node it names, or none if it was cleared or unreadable.
fn default_event(key: Option<&str>, value: Option<&str>) -> Option<GraphEvent> {
    let class = match key? {
        DEFAULT_SINK => Class::Sink,
        DEFAULT_SOURCE => Class::Source,
        _ => return None,
    };
    Some(GraphEvent::Default {
        class,
        name: value.and_then(route::default_name),
    })
}

#[cfg(test)]
mod tests {
    use pw::properties::PropertiesBox;

    use super::*;

    /// Registry properties holding `pairs`.
    fn props(pairs: &[(&str, &str)]) -> PropertiesBox {
        let mut props = PropertiesBox::new();
        for (key, value) in pairs {
            props.insert(*key, *value);
        }
        props
    }

    /// A sink, a source, a virtual source and a duplex node are audio nodes
    /// of their kind, named as the user knows them.
    #[test]
    fn audio_nodes_are_read_with_their_kind() {
        for (class, kind) in [
            ("Audio/Sink", Class::Sink),
            ("Audio/Source", Class::Source),
            ("Audio/Source/Virtual", Class::Source),
            ("Audio/Duplex", Class::Other),
        ] {
            let node = props(&[
                ("media.class", class),
                ("node.name", "alsa.x"),
                ("node.description", "Speakers"),
                ("node.nick", "Spk"),
            ]);
            assert_eq!(
                audio_node(node.as_ref()),
                Some(Node {
                    name: "alsa.x".into(),
                    description: "Speakers".into(),
                    class: kind,
                }),
                "{class}"
            );
        }
    }

    /// A node with no description goes by its nick, then by its name.
    #[test]
    fn a_node_without_a_description_goes_by_its_nick_or_name() {
        let nick = props(&[
            ("media.class", "Audio/Sink"),
            ("node.name", "n"),
            ("node.nick", "Nick"),
        ]);
        let bare = props(&[("media.class", "Audio/Sink"), ("node.name", "n")]);
        let described = |p: &PropertiesBox| audio_node(p.as_ref()).map(|n| n.description);
        assert_eq!(
            (described(&nick), described(&bare)),
            (Some("Nick".into()), Some("n".into()))
        );
    }

    /// Streams, video and nameless nodes aren't audio devices.
    #[test]
    fn other_nodes_are_not_audio_devices() {
        for node in [
            props(&[("media.class", "Stream/Input/Audio"), ("node.name", "rec")]),
            props(&[("media.class", "Video/Source"), ("node.name", "cam")]),
            props(&[("media.class", "Audio/Sink")]),
            props(&[("node.name", "x")]),
        ] {
            assert_eq!(audio_node(node.as_ref()), None);
        }
    }

    /// A subscription has caught up once every sync it sent is answered,
    /// and only its own answers count.
    #[test]
    fn a_subscription_catches_up_on_its_last_answer() {
        let mut pending = Pending::default();
        assert!(pending.caught_up());
        pending.sent(AsyncSeq::from_seq(1));
        pending.sent(AsyncSeq::from_seq(2));
        assert!(!pending.answered(AsyncSeq::from_seq(9)), "another's sync");
        assert!(!pending.answered(AsyncSeq::from_seq(1)));
        assert!(!pending.caught_up());
        assert!(pending.answered(AsyncSeq::from_seq(2)));
        assert!(pending.caught_up());
        // Answered again, it's nothing new.
        assert!(!pending.answered(AsyncSeq::from_seq(2)));
    }

    /// A watch that can't run says so, and why.
    #[test]
    fn a_watch_that_cant_run_says_why() {
        assert_eq!(
            unwatched(&"no socket"),
            CaptureNotice::Warning("device changes aren't watched: no socket".into())
        );
    }

    /// The default sink and source keys give the node they name; a cleared
    /// key gives none; other keys nothing.
    #[test]
    fn default_properties_give_the_defaults() {
        assert_eq!(
            default_event(Some(DEFAULT_SINK), Some(r#"{"name":"speakers"}"#)),
            Some(GraphEvent::Default {
                class: Class::Sink,
                name: Some("speakers".into()),
            })
        );
        assert_eq!(
            default_event(Some(DEFAULT_SOURCE), None),
            Some(GraphEvent::Default {
                class: Class::Source,
                name: None,
            })
        );
        assert_eq!(
            default_event(Some("default.configured.audio.sink"), Some("{}")),
            None
        );
        assert_eq!(default_event(None, None), None);
    }
}
