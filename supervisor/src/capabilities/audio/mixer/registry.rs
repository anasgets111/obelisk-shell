//! PipeWire registry plumbing for stream/video nodes, `Audio/Sink`/`Source`, default metadata,
//! ALSA devices, and BlueZ devices' codec profiles. `run` is the thread entry point; callbacks run
//! in its blocking `main_loop.run()`.

use std::cell::{Cell, RefCell};
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

use super::PropsLookup;
use super::state::{
    AudioCommand, AudioState, BluezCard, DEFAULT_AUDIO_SINK_KEY, DEFAULT_AUDIO_SOURCE_KEY, DefaultDevice, DeviceEntry,
    DeviceRoute, MixerState, PrivacySources, device_names,
};
use super::streams::{NodeKind, apply_capture_info_event, apply_info_event, apply_video_info_event, classify};
use super::write::apply_command;

/// Runs the listener until process exit. Sends [`AudioState`] on every relevant
/// node-added/-properties-changed/-removed/param-changed event, and [`PrivacySources`] on the
/// equivalent camera/microphone/screencast events (ADR-0034, ADR-0137). One PipeWire connection
/// serves two capabilities, each publishing only on its own changes. The `pipewire-rs` loop and
/// `Rc` state are single-threaded and non-`Send`: call from
/// `std::thread::spawn`, never async.
///
/// `updates` currently reaches only a `main()` log (ADR-0017); `privacy_updates` feeds
/// `privacy::PrivacyController`, enriching cameras (ADR-0034) and fully describing microphones
/// and screencasts (ADR-0137). ponytail: no shutdown path; `main_loop.run()` ends at process exit
/// until Phase 7/8's reload orchestrator needs a `quit()` trigger. Unreachable PipeWire logs and
/// returns because this subsystem is optional.
pub fn run(
    updates: UnboundedSender<AudioState>,
    privacy_updates: UnboundedSender<PrivacySources>,
    commands: AudioCommandReceiver,
) {
    if let Err(err) = run_inner(updates, privacy_updates, commands) {
        eprintln!("pipewire registry listener stopped: {err}");
    }
}

/// Channel types for § 3.2 audio writes; `run` attaches the receiver to the PipeWire loop.
pub type AudioCommandSender = pw::channel::Sender<AudioCommand>;
pub type AudioCommandReceiver = pw::channel::Receiver<AudioCommand>;

/// Built here, not in `main.rs`, so nothing outside this module needs to name `pipewire::channel`.
pub fn command_channel() -> (AudioCommandSender, AudioCommandReceiver) {
    pw::channel::channel()
}

fn run_inner(
    updates: UnboundedSender<AudioState>,
    privacy_updates: UnboundedSender<PrivacySources>,
    commands: AudioCommandReceiver,
) -> Result<(), pw::Error> {
    pw::init();

    let main_loop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&main_loop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    let state = Rc::new(RefCell::new(MixerState::new(updates, privacy_updates)));

    // Weak: the listener is stored on registry's C object, so a captured strong RegistryRc would
    // keep itself alive forever.
    let registry_weak = registry.downgrade();
    let state_for_global = Rc::clone(&state);
    let state_for_remove = Rc::clone(&state);

    // Publication is held until PipeWire has answered for everything this listener asked for.
    // `core.sync`'s `done` is a barrier: it means every method issued before it, and every event
    // those produced, has been handled.
    //
    // One sync cannot be enough, because the requests that matter are made *by* the events it is
    // waiting on: `on_global` binds a node and subscribes to its params from inside the `global`
    // callback, and a sink's volume arrives on the `param` event that follows. So every global
    // handled while the gate is shut pushes the barrier back behind whatever it just asked for,
    // and only the newest barrier opens the gate. A fixed pair of syncs would leave a sink
    // discovered between the two publishing at volume zero, which is the bug this exists to close.
    //
    // Matched on the returned sequence, never on any `done`: `apply_command`'s writes are methods
    // too, and their completion must not be read as hydration.
    let barrier: Rc<Cell<Option<i32>>> = Rc::new(Cell::new(None));
    let core_weak = core.downgrade();

    /// Re-arms the barrier behind the requests just issued. Opening early is the whole failure
    /// mode, so a `sync` that cannot be issued opens the gate rather than shutting it forever.
    fn rearm(core: &pw::core::CoreWeak, barrier: &Cell<Option<i32>>, state: &RefCell<MixerState>) {
        match core.upgrade().map(|core| core.sync(0)) {
            Some(Ok(seq)) => barrier.set(Some(seq.seq())),
            other => {
                if let Some(Err(err)) = other {
                    eprintln!("audio: core.sync failed ({err}); publishing without waiting for PipeWire to settle");
                }
                let mut state = state.borrow_mut();
                state.hydrated = true;
                state.publish_audio();
                state.publish_privacy();
            }
        }
    }

    let barrier_for_global = Rc::clone(&barrier);
    let core_for_global = core.downgrade();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |obj| {
            if let Some(registry) = registry_weak.upgrade() {
                on_global(&state_for_global, &registry, obj);
            }
            // Bind the read before the call: `rearm` takes the state mutably on its failure path,
            // and a borrow held across it would panic inside a PipeWire callback.
            let gated = !state_for_global.borrow().hydrated;
            if gated {
                rearm(&core_for_global, &barrier_for_global, &state_for_global);
            }
        })
        .global_remove(move |id| {
            let mut state = state_for_remove.borrow_mut();
            state.nodes.remove(&id);
            state.sinks.remove(&id);
            state.sink_nodes.remove(&id);
            state.sources.remove(&id);
            state.source_nodes.remove(&id);
            state.app_props.remove(&id);
            state.bluez_cards.remove(&id);
            state.bluez_devices.remove(&id);
            if state.devices.remove(&id).is_some() {
                // Remove all route indices keyed by this device id, so a reused id inherits none.
                state.device_routes.retain(|&(device_id, _), _| device_id != id);
            }
            if state.metadata_id == Some(id) {
                state.metadata = None;
                state.metadata_id = None;
                state.default_sink_name = None;
                state.default_source_name = None;
            }
            // Audio publishes on every removal (ADR-0034); privacy publishes only for its three
            // node kinds. Sink/metadata cleanup is unconditional because publish_audio recomputes.
            state.apps.remove(&id);
            state.publish_audio();
            // Evaluate all three before the check: `||` could leave one stale map entry.
            let was_camera = state.video_sources.remove(&id).is_some();
            let was_microphone = state.microphones.remove(&id).is_some();
            let was_screencast = state.screencasts.remove(&id).is_some();
            if was_camera || was_microphone || was_screencast {
                state.publish_privacy();
            }
        })
        .register();

    // The gate itself. `error` opens it too: a core error means no further `done` is coming, and a
    // shell whose audio never publishes at all is worse than one that publishes early.
    let state_for_done = Rc::clone(&state);
    let barrier_for_done = Rc::clone(&barrier);
    let state_for_error = Rc::clone(&state);
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id != pw::core::PW_ID_CORE || barrier_for_done.get() != Some(seq.seq()) {
                return;
            }
            // Publish once here even with no sink at all: a machine with no audio hardware still
            // owes the config an answer, and waiting for a nonempty map would never end.
            let mut state = state_for_done.borrow_mut();
            state.hydrated = true;
            state.publish_audio();
            state.publish_privacy();
        })
        .error(move |id, _seq, _res, message| {
            if id != pw::core::PW_ID_CORE {
                return;
            }
            let mut state = state_for_error.borrow_mut();
            if !state.hydrated {
                eprintln!("audio: PipeWire core error before the first snapshot ({message}); publishing what arrived");
                state.hydrated = true;
                state.publish_audio();
                state.publish_privacy();
            }
        })
        .register();

    // Arm the first barrier. Without a single global this is the one that opens the gate, which is
    // what gives a machine with no audio hardware its empty first snapshot.
    rearm(&core_weak, &barrier, &state);

    // Hold it for the loop lifetime; dropping AttachedReceiver detaches the eventfd source and
    // every later command is silently discarded.
    let state_for_command = Rc::clone(&state);
    let _attached_commands =
        commands.attach(main_loop.loop_(), move |command| apply_command(&state_for_command, command));

    main_loop.run();
    Ok(())
}

/// Routes `Node`, `Device`, and `default` `Metadata` globals to their binders; ignores the rest.
fn on_global(state: &Rc<RefCell<MixerState>>, registry: &pw::registry::RegistryRc, obj: &GlobalObject<&DictRef>) {
    match obj.type_ {
        ObjectType::Node => on_node_global(state, registry, obj),
        ObjectType::Device => bind_device(state, registry, obj),
        ObjectType::Metadata => bind_default_metadata(state, registry, obj),
        _ => {}
    }
}

/// `media.class` for output devices. § 2.4 master volume lives here, not on streams.
const AUDIO_SINK: &str = "Audio/Sink";

/// `media.class` for input devices, tracked like sinks for §2.4 source volume/mute. No monitor
/// filter: PulseAudio synthesizes `.monitor` sources, native PipeWire does not, and this `pw-dump`
/// lists one `Audio/Source` beside one `Audio/Sink`.
const AUDIO_SOURCE: &str = "Audio/Source";

/// Metadata name key. A machine advertises several names (`"settings"`, `"default"`,
/// `"route-settings"`); it is absent from `pipewire-rs::keys`. Live `pw-mon` showed the needed
/// global as `metadata.name = "default"`.
const METADATA_NAME: &str = "metadata.name";

/// Classifies `Node` globals. `Audio/Sink`/`Source` use [`bind_device_node`] for `param` volume
/// events; streams/video use `info`, gated to props changes. Stream pids may arrive after `global`.
fn on_node_global(state: &Rc<RefCell<MixerState>>, registry: &pw::registry::RegistryRc, obj: &GlobalObject<&DictRef>) {
    match obj.props.and_then(|props| props.get_prop(*keys::MEDIA_CLASS)) {
        Some(AUDIO_SINK) => return bind_device_node(state, registry, obj, DefaultDevice::Sink),
        Some(AUDIO_SOURCE) => return bind_device_node(state, registry, obj, DefaultDevice::Source),
        _ => {}
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
                    apply_info_event(
                        std::path::Path::new("/proc"),
                        &mut state_mut.apps,
                        node_id,
                        has_props_change,
                        info.props(),
                    );
                    state_mut.publish_audio();
                }
                NodeKind::Video => {
                    apply_video_info_event(&mut state_mut.video_sources, node_id, has_props_change, info.props());
                    state_mut.publish_privacy();
                }
                NodeKind::Microphone | NodeKind::Screencast => {
                    let running = matches!(info.state(), pw::node::NodeState::Running);
                    let apps = match kind {
                        NodeKind::Microphone => &mut state_mut.microphones,
                        _ => &mut state_mut.screencasts,
                    };
                    apply_capture_info_event(apps, node_id, kind, has_props_change, info.props(), running);
                    state_mut.publish_privacy();
                }
            }
        })
        // Per-app streams carry the same channelVolumes/mute pod, parser, and cube-root conversion
        // as master sinks (verified live).
        // Video sources have no useful Props param, so this does not run for them.
        .param(move |_seq, param_type, _index, _next, param| {
            if param_type != pw::spa::param::ParamType::Props {
                return;
            }
            let Some(pod) = param else { return };
            let Ok((_, value)) = PodDeserializer::deserialize_from::<Value>(pod.as_bytes()) else {
                return;
            };
            // As for master sinks, a node can advertise multiple Props objects; missing
            // channelVolumes means this is not a mixer update, not zero volume.
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

/// Binds an `Audio/Sink` or `Audio/Source` and subscribes to its `Props` param, where PipeWire
/// keeps volume/mute. `node.name` is present in the first `global` props, unlike a stream pid.
///
/// `extract_sink_props` rejects the second of a sink's two `Props` objects; treating its missing
/// mixer fields as zero was a shipped bug.
fn bind_device_node(
    state: &Rc<RefCell<MixerState>>,
    registry: &pw::registry::RegistryRc,
    obj: &GlobalObject<&DictRef>,
    kind: DefaultDevice,
) {
    let node_id = obj.id;
    let Some(names) = obj.props.and_then(device_names) else { return };

    // Bind before recording: a failed bind has no Props subscription, and recording its name would
    // choose a node stuck at MasterVolume::default()'s fake 0.0 (ADR-0053 has no unknown sentinel).
    let node: pw::node::Node = match registry.bind(obj) {
        Ok(node) => node,
        Err(err) => {
            eprintln!("audio: failed to bind {kind:?} node {node_id}: {err}");
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
            if let Some(entry) = state_mut.device_entries_mut(kind).get_mut(&node_id) {
                entry.props = Some(raw);
            }
            state_mut.publish_audio();
        })
        // Route data comes from info: global props have media.class/node.name but not device.id or
        // card.profile.device. Reading them at global time would silently drop writes.
        .info(move |info| {
            if !info.change_mask().contains(pw::node::NodeChangeMask::PROPS) {
                return;
            }
            let Some(route) = info.props().and_then(parse_device_route) else { return };
            if let Some(entry) = state_for_info.borrow_mut().device_entries_mut(kind).get_mut(&node_id) {
                entry.route = Some(route);
            }
        })
        .register();

    // Subscribe after listener registration; neither call dispatches until main_loop.run(), so
    // order doesn't matter.
    node.subscribe_params(&[pw::spa::param::ParamType::Props]);

    let mut state_mut = state.borrow_mut();
    state_mut.device_entries_mut(kind).insert(node_id, DeviceEntry { names, props: None, route: None });
    match kind {
        DefaultDevice::Sink => state_mut.sink_nodes.insert(node_id, (node, listener)),
        DefaultDevice::Source => state_mut.source_nodes.insert(node_id, (node, listener)),
    };
    state_mut.publish_audio();
}

/// Hardware route from node props. `device.id` and `card.profile.device` appear together or not
/// at all: this machine's analog output has `51`/`7`, its mic `51`/`0`. `None` means node-owned.
fn parse_device_route(props: &impl PropsLookup) -> Option<DeviceRoute> {
    let device_id = props.get_prop(*keys::DEVICE_ID)?.parse().ok()?;
    let profile_device = props.get_prop(CARD_PROFILE_DEVICE)?.parse().ok()?;
    Some(DeviceRoute { device_id, profile_device })
}

/// Sink property naming its device route. Absent from `pipewire-rs::keys`; live `pw-dump` shows
/// this output's `card.profile.device = 7`, matching `device: 7` on device 49's Route.
const CARD_PROFILE_DEVICE: &str = "card.profile.device";

/// Binds the `Metadata` global named `"default"`, which publishes `default.audio.sink` and
/// `default.audio.source`; other metadata globals stay unbound.
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
            // Ignore other keys, including default.configured.* (which can name a disconnected
            // Bluetooth device); default.audio.* names the real devices.
            let field = match key {
                Some(DEFAULT_AUDIO_SINK_KEY) => DefaultDevice::Sink,
                Some(DEFAULT_AUDIO_SOURCE_KEY) => DefaultDevice::Source,
                _ => return 0,
            };
            let mut state_mut = state_for_property.borrow_mut();
            // A cleared value means no default; resolve_default_device already falls back.
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

/// Binds ALSA `Device` globals for their `Route` active index, solely for writes. Without this,
/// hardware-sink `set_volume` is silently dropped. `v4l2`/`libcamera` devices have no audio routes.
///
/// Enumerated from `info`, not subscribed to: `subscribe_params` delivers only what exists when it
/// is called, and a `Route` appearing later is pushed to nobody. A shell started before its card
/// settles, which is every login, then dropped every write for the session (ADR-0200).
fn bind_device(state: &Rc<RefCell<MixerState>>, registry: &pw::registry::RegistryRc, obj: &GlobalObject<&DictRef>) {
    match obj.props.and_then(|props| props.get_prop(*keys::DEVICE_API)) {
        Some("alsa") => {}
        Some("bluez5") => return bind_bluez_device(state, registry, obj),
        _ => return,
    }
    let device_id = obj.id;
    let device: pw::device::Device = match registry.bind(obj) {
        Ok(device) => device,
        Err(err) => {
            eprintln!("audio: failed to bind ALSA device {device_id}: {err}");
            return;
        }
    };

    // `Rc` so the `info` handler below can reach the proxy it is registered on; `Weak` there,
    // because a strong one is a cycle through the listener the device owns.
    let device = Rc::new(device);
    let device_for_info = Rc::downgrade(&device);
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
        // Re-asks on every param change, including the first `info` a bind always answers with.
        // A profile switch or a plugged headset moves the active route.
        .info(move |info| {
            if !info.change_mask().contains(pw::device::DeviceChangeMask::PARAMS) {
                return;
            }
            if let Some(device) = device_for_info.upgrade() {
                device.enum_params(0, Some(pw::spa::param::ParamType::Route), 0, u32::MAX);
            }
        })
        .register();

    state.borrow_mut().devices.insert(device_id, (device, listener));
}

/// Binds a BlueZ `Device` for its codec profiles, keyed by MAC for the Bluetooth panel's join; its
/// `Route` is never read. Enumerated from `info` (ADR-0200). `Profile` is asked after `EnumProfile`,
/// so its answer ends the enumeration and publishes only on a change. Answers are matched by that
/// order, not by seq: protocol-native's `device_marshal_enum_params` sends
/// `SPA_RESULT_RETURN_ASYNC(msg->seq)` and ignores the caller's (a live `enum_params(7, ..)` came
/// back as 1073741828).
fn bind_bluez_device(
    state: &Rc<RefCell<MixerState>>,
    registry: &pw::registry::RegistryRc,
    obj: &GlobalObject<&DictRef>,
) {
    let Some(mac) = obj.props.and_then(|props| props.get_prop(*keys::DEVICE_NAME)).and_then(master::mac_from_card_name)
    else {
        eprintln!("audio: Bluetooth device {} names no address; its codecs are not tracked", obj.id);
        return;
    };
    let device_id = obj.id;
    let device: pw::device::Device = match registry.bind(obj) {
        Ok(device) => device,
        Err(err) => {
            eprintln!("audio: failed to bind Bluetooth device {device_id} ({mac}): {err}");
            return;
        }
    };

    // `Rc`/`Weak` for the same reason as `bind_device`: the `info` handler needs the proxy it is
    // registered on without a cycle through the listener.
    let device = Rc::new(device);
    let device_for_info = Rc::downgrade(&device);
    let state_for_param = Rc::clone(state);
    let listener = device
        .add_listener_local()
        .param(move |_seq, param_type, _index, _next, param| {
            let Some(pod) = param else { return };
            let Ok((_, value)) = PodDeserializer::deserialize_from::<Value>(pod.as_bytes()) else {
                return;
            };
            let Some(profile) = master::extract_profile(&value) else { return };
            let mut state = state_for_param.borrow_mut();
            let Some(card) = state.bluez_cards.get_mut(&device_id) else { return };
            match param_type {
                pw::spa::param::ParamType::EnumProfile => card.enumerated(profile),
                pw::spa::param::ParamType::Profile if card.finish_enumeration(profile.index) => {
                    state.publish_audio();
                }
                _ => {}
            }
        })
        .info(move |info| {
            if !info.change_mask().contains(pw::device::DeviceChangeMask::PARAMS) {
                return;
            }
            if let Some(device) = device_for_info.upgrade() {
                device.enum_params(0, Some(pw::spa::param::ParamType::EnumProfile), 0, u32::MAX);
                device.enum_params(0, Some(pw::spa::param::ParamType::Profile), 0, u32::MAX);
            }
        })
        .register();

    let mut state = state.borrow_mut();
    state.bluez_cards.insert(device_id, BluezCard { mac, ..BluezCard::default() });
    state.bluez_devices.insert(device_id, (device, listener));
}
