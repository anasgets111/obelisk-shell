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

/// Runs the PipeWire registry listener until the process exits, sending an updated [`AudioState`]
/// (per-app streams plus § 2.4 master volume/mute) over `updates` on every relevant
/// node-added/-properties-changed/-removed/param-changed event, and a snapshot of
/// [`VideoSourceApp`]s over `video_updates` on the equivalent video events (ADR-0034): one
/// PipeWire connection serving two capabilities, each publishing only on its own changes. Blocks
/// the calling thread (`pipewire-rs`'s event loop and the `Rc`-based listener state here are
/// single-threaded and non-`Send`), so call from `std::thread::spawn`, never async.
///
/// `updates` only reaches a log line in `main()`, not Lua yet (ADR-0017); `video_updates` feeds
/// `privacy::PrivacyController`'s name-enrichment (ADR-0034). ponytail: no shutdown path, since
/// `main_loop.run()` returns only at process exit and nothing yet needs Phase 7/8's reload
/// orchestrator to add a `quit()` trigger. Logs and returns rather than panicking if PipeWire is
/// unreachable: one optional subsystem, not a reason to take down the supervisor.
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
/// the receiver to [`run`], which attaches it to the PipeWire loop (see [`AudioCommand`] for why
/// this is a channel, not a method on a controller).
pub type AudioCommandSender = pw::channel::Sender<AudioCommand>;
pub type AudioCommandReceiver = pw::channel::Receiver<AudioCommand>;

/// Built here, not in `main.rs`, so nothing outside this module needs to name `pipewire::channel`.
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

    // Weak, not a clone: the listener this builds is a hook stored on registry's own C object,
    // so a strong RegistryRc captured here would keep itself alive forever.
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
                // own global id, so a device reusing that id later doesn't inherit stale indices.
                state.device_routes.retain(|&(device_id, _), _| device_id != id);
            }
            if state.metadata_id == Some(id) {
                state.metadata = None;
                state.metadata_id = None;
                state.default_sink_name = None;
                state.default_source_name = None;
            }
            // Audio publishes unconditionally on every removal (ADR-0034 scopes this as
            // name-enrichment, not a cadence change); video only when `id` was a tracked video
            // source (see VideoSourceApps::remove). Sink/metadata cleanup is unconditional too,
            // since publish_audio recomputes master state from what's left each call.
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
/// stream, video source, or `Audio/Sink`; see [`classify`], [`bind_sink`]) and the `default`
/// `Metadata` global to [`bind_default_metadata`] (§ 2.4's routing); everything else is ignored.
fn on_global(state: &Rc<RefCell<MixerState>>, registry: &pw::registry::RegistryRc, obj: &GlobalObject<&DictRef>) {
    match obj.type_ {
        ObjectType::Node => on_node_global(state, registry, obj),
        ObjectType::Device => bind_device(state, registry, obj),
        ObjectType::Metadata => bind_default_metadata(state, registry, obj),
        _ => {}
    }
}

/// `media.class` value an output audio device node carries (§ 2.4's master volume lives here,
/// not on any `Stream/Output/Audio` node); verified against real `pw-dump` output (see [`master`]).
const AUDIO_SINK: &str = "Audio/Sink";

/// `media.class` value an input capture device node carries (§ 2.4's `sources`). Unlike
/// [`AUDIO_SINK`], a source is never bound: its `id`/`name`/`active` are answerable straight from
/// the `global` event's own props plus the default-source metadata key. No monitor filter is
/// applied, deliberately: PulseAudio synthesizes a `.monitor` source per sink, but native
/// PipeWire does not, and `pw-dump` here lists exactly one `Audio/Source` beside one `Audio/Sink`.
const AUDIO_SOURCE: &str = "Audio/Source";

/// PipeWire's own key for a `Metadata` object's name (`"settings"`, `"default"`,
/// `"route-settings"`, etc.: a machine advertises several). Not in `pipewire-rs`'s `keys` module,
/// confirmed against live `pw-mon` output (`metadata.name = "default"` on the needed global).
const METADATA_NAME: &str = "metadata.name";

/// Handles one `Node` `global` event: classifies it by `media.class` (audio stream, video
/// source, `Audio/Sink`, or neither; see [`classify`]). `Audio/Sink` goes to [`bind_sink`]
/// instead of [`classify`]'s [`NodeKind`], since a sink needs a `param` listener (§ 2.4's master
/// volume), not the `info` listener every [`NodeKind`] variant uses. The stream/video-source path
/// binds the node so its `info` event, gated to props-change calls, extracts the pid;
/// `application.process.id` can still be missing at `global` time and arrive later instead.
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
        // § 2.4's per-app volume/muted: the same pod, parse, and cube root bind_sink runs for the
        // master sink, on a node that's a stream (verified live: it carries channelVolumes and
        // mute like a sink). A video source has no Props param worth reading, so this never fires.
        .param(move |_seq, param_type, _index, _next, param| {
            if param_type != pw::spa::param::ParamType::Props {
                return;
            }
            let Some(pod) = param else { return };
            let Ok((_, value)) = PodDeserializer::deserialize_from::<Value>(pod.as_bytes()) else {
                return;
            };
            // Same channelVolumes-must-be-present filter the master sink needs (see
            // extract_sink_props's own doc): a node can advertise more than one Props object,
            // and reading a non-mixer one as the mixer reports a volume of zero.
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

/// Binds an `Audio/Sink` node (§ 2.4's master volume) and subscribes to its `Props` param,
/// where PipeWire keeps volume/mute, not the `info` props dict the stream/video-source path reads
/// (see [`master`]). `node.name` is read once from the `global` event's own props: checked live,
/// it's present from the sink's very first `global` event, unlike a stream's
/// `application.process.id`, so no `info` listener is needed, only `param`.
///
/// The `param` callback relies on [`master::extract_sink_props`] returning `None` for a
/// non-mixer `Props` event: a sink delivers two separate `Props` objects per change (mixer, then
/// unrelated ALSA settings), and writing the second's absence as zero was a real shipped bug.
fn bind_sink(state: &Rc<RefCell<MixerState>>, registry: &pw::registry::RegistryRc, obj: &GlobalObject<&DictRef>) {
    let node_id = obj.id;
    let Some(props) = obj.props else { return };
    let Some(node_name) = props.get_prop(*keys::NODE_NAME) else { return };
    let names = DeviceNames { node_name: node_name.to_string(), description: device_display_name(props) };

    // Bound before anything is recorded: a sink whose bind fails never gets a Props subscription
    // or a volume, so recording its name first would let resolve_default_device pick a node
    // whose props never arrive, so compute_master reports MasterVolume::default()'s fake 0.0
    // forever (ADR-0053: no "unknown" sentinel).
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
        // The route comes from info, not the global event, deliberately: global props carry
        // media.class and node.name but not device.id or card.profile.device, present only in
        // full info props. Reading them at global time returns None, so write_master silently
        // discards the write.
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

    // Requested after registering, matching this file's bind-then-listen-then-subscribe ordering
    // elsewhere; nothing dispatches either call until main_loop.run(), so order doesn't matter.
    node.subscribe_params(&[pw::spa::param::ParamType::Props]);

    let mut state_mut = state.borrow_mut();
    state_mut.sinks.insert(node_id, SinkEntry { names, props: None, route: None });
    state_mut.sink_nodes.insert(node_id, (node, listener));
    state_mut.publish_audio();
}

/// The hardware device behind a sink, from the sink node's own `global` props. Both keys are
/// present together or not at all (this machine's analog output carries `device.id = 49` and
/// `card.profile.device = 7`); `None` means the volume really does live on the sink's own node.
fn parse_sink_route(props: &impl PropsLookup) -> Option<SinkRoute> {
    let device_id = props.get_prop(*keys::DEVICE_ID)?.parse().ok()?;
    let profile_device = props.get_prop(CARD_PROFILE_DEVICE)?.parse().ok()?;
    Some(SinkRoute { device_id, profile_device })
}

/// The sink node property naming which of its device's routes it plays through. Not in
/// `pipewire-rs`'s `keys` module, so spelled out here: read live off `pw-dump`, this machine's
/// analog output carries `card.profile.device = 7`, matching `device: 7` on device 49's Route.
const CARD_PROFILE_DEVICE: &str = "card.profile.device";

/// Records an `Audio/Source` node's two names. No `registry.bind`, no proxy, no listener: § 2.4's
/// source object is `id`/`name`/`active`, the first two from the `global` event and the third
/// from `default.audio.source`. Unlike [`bind_sink`]: a sink is bound because § 2.4 needs
/// `volume`/`muted`, found only on its `Props` param, which a source lacks.
fn track_source(state: &Rc<RefCell<MixerState>>, obj: &GlobalObject<&DictRef>) {
    let Some(props) = obj.props else { return };
    let Some(node_name) = props.get_prop(*keys::NODE_NAME) else { return };
    let mut state_mut = state.borrow_mut();
    state_mut
        .sources
        .insert(obj.id, DeviceNames { node_name: node_name.to_string(), description: device_display_name(props) });
    state_mut.publish_audio();
}

/// Binds the one `Metadata` global whose `metadata.name` is `"default"`: the object that
/// publishes `default.audio.sink` (§ 2.4's routing), among other `default.*` keys this capability
/// doesn't need. A machine advertises several `Metadata` objects; non-matching ones stay unbound.
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
            // The default metadata carries several keys this capability doesn't read
            // (default.configured.audio.sink, default.video.source): default.configured.* names
            // a disconnected Bluetooth device here, while default.audio.sink names the real output.
            let field = match key {
                Some(DEFAULT_AUDIO_SINK_KEY) => DefaultDevice::Sink,
                Some(DEFAULT_AUDIO_SOURCE_KEY) => DefaultDevice::Source,
                _ => return 0,
            };
            let mut state_mut = state_for_property.borrow_mut();
            // value: None means the property was cleared, treated the same as "no default
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
/// and without this a `set_volume` on a hardware sink would be silently dropped (see
/// `write::write_master`). Only `device.api == "alsa"` devices are bound; `v4l2`/`libcamera`
/// devices, also present, have no audio routes at all.
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
