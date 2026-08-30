//! The write half of § 3.2's audio actions: applies one `AudioCommand` against the live
//! `MixerState` maps `registry` populated. No optimistic state update anywhere here -- a
//! successful write comes back as a `param`/`property` event on `registry`'s own listener,
//! which publishes the new value, same as a change from `wpctl` or a headset button.

use std::cell::RefCell;
use std::rc::Rc;

use pipewire as pw;

use crate::audio::master;

use super::state::{AudioCommand, DefaultDevice, MixerState, SinkRoute, DEFAULT_AUDIO_SINK_KEY, DEFAULT_AUDIO_SOURCE_KEY};

/// The `type_` a `default.audio.*` metadata value carries. Read straight off `pw-metadata`'s own
/// dump (`type:'Spa:String:JSON'`) rather than inferred from the value looking like JSON.
const METADATA_JSON_TYPE: &str = "Spa:String:JSON";

/// Applies one [`AudioCommand`] on the PipeWire loop's own thread. Every arm resolves its target
/// against the live maps first and logs rather than guessing when it cannot: a registry id a
/// config read moments ago can name a different node by the time the command lands, because
/// PipeWire recycles ids.
///
/// No optimistic state update anywhere here. A successful write comes back as a `param` or
/// `property` event on the same loop, which publishes the new value -- also what makes a volume
/// changed by `wpctl` or a headset button look identical to one this shell asked for.
pub(super) fn apply_command(state: &Rc<RefCell<MixerState>>, command: AudioCommand) {
    match command {
        AudioCommand::SetMasterVolume(volume) => {
            let Some((node_id, current)) = resolve_master(state) else {
                eprintln!("audio: set_volume({volume}) has no resolved default sink to write to; ignored");
                return;
            };
            let Some(channel_volumes) = master::cubed_channel_volumes(volume, current.channel_volumes.len()) else {
                eprintln!("audio: set_volume({volume}) resolved to sink {node_id}, which reports no channels; ignored");
                return;
            };
            write_master(state, node_id, Some(channel_volumes), None);
        }
        AudioCommand::SetMasterMuted(muted) => {
            // Only the node id is needed: a mute write carries no channelVolumes, so unlike
            // SetMasterVolume it does not need to know how many channels the sink has.
            let Some((node_id, _)) = resolve_master(state) else {
                eprintln!("audio: set_muted({muted}) has no resolved default sink to write to; ignored");
                return;
            };
            write_master(state, node_id, None, Some(muted));
        }
        AudioCommand::ToggleMasterMute => {
            let Some((node_id, current)) = resolve_master(state) else {
                eprintln!("audio: toggle_mute has no resolved default sink to write to; ignored");
                return;
            };
            let muted = !current.mute;
            write_master(state, node_id, None, Some(muted));
        }
        AudioCommand::SetAppVolume { id, volume } => {
            let Some(current) = state.borrow().app_props.get(&id).cloned() else {
                eprintln!("audio: set_app_volume({id}, {volume}) names a stream with no known Props; ignored");
                return;
            };
            let Some(channel_volumes) = master::cubed_channel_volumes(volume, current.channel_volumes.len()) else {
                eprintln!("audio: set_app_volume({id}, {volume}) names a stream that reports no channels; ignored");
                return;
            };
            write_node_props(state, id, Some(channel_volumes), None);
        }
        // No app_props lookup, same reason SetMasterMuted has none: a mute write needs no
        // channel count. write_node_props already refuses an id with no bound node.
        AudioCommand::SetAppMuted { id, muted } => write_node_props(state, id, None, Some(muted)),
        AudioCommand::SetDefaultSink(id) => write_default_device(state, DefaultDevice::Sink, id),
        AudioCommand::SetDefaultSource(id) => write_default_device(state, DefaultDevice::Source, id),
    }
}

/// The default sink's node id and its last-read `Props`, or `None` when no sink has resolved yet
/// (the same window `master::compute_master` falls back over). Cloned out from behind the
/// `RefCell`, not held across the write, because the write borrows the same state again.
fn resolve_master(state: &Rc<RefCell<MixerState>>) -> Option<(u32, master::RawSinkProps)> {
    let state = state.borrow();
    let node_id = master::resolve_default_device(state.default_sink_name.as_deref(), state.sinks.iter().map(|(&id, sink)| (id, sink.names.node_name.as_str())))?;
    let current = state.sinks.get(&node_id)?.props.clone()?;
    Some((node_id, current))
}

/// Writes the master sink's volume and mute, through whichever object actually owns them.
///
/// **A sink backed by hardware does not own its own volume, and writing its node's `Props`
/// succeeds and does nothing.** Found by running it, not by reading: `pw-cli set-param 59 Props
/// '{ mute: true }'` against this machine's analog sink was accepted and had no effect, while the
/// same write against a stream node worked immediately. The volume lives on the ALSA `Device`'s
/// `Route` param, which the node's `channelVolumes` only mirrors, restored the moment anything
/// writes over it. `pw-cli set-param 49 Route '{ index: 2, device: 7, props: { channelVolumes:
/// [...] }, save: true }'` moved the volume; that is what this builds.
///
/// So the route path is used whenever one is known, and the node path is the fallback for a sink
/// with no device behind it (a virtual or null sink), where the node really is the owner.
fn write_master(state: &Rc<RefCell<MixerState>>, node_id: u32, channel_volumes: Option<Vec<f32>>, muted: Option<bool>) {
    let route = state.borrow().sinks.get(&node_id).and_then(|sink| sink.route);
    match route {
        Some(route) => write_device_route(state, node_id, route, channel_volumes, muted),
        None => write_node_props(state, node_id, channel_volumes, muted),
    }
}

/// Sends one `SPA_PARAM_Route` object to the ALSA device behind a sink, carrying the volume and
/// mute in its nested `props`. `save: true` matches what every other mixer writes, and is what
/// makes the setting survive the device being re-plugged.
///
/// The route `index` is not guessable: a card publishes several routes and only its own `Route`
/// param says which one is active for a given `card.profile.device`. Without that index the
/// write goes to the wrong route or to none.
fn write_device_route(state: &Rc<RefCell<MixerState>>, node_id: u32, route: SinkRoute, channel_volumes: Option<Vec<f32>>, muted: Option<bool>) {
    let Some(index) = state.borrow().device_routes.get(&(route.device_id, route.profile_device)).copied() else {
        eprintln!("audio: sink {node_id} routes through device {} port {}, whose active Route index has not been seen; ignored", route.device_id, route.profile_device);
        return;
    };
    let Some(bytes) = master::serialize_props(&master::route_object(index, route.profile_device, channel_volumes, muted)) else {
        eprintln!("audio: failed to serialize a Route object for device {}; ignored", route.device_id);
        return;
    };
    let Some(pod) = pw::spa::pod::Pod::from_bytes(&bytes) else {
        eprintln!("audio: serialized Route for device {} did not read back as a pod; ignored", route.device_id);
        return;
    };
    let state = state.borrow();
    let Some((device, _listener)) = state.devices.get(&route.device_id) else {
        eprintln!("audio: device {} is not bound; cannot write its Route", route.device_id);
        return;
    };
    device.set_param(pw::spa::param::ParamType::Route, 0, pod);
}

/// Sends one `SPA_PARAM_Props` object to a node. The write that works for a stream, and the
/// fallback for a sink with no hardware device behind it (see [`write_master`]).
///
/// The serialized bytes are held in a local for the whole call: `Pod::from_bytes` borrows them,
/// and `set_param` reads through that borrow into C, so letting the `Vec` drop early would hand
/// PipeWire a dangling pointer.
fn write_node_props(state: &Rc<RefCell<MixerState>>, node_id: u32, channel_volumes: Option<Vec<f32>>, muted: Option<bool>) {
    let Some(bytes) = master::serialize_props(&master::props_object(channel_volumes, muted)) else {
        eprintln!("audio: failed to serialize a Props object for node {node_id}; ignored");
        return;
    };
    let Some(pod) = pw::spa::pod::Pod::from_bytes(&bytes) else {
        eprintln!("audio: serialized Props for node {node_id} did not read back as a pod; ignored");
        return;
    };
    let state = state.borrow();
    let Some((node, _listener)) = state.sink_nodes.get(&node_id).or_else(|| state.nodes.get(&node_id)) else {
        eprintln!("audio: no bound node {node_id} to write Props to; ignored");
        return;
    };
    node.set_param(pw::spa::param::ParamType::Props, 0, pod);
}

/// `audio:set_default_sink(id)`/`set_default_source(id)`. The metadata keys name a device by
/// `node.name`, not by registry id, so the id a config read off § 2.4's array is translated back
/// through the same entry that produced it. `subject` is `0`, what a system-wide default is
/// keyed under (`pw-metadata`'s own dump shows `update: id:0 key:'default.audio.sink'`).
fn write_default_device(state: &Rc<RefCell<MixerState>>, kind: DefaultDevice, id: u32) {
    let state = state.borrow();
    let node_name = match kind {
        DefaultDevice::Sink => state.sinks.get(&id).map(|sink| sink.names.node_name.clone()),
        DefaultDevice::Source => state.sources.get(&id).map(|names| names.node_name.clone()),
    };
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
