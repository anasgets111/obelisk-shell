//! Applies `AudioCommand`s against live `MixerState` maps. Writes are not optimistic:
//! success returns through `registry`'s `param`/`property` listener, like `wpctl` or a headset
//! button change.

use std::cell::RefCell;
use std::rc::Rc;

use pipewire as pw;

use crate::capabilities::audio::master;

use super::devices::DeviceRoute;
use super::state::{AudioCommand, DEFAULT_AUDIO_SINK_KEY, DEFAULT_AUDIO_SOURCE_KEY, DefaultDevice, MixerState};

/// The `type_` a `default.audio.*` metadata value carries. Read straight off `pw-metadata`'s own
/// dump (`type:'Spa:String:JSON'`) rather than inferred from the value looking like JSON.
const METADATA_JSON_TYPE: &str = "Spa:String:JSON";

/// Applies one [`AudioCommand`] on the PipeWire thread. Every arm resolves its target against live
/// maps first and logs rather than guessing when it cannot. Stale ids matter because PipeWire
/// recycles them. Results return through the same-loop event.
pub(super) fn apply_command(state: &Rc<RefCell<MixerState>>, command: AudioCommand) {
    match command {
        AudioCommand::SetMasterVolume(volume) => set_default_volume(state, DefaultDevice::Sink, volume),
        AudioCommand::SetMasterMuted(muted) => set_default_muted(state, DefaultDevice::Sink, Some(muted)),
        AudioCommand::ToggleMasterMute => set_default_muted(state, DefaultDevice::Sink, None),
        AudioCommand::SetSourceVolume(volume) => set_default_volume(state, DefaultDevice::Source, volume),
        AudioCommand::SetSourceMuted(muted) => set_default_muted(state, DefaultDevice::Source, Some(muted)),
        AudioCommand::ToggleSourceMute => set_default_muted(state, DefaultDevice::Source, None),
        AudioCommand::SetAppVolume { id, volume } => {
            let Some(current) = state.borrow().app_props.get(&id).cloned() else {
                eprintln!("audio: set_app_volume({id}, {volume}) names a stream with no known Props; ignored");
                return;
            };
            let Some(channel_volumes) = master::cubed_channel_volumes(volume, &current.channel_volumes, 1.0) else {
                eprintln!("audio: set_app_volume({id}, {volume}) names a stream that reports no channels; ignored");
                return;
            };
            write_node_props(state, id, Some(channel_volumes), None);
        }
        // Mute needs no channel count; write_node_props still rejects an unbound id.
        AudioCommand::SetAppMuted { id, muted } => write_node_props(state, id, None, Some(muted)),
        AudioCommand::SetDefaultSink(id) => write_default_device(state, DefaultDevice::Sink, id),
        AudioCommand::SetDefaultSource(id) => write_default_device(state, DefaultDevice::Source, id),
        AudioCommand::SetBluetoothProfile { device, index } => write_bluetooth_profile(state, device, index),
    }
}

/// Sets one direction's default volume.
fn set_default_volume(state: &Rc<RefCell<MixerState>>, kind: DefaultDevice, volume: f32) {
    let Some((node_id, current)) = resolve_default(state, kind) else {
        eprintln!("audio: a {kind:?} volume of {volume} has no resolved default device to write to; ignored");
        return;
    };
    let max = if kind == DefaultDevice::Sink { master::SINK_MAX_VOLUME } else { 1.0 };
    let Some(channel_volumes) = master::cubed_channel_volumes(volume, &current.channel_volumes, max) else {
        eprintln!(
            "audio: a {kind:?} volume of {volume} resolved to node {node_id}, which reports no channels; ignored"
        );
        return;
    };
    write_device_volume(state, kind, node_id, Some(channel_volumes), None);
}

/// Pulls the default sink back to the cap when another client (`wpctl set-volume 5%+`) raised it past.
pub(super) fn cap_default_sink(state: &Rc<RefCell<MixerState>>) {
    let Some((_, current)) = resolve_default(state, DefaultDevice::Sink) else { return };
    // The slack stops a loop: 1.5 written as 3.375 reads back as 1.4999999.
    if master::master_volume_from_props(&current).volume > master::SINK_MAX_VOLUME + 1e-3 {
        set_default_volume(state, DefaultDevice::Sink, master::SINK_MAX_VOLUME);
    }
}

/// Sets or toggles one direction's mute. `None` toggles using resolved `Props`; a set needs no
/// channel count because mute carries no `channelVolumes`.
fn set_default_muted(state: &Rc<RefCell<MixerState>>, kind: DefaultDevice, muted: Option<bool>) {
    let Some(node_id) = resolve_default_node(state, kind) else {
        eprintln!("audio: a {kind:?} mute has no resolved default device to write to; ignored");
        return;
    };
    // A toggle needs the current value; an explicit set does not, so only the toggle waits for
    // the first `Props`. Requiring it for both dropped a mute keypress during startup.
    //
    // Not a total fix: a hardware sink whose `Route` has not arrived yet still looks node-owned
    // here, and `write_node_props` addresses the node rather than the device. That window is
    // narrower than the one this closes, and the write is attempted rather than refused.
    let current = || state.borrow().device_entries(kind).get(&node_id)?.props.as_ref().map(|props| !props.mute);
    let Some(muted) = muted.or_else(current) else {
        eprintln!("audio: a {kind:?} mute toggle has no Props on node {node_id} to read; ignored");
        return;
    };
    write_device_volume(state, kind, node_id, None, Some(muted));
}

/// The default node id alone, without waiting for its `Props`. A mute carries no `channelVolumes`,
/// so an explicit set needs only this; [`resolve_default`] is for the paths that read the current
/// value.
fn resolve_default_node(state: &Rc<RefCell<MixerState>>, kind: DefaultDevice) -> Option<u32> {
    let state = state.borrow();
    let entries = state.device_entries(kind);
    master::resolve_default_device(
        state.default_name(kind),
        entries.iter().map(|(&id, entry)| (id, entry.names.node_name.as_str())),
    )
}

/// One direction's node and last-read `Props`, `None` whenever [`master::compute_master`] is.
fn resolve_default(state: &Rc<RefCell<MixerState>>, kind: DefaultDevice) -> Option<(u32, master::RawSinkProps)> {
    let node_id = resolve_default_node(state, kind)?;
    let current = state.borrow().device_entries(kind).get(&node_id)?.props.clone()?;
    Some((node_id, current))
}

/// Writes through the owning object. A hardware sink's node `Props` accepts
/// `pw-cli set-param 59 Props '{ mute: true }'` but does nothing, while a stream write works;
/// the volume lives on the ALSA `Device` `Route`, and node `channelVolumes` only mirrors it.
/// Live `pw-cli set-param 49 Route '{ index: 2, device: 7, props: { channelVolumes: [...] },
/// save: true }'` moved the volume. Use a known route, or node `Props` for virtual/null sinks.
fn write_device_volume(
    state: &Rc<RefCell<MixerState>>,
    kind: DefaultDevice,
    node_id: u32,
    channel_volumes: Option<Vec<f32>>,
    muted: Option<bool>,
) {
    let route = state.borrow().device_entries(kind).get(&node_id).and_then(|entry| entry.route);
    match route {
        Some(route) => write_device_route(state, node_id, route, channel_volumes, muted),
        None => write_node_props(state, node_id, channel_volumes, muted),
    }
}

/// Sends `SPA_PARAM_Route` with nested `props`; `save: true` survives re-plug. The `index` is not
/// guessable: cards publish several routes, and only `Route` reports the active one for a given
/// `card.profile.device`; without it the write goes nowhere or to the wrong route.
fn write_device_route(
    state: &Rc<RefCell<MixerState>>,
    node_id: u32,
    route: DeviceRoute,
    channel_volumes: Option<Vec<f32>>,
    muted: Option<bool>,
) {
    let Some(index) =
        state.borrow().device_routes.get(&(route.device_id, route.profile_device)).map(|active| active.index)
    else {
        eprintln!(
            "audio: sink {node_id} routes through device {} port {}, whose active Route index has not been seen; ignored",
            route.device_id, route.profile_device
        );
        return;
    };
    let object = master::route_object(index, route.profile_device, channel_volumes, muted);
    with_pod(&object, format_args!("a Route object for device {}", route.device_id), |pod| {
        let state = state.borrow();
        let Some((device, _listener)) = state.devices.get(&route.device_id) else {
            eprintln!("audio: device {} is not bound; cannot write its Route", route.device_id);
            return;
        };
        device.set_param(pw::spa::param::ParamType::Route, 0, pod);
    });
}

/// Sends `SPA_PARAM_Profile`, which changes a BlueZ device's codec; the new profile returns through
/// `bind_bluez_device`'s listener.
fn write_bluetooth_profile(state: &Rc<RefCell<MixerState>>, device_id: u32, index: i32) {
    with_pod(&master::profile_object(index), format_args!("a Profile object for device {device_id}"), |pod| {
        let state = state.borrow();
        let Some((device, _listener)) = state.bluez_devices.get(&device_id) else {
            eprintln!("audio: set_bluetooth_profile({device_id}, {index}) names no bound Bluetooth device; ignored");
            return;
        };
        device.set_param(pw::spa::param::ParamType::Profile, 0, pod);
    });
}

/// Sends `SPA_PARAM_Props` to a stream or a node-owned sink/source.
fn write_node_props(
    state: &Rc<RefCell<MixerState>>,
    node_id: u32,
    channel_volumes: Option<Vec<f32>>,
    muted: Option<bool>,
) {
    let object = master::props_object(channel_volumes, muted);
    with_pod(&object, format_args!("a Props object for node {node_id}"), |pod| {
        let state = state.borrow();
        let Some((node, _listener)) = state
            .sink_nodes
            .get(&node_id)
            .or_else(|| state.source_nodes.get(&node_id))
            .or_else(|| state.nodes.get(&node_id))
        else {
            eprintln!("audio: no bound node {node_id} to write Props to; ignored");
            return;
        };
        node.set_param(pw::spa::param::ParamType::Props, 0, pod);
    });
}

/// Serializes `object` and hands the pod to `send`, logging `what` when either step fails. The
/// bytes live only in this frame: `Pod::from_bytes` borrows them and `set_param` reads that borrow
/// into C, so dropping the `Vec` before the send would hand PipeWire a dangling pointer.
fn with_pod(object: &pw::spa::pod::Value, what: std::fmt::Arguments, send: impl FnOnce(&pw::spa::pod::Pod)) {
    let Some(bytes) = master::serialize_props(object) else {
        eprintln!("audio: failed to serialize {what}; ignored");
        return;
    };
    let Some(pod) = pw::spa::pod::Pod::from_bytes(&bytes) else {
        eprintln!("audio: serialized {what} did not read back as a pod; ignored");
        return;
    };
    send(pod);
}

/// Writes `audio:set_default_sink/source(id)`. Metadata names devices by `node.name`, so an id
/// read from the device array is translated through its tracked entry. System-wide defaults use `0`
/// (`pw-metadata` shows `update: id:0 key:'default.audio.sink'`).
fn write_default_device(state: &Rc<RefCell<MixerState>>, kind: DefaultDevice, id: u32) {
    let state = state.borrow();
    let node_name = state.device_entries(kind).get(&id).map(|entry| entry.names.node_name.clone());
    let Some(node_name) = node_name else {
        eprintln!("audio: no tracked {kind:?} with registry id {id}; ignored");
        return;
    };
    let Some((metadata, _listener)) = state.metadata.as_ref() else {
        eprintln!("audio: the `default` metadata object is not bound; cannot set the default {kind:?}");
        return;
    };
    let key = match kind {
        DefaultDevice::Sink => DEFAULT_AUDIO_SINK_KEY,
        DefaultDevice::Source => DEFAULT_AUDIO_SOURCE_KEY,
    };
    let value = serde_json::json!({ "name": node_name }).to_string();
    metadata.set_property(0, key, Some(METADATA_JSON_TYPE), Some(&value));
}
