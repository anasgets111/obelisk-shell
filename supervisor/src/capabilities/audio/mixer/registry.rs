//! The PipeWire registry/event plumbing: binds every global this capability cares about
//! (stream/video-source nodes, `Audio/Sink` nodes, the `default` metadata object, ALSA
//! devices) and routes their events into `state::MixerState`. `run` is the thread entry point;
//! everything else here runs on that thread's own blocking `main_loop.run()`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use pipewire as pw;
use pw::keys;
use pw::registry::GlobalObject;
use pw::spa::pod::Value;
use pw::spa::pod::deserialize::PodDeserializer;
use pw::spa::utils::dict::DictRef;
use pw::types::ObjectType;
use tokio::sync::mpsc::UnboundedSender;

use crate::capabilities::audio::master;

use super::state::{
    AudioApps, AudioCommand, AudioState, DEFAULT_AUDIO_SINK_KEY, DEFAULT_AUDIO_SOURCE_KEY, DefaultDevice, DeviceNames,
    MixerState, NodeKind, PropsLookup, SinkEntry, SinkRoute, VideoSourceApp, VideoSourceApps, apply_info_event,
    apply_video_info_event, classify, device_display_name,
};
use super::write::apply_command;

/// Runs the PipeWire registry listener until the process exits, sending an updated
/// [`AudioState`] (per-app streams plus § 2.4 master volume/mute) over `updates` on every
/// relevant node-added/-properties-changed/-removed/param-changed event, and an updated snapshot
/// of [`VideoSourceApp`]s over `video_updates` on the equivalent video events (ADR-0034) --
/// one PipeWire connection, two independent capabilities' worth of data, each publishing only on
/// its own changes. Blocks the calling thread -- call from a dedicated `std::thread::spawn`,
/// never an async task: `pipewire-rs`'s event loop and the `Rc`-based listener state here are
/// single-threaded and non-`Send`.
///
/// `updates` only reaches a log line in `main()` for now, not Lua -- see ADR-0017 for why
/// and what unblocks it. `video_updates` feeds `privacy::PrivacyController`'s name-enrichment
/// (ADR-0034), not a log line.
///
/// ponytail: no shutdown path -- `main_loop.run()` returns only when the process exits. Phase
/// 7/8's reload orchestrator is what would give this a `main_loop.quit()` trigger to react to;
/// nothing calls for that yet.
///
/// Logs and returns if PipeWire can't be reached at all (no daemon running) rather than
/// panicking: audio/video-source tracking is one optional subsystem, not a reason to take the
/// whole supervisor down.
pub fn run(
    updates: UnboundedSender<AudioState>,
    video_updates: UnboundedSender<Vec<VideoSourceApp>>,
    commands: AudioCommandReceiver,
) {
    if let Err(err) = run_inner(updates, video_updates, commands) {
        eprintln!("pipewire registry listener stopped: {err}");
    }
}

/// The write half of § 3.2's audio actions. `main.rs` holds the [`AudioCommandSender`] and hands
/// the receiver to [`run`], which attaches it to the PipeWire loop -- see [`AudioCommand`] for
/// why this is a channel rather than a method on a controller.
pub type AudioCommandSender = pw::channel::Sender<AudioCommand>;
pub type AudioCommandReceiver = pw::channel::Receiver<AudioCommand>;

/// Built here rather than in `main.rs` so nothing outside this module needs to name
/// `pipewire::channel` at all.
pub fn command_channel() -> (AudioCommandSender, AudioCommandReceiver) {
    pw::channel::channel()
}

fn run_inner(
    updates: UnboundedSender<AudioState>,
    video_updates: UnboundedSender<Vec<VideoSourceApp>>,
    commands: AudioCommandReceiver,
) -> Result<(), pw::Error> {
    pw::init();

    let main_loop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&main_loop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    let state = Rc::new(RefCell::new(MixerState {
        apps: AudioApps::new(),
        video_sources: VideoSourceApps::new(),
        nodes: HashMap::new(),
        updates,
        video_updates,
        sinks: HashMap::new(),
        sink_nodes: HashMap::new(),
        sources: HashMap::new(),
        devices: HashMap::new(),
        device_routes: HashMap::new(),
        app_props: HashMap::new(),
        default_sink_name: None,
        default_source_name: None,
        metadata: None,
        metadata_id: None,
    }));

    // Weak, not a clone of registry itself: the listener this builds is a hook stored on
    // registry's own C object, so a strong RegistryRc captured here would keep itself alive
    // forever.
    let registry_weak = registry.downgrade();
    let state_for_global = Rc::clone(&state);
    let state_for_remove = Rc::clone(&state);

    let _registry_listener = registry
        .add_listener_local()
        .global(move |obj| {
            if let Some(registry) = registry_weak.upgrade() {
                on_global(&state_for_global, &registry, obj);
            }
        })
        .global_remove(move |id| {
            let mut state = state_for_remove.borrow_mut();
            state.nodes.remove(&id);
            state.sinks.remove(&id);
            state.sink_nodes.remove(&id);
            state.sources.remove(&id);
            state.app_props.remove(&id);
            if state.devices.remove(&id).is_some() {
                // A removed device takes every route index it published with it, keyed by its
                // own global id, so a later device reusing that id doesn't inherit route indices
                // for hardware that's no longer plugged in.
                state.device_routes.retain(|&(device_id, _), _| device_id != id);
            }
            if state.metadata_id == Some(id) {
                state.metadata = None;
                state.metadata_id = None;
                state.default_sink_name = None;
                state.default_source_name = None;
            }
            // Audio publishes unconditionally on every removal, matching this capability's
            // already-shipped behavior (ADR-0034 scopes this extension as privacy name-
            // enrichment, not a change to audio's publish cadence). Video is new code, so it
            // publishes only when `id` was actually a tracked video source (see
            // VideoSourceApps::remove). Sink/metadata cleanup runs unconditionally for the same
            // reason: publish_audio recomputes master state from whatever's left on every call,
            // so a stale sink entry would otherwise linger until some other event overwrote it.
            state.apps.remove(id);
            state.publish_audio();
            if state.video_sources.remove(id) {
                state.publish_video();
            }
        })
        .register();

    // Held for the loop's lifetime: dropping the AttachedReceiver detaches the eventfd source
    // and every later command is silently discarded.
    let state_for_command = Rc::clone(&state);
    let _attached_commands =
        commands.attach(main_loop.loop_(), move |command| apply_command(&state_for_command, command));

    main_loop.run();
    Ok(())
}

/// Handles one registry `global` event: routes `Node` globals to [`on_node_global`] (audio
/// stream, video source, or `Audio/Sink` -- see [`classify`] and [`bind_sink`]) and the `default`
/// `Metadata` global to [`bind_default_metadata`] (§ 2.4's default-sink routing). Anything else
/// (other object types, other `Metadata` objects like `settings`/`filters`/`route-settings`,
/// checked live via `pw-mon`) is ignored.
fn on_global(state: &Rc<RefCell<MixerState>>, registry: &pw::registry::RegistryRc, obj: &GlobalObject<&DictRef>) {
    match obj.type_ {
        ObjectType::Node => on_node_global(state, registry, obj),
        ObjectType::Device => bind_device(state, registry, obj),
        ObjectType::Metadata => bind_default_metadata(state, registry, obj),
        _ => {}
    }
}

/// `media.class` value an output audio device node carries (§ 2.4's master volume lives here,
/// not on any `Stream/Output/Audio` node). Verified against real `pw-dump` output -- see
/// [`master`]'s module doc comment.
const AUDIO_SINK: &str = "Audio/Sink";

/// `media.class` value an input capture device node carries (§ 2.4's `sources`). Unlike
/// [`AUDIO_SINK`], a source is never bound: § 2.4's source object is `id`/`name`/`active`, all
/// answerable from the `global` event's own props plus the default-source metadata key.
///
/// No monitor filter, deliberately: PulseAudio synthesizes a `.monitor` source per sink, but a
/// native PipeWire registry does not, and `pw-dump` on this machine lists exactly one
/// `Audio/Source` beside one `Audio/Sink`, no monitor node between them.
const AUDIO_SOURCE: &str = "Audio/Source";

/// PipeWire's own key for a `Metadata` object's name (`"settings"`, `"default"`,
/// `"route-settings"`, etc. -- a machine advertises several). Not in `pipewire-rs`'s `keys`
/// module, so spelled out directly; confirmed against live `pw-mon` output (`metadata.name =
/// "default"` on the global this file needs).
const METADATA_NAME: &str = "metadata.name";

/// Handles one `Node` `global` event: classifies it by `media.class` (audio stream, video
/// source, `Audio/Sink`, or neither -- see [`classify`]). `Audio/Sink` is handled separately by
/// [`bind_sink`], not folded into [`classify`]'s [`NodeKind`]: a sink needs a `param` listener
/// (§ 2.4's master volume) instead of the `info` listener every [`NodeKind`] variant here uses.
/// For the stream/video-source path: binds the node so its `info` event -- gated to only the
/// calls that carry a props change, see the module doc comment -- extracts the pid and keeps
/// the matching list current. Doesn't require the full parse to succeed here:
/// `application.process.id` can still be missing at `global` time and shows up in a later `info`
/// call instead.
fn on_node_global(state: &Rc<RefCell<MixerState>>, registry: &pw::registry::RegistryRc, obj: &GlobalObject<&DictRef>) {
    if obj.props.and_then(|props| props.get_prop(*keys::MEDIA_CLASS)) == Some(AUDIO_SINK) {
        bind_sink(state, registry, obj);
        return;
    }
    if obj.props.and_then(|props| props.get_prop(*keys::MEDIA_CLASS)) == Some(AUDIO_SOURCE) {
        track_source(state, obj);
        return;
    }
    let Some(kind) = obj.props.and_then(classify) else {
        return;
    };

    let node: pw::node::Node = match registry.bind(obj) {
        Ok(node) => node,
        Err(_) => return,
    };
    let node_id = obj.id;
    let state_for_info = Rc::clone(state);
    let state_for_param = Rc::clone(state);
    let listener = node
        .add_listener_local()
        .info(move |info| {
            let mut state_mut = state_for_info.borrow_mut();
            let has_props_change = info.change_mask().contains(pw::node::NodeChangeMask::PROPS);
            match kind {
                NodeKind::Audio => {
                    apply_info_event(&mut state_mut.apps, node_id, has_props_change, info.props());
                    state_mut.publish_audio();
                }
                NodeKind::Video => {
                    apply_video_info_event(&mut state_mut.video_sources, node_id, has_props_change, info.props());
                    state_mut.publish_video();
                }
            }
        })
        // § 2.4's per-app volume/muted: the same pod, parse, and cube root bind_sink already
        // runs for the master sink, on a node that happens to be a stream -- verified with
        // pw-cli enum-params <id> Props against a live playback stream, which carries
        // channelVolumes and mute exactly as a sink does.
        //
        // A video source has no Props param worth reading and never gets subscribed below, so
        // this callback simply never fires for one.
        .param(move |_seq, param_type, _index, _next, param| {
            if param_type != pw::spa::param::ParamType::Props {
                return;
            }
            let Some(pod) = param else { return };
            let Ok((_, value)) = PodDeserializer::deserialize_from::<Value>(pod.as_bytes()) else {
                return;
            };
            // Same channelVolumes-must-be-present filter the master sink needs: a node can
            // advertise more than one object under ParamType::Props, and reading a non-mixer one
            // as a mixer object reports a volume of zero. See extract_sink_props's own doc
            // comment for the live probe that found it.
            let Some(raw) = master::extract_sink_props(&value) else {
                return;
            };
            let mut state_mut = state_for_param.borrow_mut();
            state_mut.app_props.insert(node_id, raw);
            state_mut.publish_audio();
        })
        .register();

    if kind == NodeKind::Audio {
        node.subscribe_params(&[pw::spa::param::ParamType::Props]);
    }

    state.borrow_mut().nodes.insert(node_id, (node, listener));
}

/// Binds an `Audio/Sink` node (§ 2.4's master volume) and subscribes to its `Props` param, which
/// is where PipeWire keeps volume/mute -- not the `info` props dict `on_node_global`'s
/// stream/video-source path reads (see [`master`]). `node.name` is read once, straight from the
/// `global` event's already-known props: checked live, a sink's `node.name` is present from its
/// very first `global` event, unlike a stream node's `application.process.id` -- so, unlike
/// streams, no `info` listener is needed here at all, only `param`.
///
/// The `param` callback below relies on [`master::extract_sink_props`] returning `None` for a
/// `Props` event that isn't the mixer one -- a sink delivers two separate `Props` objects per
/// subscription/change (mixer keys, then unrelated ALSA device settings with no mixer keys at
/// all), and writing the second one's absence-of-data into `sink_props` as a zeroed volume was
/// a real bug this codebase shipped and fixed.
fn bind_sink(state: &Rc<RefCell<MixerState>>, registry: &pw::registry::RegistryRc, obj: &GlobalObject<&DictRef>) {
    let node_id = obj.id;
    let Some(props) = obj.props else { return };
    let Some(node_name) = props.get_prop(*keys::NODE_NAME) else { return };
    let names = DeviceNames { node_name: node_name.to_string(), description: device_display_name(props) };

    // Bound before anything is recorded, not after: a sink whose bind fails never gets a Props
    // subscription and so never gets a volume either. Recording its name unconditionally would
    // leave resolve_default_device free to resolve to a node whose props can never arrive, and
    // compute_master would then report MasterVolume::default()'s 0.0 forever -- a number that
    // looks like a real 0% on a capability with no "unknown" sentinel (ADR-0053).
    let node: pw::node::Node = match registry.bind(obj) {
        Ok(node) => node,
        Err(err) => {
            eprintln!("audio: failed to bind Audio/Sink node {node_id}: {err}");
            return;
        }
    };

    let state_for_param = Rc::clone(state);
    let state_for_info = Rc::clone(state);
    let listener = node
        .add_listener_local()
        .param(move |_seq, param_type, _index, _next, param| {
            if param_type != pw::spa::param::ParamType::Props {
                return;
            }
            let Some(pod) = param else { return };
            let Ok((_, value)) = PodDeserializer::deserialize_from::<Value>(pod.as_bytes()) else {
                return;
            };
            let Some(raw) = master::extract_sink_props(&value) else {
                return;
            };
            let mut state_mut = state_for_param.borrow_mut();
            if let Some(sink) = state_mut.sinks.get_mut(&node_id) {
                sink.props = Some(raw);
            }
            state_mut.publish_audio();
        })
        // The route comes from info, not from the global event, and that is not a style choice.
        // Checked live: a sink's global props carry media.class and node.name but not device.id
        // or card.profile.device, both present only under the node's full info props. Reading
        // them at global time returns None, write_master then takes the node path, and the
        // write is accepted and silently discarded -- the same "global carries a subset" trap
        // the module doc comment records for a stream node's application.process.id.
        .info(move |info| {
            if !info.change_mask().contains(pw::node::NodeChangeMask::PROPS) {
                return;
            }
            let Some(route) = info.props().and_then(parse_sink_route) else { return };
            if let Some(sink) = state_for_info.borrow_mut().sinks.get_mut(&node_id) {
                sink.route = Some(route);
            }
        })
        .register();

    // Requested after registering the listener above, matching this file's existing bind-then-
    // listen-then-subscribe ordering elsewhere; nothing dispatches either call until
    // main_loop.run(), so the two orderings are behaviorally identical here.
    node.subscribe_params(&[pw::spa::param::ParamType::Props]);

    let mut state_mut = state.borrow_mut();
    state_mut.sinks.insert(node_id, SinkEntry { names, props: None, route: None });
    state_mut.sink_nodes.insert(node_id, (node, listener));
    state_mut.publish_audio();
}

/// The hardware device behind a sink, from the sink node's own `global` props. Both keys are
/// present together or not at all: this machine's analog output carries `device.id = 49` and
/// `card.profile.device = 7`, and a sink with no hardware behind it carries neither. `None`
/// means the sink's volume really does live on its node.
fn parse_sink_route(props: &impl PropsLookup) -> Option<SinkRoute> {
    let device_id = props.get_prop(*keys::DEVICE_ID)?.parse().ok()?;
    let profile_device = props.get_prop(CARD_PROFILE_DEVICE)?.parse().ok()?;
    Some(SinkRoute { device_id, profile_device })
}

/// The sink node property naming which of its device's routes it plays through. Not in
/// `pipewire-rs`'s `keys` module, so spelled out here; read live off `pw-dump`, where this
/// machine's analog output carries `card.profile.device = 7` and the matching `Route` object on
/// device 49 carries `device: 7`.
const CARD_PROFILE_DEVICE: &str = "card.profile.device";

/// Records an `Audio/Source` node's two names. No `registry.bind`, no proxy, no listener: § 2.4's
/// source object is `id`, `name` and `active`, and the `global` event already carries the first
/// two while the third comes from the `default.audio.source` metadata key. The asymmetry with
/// [`bind_sink`] is deliberate, not an oversight: a sink is bound because § 2.4 asks for the
/// master `volume`/`muted`, which only its `Props` param has. A source has no such field.
fn track_source(state: &Rc<RefCell<MixerState>>, obj: &GlobalObject<&DictRef>) {
    let Some(props) = obj.props else { return };
    let Some(node_name) = props.get_prop(*keys::NODE_NAME) else { return };
    let mut state_mut = state.borrow_mut();
    state_mut
        .sources
        .insert(obj.id, DeviceNames { node_name: node_name.to_string(), description: device_display_name(props) });
    state_mut.publish_audio();
}

/// Binds the one `Metadata` global whose `metadata.name` is `"default"` -- the object that
/// publishes `default.audio.sink` (§ 2.4's routing), among several other `default.*` keys this
/// capability doesn't need. A machine advertises multiple `Metadata` objects (`settings`,
/// `filters`, `route-settings`, confirmed live via `pw-mon`); every non-matching one is left
/// unbound.
fn bind_default_metadata(
    state: &Rc<RefCell<MixerState>>,
    registry: &pw::registry::RegistryRc,
    obj: &GlobalObject<&DictRef>,
) {
    if obj.props.and_then(|props| props.get_prop(METADATA_NAME)) != Some("default") {
        return;
    }

    let metadata: pw::metadata::Metadata = match registry.bind(obj) {
        Ok(metadata) => metadata,
        Err(_) => return,
    };

    let state_for_property = Rc::clone(state);
    let listener = metadata
        .add_listener_local()
        .property(move |_subject, key, _type_, value| {
            // The default metadata carries several keys this capability does not read
            // (default.configured.audio.sink, default.video.source). default.configured.* is
            // not the same fact: on this machine it names a Bluetooth device that isn't
            // connected, while default.audio.sink names the analog output actually in use.
            let field = match key {
                Some(DEFAULT_AUDIO_SINK_KEY) => DefaultDevice::Sink,
                Some(DEFAULT_AUDIO_SOURCE_KEY) => DefaultDevice::Source,
                _ => return 0,
            };
            let mut state_mut = state_for_property.borrow_mut();
            // value: None means the property was cleared -- treated the same as "no default
            // known", which resolve_default_device already falls back from.
            let name = value.and_then(master::parse_default_device_name);
            match field {
                DefaultDevice::Sink => state_mut.default_sink_name = name,
                DefaultDevice::Source => state_mut.default_source_name = name,
            }
            state_mut.publish_audio();
            0
        })
        .register();

    let mut state_mut = state.borrow_mut();
    state_mut.metadata_id = Some(obj.id);
    state_mut.metadata = Some((metadata, listener));
}

/// Binds an ALSA `Device` global and subscribes to its `Route` param, the only place a route's
/// active index is published. Bound purely for the write path: nothing in § 2.4 reads a device,
/// and without this a `set_volume` on a hardware sink would be accepted and silently dropped
/// (see `write::write_master`).
///
/// Only `device.api == "alsa"` devices are bound. This machine also advertises `v4l2` and
/// `libcamera` devices, which have no audio routes at all.
fn bind_device(state: &Rc<RefCell<MixerState>>, registry: &pw::registry::RegistryRc, obj: &GlobalObject<&DictRef>) {
    if obj.props.and_then(|props| props.get_prop(*keys::DEVICE_API)) != Some("alsa") {
        return;
    }
    let device_id = obj.id;
    let device: pw::device::Device = match registry.bind(obj) {
        Ok(device) => device,
        Err(err) => {
            eprintln!("audio: failed to bind ALSA device {device_id}: {err}");
            return;
        }
    };

    let state_for_param = Rc::clone(state);
    let listener = device
        .add_listener_local()
        .param(move |_seq, param_type, _index, _next, param| {
            if param_type != pw::spa::param::ParamType::Route {
                return;
            }
            let Some(pod) = param else { return };
            let Ok((_, value)) = PodDeserializer::deserialize_from::<Value>(pod.as_bytes()) else {
                return;
            };
            let Some((profile_device, index)) = master::extract_route_target(&value) else {
                return;
            };
            state_for_param.borrow_mut().device_routes.insert((device_id, profile_device), index);
        })
        .register();

    device.subscribe_params(&[pw::spa::param::ParamType::Route]);
    state.borrow_mut().devices.insert(device_id, (device, listener));
}
