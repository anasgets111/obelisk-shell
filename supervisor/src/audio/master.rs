//! Master output volume/mute (§ 2.4), computed from the default sink's `SPA_PARAM_Props` pod,
//! plus the metadata lookup that decides which sink is "the default" one.
//!
//! Master volume is not a node property -- it isn't in a node's `info().props()` dict at all
//! (that's what `mixer.rs`'s `AppStream` tracking reads, per app streams having no master
//! concept). It lives in the sink node's `SPA_PARAM_Props` param, which only arrives through a
//! `param` event after `Node::subscribe_params(&[ParamType::Props])`, not through `info`.
//! Verified live with a throwaway probe binary linking `pipewire-rs` 0.10.1 against this
//! machine's running `pipewire`/`wireplumber`: subscribing to `ParamType::Props` on the default
//! sink node (id 59, `alsa_output.pci-0000_00_1f.3.analog-stereo`) delivered a `param` event
//! whose pod, parsed with `PodDeserializer::deserialize_from::<Value>`, was a
//! `Value::Object(Object { properties: [..] })` containing, among others:
//! `Property { key: 65539, value: Float(1.0) }` (`SPA_PROP_volume`),
//! `Property { key: 65540, value: Bool(false) }` (`SPA_PROP_mute`), and
//! `Property { key: 65544, value: ValueArray(Float([0.027004944, 0.027004944])) }`
//! (`SPA_PROP_channelVolumes`). Key numbers checked against this build's own bindgen output
//! (`libspa-sys` 0.10.1's generated `bindings.rs`, which defines
//! `SPA_PROP_volume = 65539`, `SPA_PROP_mute = 65540`, `SPA_PROP_channelVolumes = 65544`) rather
//! than hand-computed from `/usr/include/spa-0.2/spa/param/props.h`'s enum ordering, which this
//! codebase found gives the wrong numbers (there are more `SPA_PROP_START_Audio`-relative
//! entries between `frequency` and `volume` than the header comment order alone suggests).
//! `libspa` 0.10.1 has no typed wrapper for these ids (unlike `spa::param::format::FormatProperties`
//! for format params) -- checked by grepping its whole source tree for `SPA_PROP_` -- so
//! `mixer.rs`'s pod-parsing code below matches on the raw `spa_sys::SPA_PROP_*` constants.
//!
//! **`volume` vs `channelVolumes`, and linear vs cubic**: the scalar `SPA_PROP_volume` this
//! machine reported was `1.0` (PipeWire's untouched default, "no attenuation") even though the
//! sink was actually set to 30%. `SPA_PROP_channelVolumes` was `[0.027004944, 0.027004944]`,
//! which is what `wpctl`/`pactl`/WirePlumber actually set and display, matching `build-steps.md`'s
//! own precedent of treating PulseAudio-API behavior as authoritative. Confirmed against
//! `wpctl get-volume @DEFAULT_AUDIO_SINK@`, which printed `Volume: 0.30`: `0.3^3 = 0.027`, which
//! is `channelVolumes`' value to five decimal places. `spa/param/props.h`'s own comment calls
//! `channelVolumes` "linear" ("0.0 is silence, 1.0 is without attenuation"), but that means
//! linear in the DSP/amplitude-gain sense, not perceptually linear -- it is the *cube* of the
//! number a user recognizes as "the volume", and pactl/wpctl/WirePlumber display its cube root.
//! Toggling `wpctl set-mute @DEFAULT_AUDIO_SINK@ 1` and re-probing confirmed `SPA_PROP_mute`
//! flips independently of `channelVolumes` (the array is untouched by mute), matching § 2.4
//! treating `volume` and `muted` as independent fields.
//!
//! So [`master_volume_from_props`] takes the maximum of `channelVolumes` (a stereo pair is
//! usually equal, but nothing guarantees that) and cube-roots it, rather than reading the scalar
//! `volume` key or reporting `channelVolumes` unconverted.

use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{Object, Property, Value, ValueArray};
use pipewire::spa::sys as spa_sys;
use serde::Serialize;

/// Master output volume/mute, as § 2.4 specifies (`audio.volume`, `audio.muted`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct MasterVolume {
    pub volume: f32,
    pub muted: bool,
}

impl Default for MasterVolume {
    /// Only ever observed before the default sink's first `Props` param event has arrived.
    /// ponytail: `subscribe_params` delivered that first event on the very next main-loop
    /// iteration in this codebase's own live probe, mirroring the same "the first event always
    /// carries full current state" contract `mixer.rs`'s module doc comment already established
    /// (and verified live, not just assumed) for a node's `info` event -- so this default is a
    /// startup value observed for at most one loop iteration, not a steady-state one. If that
    /// ever stops holding (e.g. a sink whose `Props` param genuinely never fires), the fix is an
    /// explicit "not yet known" state rather than silently reporting this default forever.
    fn default() -> Self {
        Self { volume: 0.0, muted: false }
    }
}

/// The `SPA_PROP_mute`/`SPA_PROP_channelVolumes` values `mixer.rs` pulls out of one sink node's
/// `Props` param pod -- everything [`master_volume_from_props`] needs, already stripped of the
/// pod/`Value` machinery so it stays testable without constructing a real pod.
#[derive(Debug, Clone, PartialEq)]
pub struct RawSinkProps {
    pub mute: bool,
    pub channel_volumes: Vec<f32>,
}

/// Pulls [`RawSinkProps`] out of a sink's `Props` param, already deserialized into `libspa`'s
/// generic pod [`Value`] by `mixer.rs` (via `PodDeserializer::deserialize_from::<Value>`). A
/// `Props` param is always `Value::Object` on this PipeWire version -- checked live, see the
/// module doc comment's probe output.
///
/// A sink advertises *two* separate objects under `ParamType::Props`, not one -- confirmed live
/// with a throwaway probe subscribed to node 59: `subscribe_params` immediately delivered two
/// `param` events back to back, `index=0` carrying the mixer keys this function reads
/// (`SPA_PROP_volume`/`mute`/`channelVolumes`/...) and `index=1` carrying unrelated ALSA device
/// settings (`device`, `deviceName`, `cardName`, `SPA_PROP_latencyOffsetNsec`, a nested `params`
/// struct) with none of the mixer keys at all. Both fire again, still as a pair, on every mixer
/// change too (`wpctl set-volume`/`set-mute` each produced the same index=0-then-index=1
/// sequence). Treating "no `channelVolumes` key" as "channel_volumes: vec![]" instead of "this
/// isn't the volume-bearing object, ignore it" is what previously made every `index=1` event
/// overwrite `mixer.rs`'s tracked `sink_volumes` entry with a zeroed `MasterVolume` moments after
/// the correct `index=0` event had just set it -- publish 1 correct, publish 2 (from `index=1`,
/// no code changed, no mute/volume actually happened) wrong. So the presence of
/// `SPA_PROP_channelVolumes` is now the signal for "this is a real mixer update"; anything
/// without it returns `None` and `mixer.rs`'s `param` callback leaves `sink_volumes` untouched
/// for that event, exactly like a non-PROPS `info` event already leaves `AudioApps` untouched
/// (see the module doc comment). `SPA_PROP_mute` defaults to unmuted only when `channelVolumes`
/// *is* present but `mute` itself happens not to be -- never observed live, but there's no
/// contract guaranteeing it's always paired, so that gap alone still isn't treated as "ignore
/// this object".
pub fn extract_sink_props(value: &Value) -> Option<RawSinkProps> {
    let Value::Object(object) = value else { return None };

    let mut mute = false;
    let mut channel_volumes = None;
    for property in &object.properties {
        match (property.key, &property.value) {
            (key, Value::Bool(value)) if key == spa_sys::SPA_PROP_mute => mute = *value,
            (key, Value::ValueArray(pipewire::spa::pod::ValueArray::Float(values))) if key == spa_sys::SPA_PROP_channelVolumes => {
                channel_volumes = Some(values.clone());
            }
            _ => {}
        }
    }
    Some(RawSinkProps { mute, channel_volumes: channel_volumes? })
}

/// Converts one sink's raw `Props` values into the `MasterVolume` § 2.4 wants: the cube root of
/// the loudest channel (see the module doc comment for why cube root, and why the max rather than
/// e.g. the first channel -- a stereo pair's two channels aren't guaranteed equal, and "the
/// volume" a user means is the louder side, matching what a linear fader bound to the max would
/// show). An empty `channel_volumes` (a pod that somehow carried no array at all) reports `0.0`
/// rather than panicking on an empty-iterator fold.
pub fn master_volume_from_props(props: &RawSinkProps) -> MasterVolume {
    let peak_linear = props.channel_volumes.iter().copied().fold(0.0_f32, f32::max);
    MasterVolume { volume: peak_linear.cbrt(), muted: props.mute }
}

/// Parses a PipeWire metadata `default.audio.sink` or `default.audio.source` property value --
/// raw JSON text of the shape `{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}` -- into the
/// device's `node.name`. One function for both keys because both carry the identical shape,
/// confirmed on the same live `pw-metadata` dump: `default.audio.source` reads
/// `{"name":"alsa_input.pci-0000_00_1f.3.analog-stereo"}`.
/// Confirmed this is the real wire shape with `pw-metadata`'s dump output on this machine
/// (`update: id:0 key:'default.audio.sink' value:'{"name":"alsa_output...stereo"}'
/// type:'Spa:String:JSON'`), not assumed from the `Spa:String:JSON` type name alone. `None` for
/// anything that isn't that shape (missing property, malformed JSON, or a `name` that isn't a
/// string) -- callers treat that the same as "no default known yet".
pub fn parse_default_device_name(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    value.get("name")?.as_str().map(str::to_string)
}

/// Which tracked device node id is the default right now, given the name PipeWire's metadata
/// reports (if any) and the `node_id -> node.name` map of `Audio/Sink` (or `Audio/Source`) nodes
/// `mixer.rs` has seen `global` events for. The metadata key names a device by `node.name`, not
/// by registry id (see the module doc comment), so this matches by name rather than assuming id 0
/// or the like means anything.
///
/// Public and device-kind-agnostic since § 2.4's `sinks`/`sources` arrays landed: both need the
/// same "which of these is the active one" answer, and the rule is identical for each.
///
/// ponytail: `default_name` can name a sink `sink_names` has no entry for, for two different
/// reasons, and this function can't tell which one it's looking at:
///
/// 1. Startup: the `default` metadata object hasn't sent a `default.audio.sink` property yet
///    this run, or it has but the sink node's own `global` event hasn't arrived yet -- the two
///    are independent registry events with no ordering guarantee between them.
/// 2. Removal: the sink `default_name` names was just unplugged. `mixer.rs`'s `global_remove`
///    clears that sink's `sink_names`/`sink_volumes`/`sink_nodes` entries on removal, but only
///    clears `default_sink_name` itself when the *metadata* global goes away, not when the sink
///    it happens to name does -- those are two different registry ids removed by two unrelated
///    events, and PipeWire doesn't push a fresh `default.audio.sink` metadata update in the same
///    breath as the node removal. So for however long it takes PipeWire to pick and announce a
///    new default, `default_name` keeps pointing at a sink this function can no longer find.
///
/// Both cases fall back the same way: the numerically lowest remaining tracked sink node id,
/// which is only correct on a machine with exactly one sink -- on one with more than one
/// (built-in speakers plus a Bluetooth headset, say), a query made during either window can
/// report the wrong device's volume (unplugging a headset at 90% can briefly show the built-in
/// speakers' volume instead, or vice versa). Both windows are brief and bounded by an event
/// PipeWire sends promptly (the node's first `Props` param, or the next `default.audio.sink`
/// metadata push), and reporting another real sink's volume for a moment is no worse than the
/// `MasterVolume::default()` a totally untracked sink falls back to -- so this is left as is
/// rather than restructured. The upgrade path, if either window ever proves too wide in
/// practice, is to hold the previous resolved master id across an unresolved query instead of
/// guessing by id.
pub fn resolve_default_device<'a>(default_name: Option<&str>, names: impl Iterator<Item = (u32, &'a str)>) -> Option<u32> {
    let mut matched: Option<u32> = None;
    let mut lowest: Option<u32> = None;
    for (id, name) in names {
        if default_name == Some(name) {
            // Lowest id wins a tie rather than whichever the map happened to yield first. Two
            // devices can share a `node.name` while hardware reconnects, and `HashMap` iteration
            // order is not an order: without this, which device § 2.4's array marks `active`
            // would change between two publishes of an unchanged registry.
            matched = Some(matched.map_or(id, |current| current.min(id)));
        }
        lowest = Some(lowest.map_or(id, |low| low.min(id)));
    }
    matched.or(lowest)
}

/// Combines [`resolve_default_device`] and the tracked per-sink [`RawSinkProps`] into the one
/// value `mixer.rs` publishes -- [`MasterVolume::default`] (see its doc comment) if no sink is
/// resolved yet, or a resolved sink's own `Props` haven't been parsed into `sink_props` yet
/// (the two maps update from separate PipeWire events and aren't guaranteed to catch up in the
/// same tick).
///
/// `mixer.rs` keeps the whole [`RawSinkProps`], not the derived [`MasterVolume`], because the
/// write path needs the channel count: a `channelVolumes` array set with the wrong arity is
/// silently ignored by PipeWire, and the only honest source for how many channels a device has
/// is the array it last reported.
pub fn compute_master<'a>(default_name: Option<&str>, names: impl Iterator<Item = (u32, &'a str)>, props: impl Fn(u32) -> Option<RawSinkProps>) -> MasterVolume {
    resolve_default_device(default_name, names).and_then(props).map(|raw| master_volume_from_props(&raw)).unwrap_or_default()
}

/// The inverse of [`master_volume_from_props`]'s cube root, spread across `channels` channels.
/// PipeWire stores `channelVolumes` cubed, so a linear `0.3` a config asks for is written as
/// `0.027`; skipping this writes 30% as 67%, which is the same conversion `audio` already got
/// wrong once in the read direction (docs/adr/0053's own note about `wpctl` disagreeing).
///
/// `linear` is clamped to `[0.0, 1.0]` here rather than at the parse, matching
/// `hardware::scale::raw_from_percent`'s own "clamp once, in the place that needs the bound"
/// convention: § 3.2 states the range and this is the one function whose output leaves the
/// process.
///
/// Every channel gets the same value. § 2.4 has one volume per device, not one per channel, so
/// setting a volume through this capability necessarily flattens a device a user had balanced
/// unevenly. That is the shape of the spec, and it is worth knowing before wondering where a
/// left/right balance went.
pub fn cubed_channel_volumes(linear: f32, channels: usize) -> Option<Vec<f32>> {
    if channels == 0 {
        // `extract_sink_props` accepts a structurally valid but empty `channelVolumes` array, and
        // `master_volume_from_props` reads that as `0.0` on purpose. The write direction has no
        // such honest answer: an empty array is a `Props` object PipeWire accepts and ignores, so
        // a `set_volume` would report success and change nothing. Refusing lets the caller say so.
        return None;
    }
    let clamped = linear.clamp(0.0, 1.0);
    Some(vec![clamped * clamped * clamped; channels])
}

/// Builds the `SPA_PARAM_Props` object `Node::set_param` takes, carrying only the fields the
/// caller is actually changing. A partial object is applied as a partial update, confirmed live
/// with `pw-cli set-param <stream> Props '{ mute: true }'`, which muted the stream and left its
/// volume alone.
///
/// Only the changed field, and not the pair, because sending the pair loses a write. Both values
/// reach this capability through PipeWire's own `param` event and nothing here updates them
/// optimistically, so two commands in the same tick both read the state from before either of
/// them: `set_app_volume(id, 0.42)` followed immediately by `set_app_muted(id, true)` sent the
/// second object with the volume from before the first, and put it back to 1.0. Observed on a
/// live session, not reasoned about.
pub fn props_object(channel_volumes: Option<Vec<f32>>, muted: Option<bool>) -> Value {
    props_object_with_id(spa_sys::SPA_PARAM_Props, channel_volumes, muted)
}

/// The same object under a caller-chosen param id. A `Props` object nested inside a `Route`
/// carries `SPA_PARAM_Route` as its id, not `SPA_PARAM_Props`: read straight off a live `pw-cli
/// enum-params <device> Route`, whose nested object prints as `type Spa:Pod:Object:Param:Props
/// (262146), id Spa:Enum:ParamId:Route (13)`. Building it with the standalone id instead is a pod
/// PipeWire accepts and ignores, which is the same silent failure a node-level write already is.
fn props_object_with_id(id: u32, channel_volumes: Option<Vec<f32>>, muted: Option<bool>) -> Value {
    let mut properties = Vec::new();
    if let Some(muted) = muted {
        properties.push(Property::new(spa_sys::SPA_PROP_mute, Value::Bool(muted)));
    }
    if let Some(channel_volumes) = channel_volumes {
        properties.push(Property::new(spa_sys::SPA_PROP_channelVolumes, Value::ValueArray(ValueArray::Float(channel_volumes))));
    }
    Value::Object(Object { type_: spa_sys::SPA_TYPE_OBJECT_Props, id, properties })
}

/// Serializes [`props_object`]'s output into the raw pod bytes `Pod::from_bytes` reads back.
/// Separate from `props_object` so the object shape stays unit-testable without going through a
/// serializer, and so `mixer.rs` holds the bytes alive for exactly as long as the `&Pod` borrowed
/// from them is in use.
pub fn serialize_props(object: &Value) -> Option<Vec<u8>> {
    let (cursor, _) = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), object).ok()?;
    Some(cursor.into_inner())
}

/// Builds the `SPA_PARAM_Route` object a hardware sink's volume actually has to be written
/// through (see `mixer::write_master` for how that was found, and why a node write is silently
/// ignored). `index` names which of the card's routes to write; `profile_device` is the
/// `card.profile.device` the sink node reports and the `device` field a `Route` matches on.
///
/// `save: true` matches every other mixer's write and is what makes the setting survive the
/// device being re-plugged. The volume and mute ride in a nested `Props` object, which is the
/// shape a live `pw-cli enum-params <device> Route` shows on this machine.
pub fn route_object(index: i32, profile_device: i32, channel_volumes: Option<Vec<f32>>, muted: Option<bool>) -> Value {
    Value::Object(Object {
        type_: spa_sys::SPA_TYPE_OBJECT_ParamRoute,
        id: spa_sys::SPA_PARAM_Route,
        properties: vec![
            Property::new(spa_sys::SPA_PARAM_ROUTE_index, Value::Int(index)),
            Property::new(spa_sys::SPA_PARAM_ROUTE_device, Value::Int(profile_device)),
            Property::new(spa_sys::SPA_PARAM_ROUTE_props, props_object_with_id(spa_sys::SPA_PARAM_Route, channel_volumes, muted)),
            Property::new(spa_sys::SPA_PARAM_ROUTE_save, Value::Bool(true)),
        ],
    })
}

/// Pulls `(card.profile.device, route index)` out of one `Route` object a device published. That
/// pair is the whole reason devices are tracked: a card advertises several routes and only this
/// param says which index is currently active for a given port, and a write to the wrong index
/// goes nowhere.
///
/// `None` for an object missing either field, which is how the `EnumRoute`-shaped entries and
/// anything else arriving under a different param are skipped rather than misread.
pub fn extract_route_target(value: &Value) -> Option<(i32, i32)> {
    let Value::Object(object) = value else { return None };
    let mut index = None;
    let mut profile_device = None;
    for property in &object.properties {
        match (property.key, &property.value) {
            (spa_sys::SPA_PARAM_ROUTE_index, Value::Int(value)) => index = Some(*value),
            (spa_sys::SPA_PARAM_ROUTE_device, Value::Int(value)) => profile_device = Some(*value),
            _ => {}
        }
    }
    Some((profile_device?, index?))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// Adapts a plain `id -> node.name` fixture map into the iterator both functions take. They
    /// take an iterator rather than a map because `mixer.rs` keeps one entry struct per device,
    /// not a map of names, and building a throwaway `HashMap` on every publish to call these
    /// would be an allocation per PipeWire event.
    fn names(map: &HashMap<u32, String>) -> impl Iterator<Item = (u32, &str)> {
        map.iter().map(|(&id, name)| (id, name.as_str()))
    }

    // ---- extract_sink_props ----

    /// The shape observed live for the default sink's `Props` param (see the module doc
    /// comment's probe output), trimmed to just the three keys this module reads.
    fn sample_props_object() -> Value {
        Value::Object(pipewire::spa::pod::Object {
            type_: 262146,
            id: 2,
            properties: vec![
                pipewire::spa::pod::Property::new(spa_sys::SPA_PROP_volume, Value::Float(1.0)),
                pipewire::spa::pod::Property::new(spa_sys::SPA_PROP_mute, Value::Bool(false)),
                pipewire::spa::pod::Property::new(
                    spa_sys::SPA_PROP_channelVolumes,
                    Value::ValueArray(pipewire::spa::pod::ValueArray::Float(vec![0.027004944, 0.027004944])),
                ),
            ],
        })
    }

    #[test]
    fn extract_sink_props_reads_mute_and_channel_volumes_from_a_real_props_object() {
        let extracted = extract_sink_props(&sample_props_object()).expect("should parse as sink props");
        assert!(!extracted.mute);
        assert_eq!(extracted.channel_volumes, vec![0.027004944, 0.027004944]);
    }

    #[test]
    fn extract_sink_props_ignores_the_unused_scalar_volume_key() {
        // SPA_PROP_volume (Float(1.0) above) is PipeWire's own default, not what pactl/wpctl
        // show -- RawSinkProps must not have a field for it at all, and extraction must not
        // trip over the unmatched key.
        let extracted = extract_sink_props(&sample_props_object()).unwrap();
        assert_eq!(extracted.channel_volumes.len(), 2, "the scalar volume key must not leak into channel_volumes");
    }

    #[test]
    fn extract_sink_props_rejects_a_non_object_pod() {
        assert_eq!(extract_sink_props(&Value::Float(1.0)), None);
    }

    #[test]
    fn extract_sink_props_defaults_mute_when_the_key_is_absent() {
        let value = Value::Object(pipewire::spa::pod::Object {
            type_: 262146,
            id: 2,
            properties: vec![pipewire::spa::pod::Property::new(
                spa_sys::SPA_PROP_channelVolumes,
                Value::ValueArray(pipewire::spa::pod::ValueArray::Float(vec![1.0])),
            )],
        });
        let extracted = extract_sink_props(&value).unwrap();
        assert!(!extracted.mute);
    }

    /// Regression test for the bug the coordinator reported live: `wpctl get-volume` said 0.45,
    /// the supervisor's very next `oblisk.audio` push after the correct one reported `0.0`, with
    /// no app-stream change and no real volume/mute change in between. Root cause, found with a
    /// throwaway probe subscribed to the same node this codebase binds: a sink's `Props` param
    /// isn't one object, it's two, delivered back to back on every subscription *and* on every
    /// real mixer change -- `index=0` (the one [`sample_props_object`] models) carries
    /// `volume`/`mute`/`channelVolumes`; `index=1` carries unrelated ALSA device settings and has
    /// none of those keys. This is `index=1`'s real shape, keys taken verbatim from that probe's
    /// output (`keys=[257, 258, 261, 65550, 524289]`) rather than invented -- 257/258/261 are
    /// `device`/`deviceName`/`cardName` (all `String`), 65550 is `SPA_PROP_latencyOffsetNsec`
    /// (`Long`), 524289 is a nested `params` `Struct`, none of which this test needs to represent
    /// faithfully since the point is that `SPA_PROP_channelVolumes` is absent. Before the fix,
    /// this returned `Some(RawSinkProps { mute: false, channel_volumes: vec![] })` -- a
    /// successfully "parsed" zero that `mixer.rs`'s `param` callback then wrote straight into
    /// `sink_volumes`, clobbering the value `index=0`'s event had just set. It must return `None`
    /// instead, so that callback's existing `let Some(raw) = ... else { return }` skips the write
    /// entirely and leaves the previously tracked volume alone.
    #[test]
    fn extract_sink_props_ignores_the_alsa_device_settings_props_object() {
        let device_settings_object = Value::Object(pipewire::spa::pod::Object {
            type_: 262146,
            id: 2,
            properties: vec![
                pipewire::spa::pod::Property::new(257, Value::String("front:0".to_string())),
                pipewire::spa::pod::Property::new(258, Value::String(String::new())),
                pipewire::spa::pod::Property::new(261, Value::String(String::new())),
                pipewire::spa::pod::Property::new(65550, Value::Long(0)),
                pipewire::spa::pod::Property::new(524289, Value::Struct(vec![])),
            ],
        });
        assert_eq!(
            extract_sink_props(&device_settings_object),
            None,
            "a Props object with no channelVolumes key is not a mixer update and must not report a fabricated zero volume"
        );
    }

    // ---- master_volume_from_props ----

    #[test]
    fn master_volume_from_props_cube_roots_the_loudest_channel() {
        // The exact live value this module's doc comment records: wpctl showed 30%,
        // channelVolumes carried 0.3^3 (to float rounding).
        let props = RawSinkProps { mute: false, channel_volumes: vec![0.027004944, 0.027004944] };
        let master = master_volume_from_props(&props);
        assert!((master.volume - 0.3).abs() < 0.001, "expected ~0.3, got {}", master.volume);
        assert!(!master.muted);
    }

    #[test]
    fn master_volume_from_props_takes_the_max_channel_not_the_first() {
        // An unbalanced pair (e.g. a manual per-channel adjustment) -- "the volume" should track
        // the louder side, not silently under-report because the first channel happens to be
        // quieter.
        let props = RawSinkProps { mute: false, channel_volumes: vec![0.0, 1.0] };
        assert_eq!(master_volume_from_props(&props).volume, 1.0);
    }

    #[test]
    fn master_volume_from_props_reports_mute_independently_of_channel_volumes() {
        // Verified live: toggling wpctl's mute flips SPA_PROP_mute without touching
        // channelVolumes at all.
        let props = RawSinkProps { mute: true, channel_volumes: vec![0.027004944, 0.027004944] };
        assert!(master_volume_from_props(&props).muted);
    }

    #[test]
    fn master_volume_from_props_handles_an_empty_channel_array_without_panicking() {
        let props = RawSinkProps { mute: false, channel_volumes: vec![] };
        assert_eq!(master_volume_from_props(&props).volume, 0.0);
    }

    // ---- parse_default_device_name ----

    #[test]
    fn parse_default_device_name_reads_the_real_pw_metadata_shape() {
        let json = r#"{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}"#;
        assert_eq!(parse_default_device_name(json), Some("alsa_output.pci-0000_00_1f.3.analog-stereo".to_string()));
    }

    #[test]
    fn parse_default_device_name_rejects_malformed_json() {
        assert_eq!(parse_default_device_name("not json"), None);
    }

    #[test]
    fn parse_default_device_name_rejects_a_missing_name_key() {
        assert_eq!(parse_default_device_name("{}"), None);
    }

    // ---- resolve_default_device ----

    #[test]
    fn resolve_default_device_matches_by_name() {
        let sinks = HashMap::from([(59, "alsa_output.pci-...analog-stereo".to_string()), (70, "bluez_output.headset".to_string())]);
        assert_eq!(resolve_default_device(Some("bluez_output.headset"), names(&sinks)), Some(70));
    }

    #[test]
    fn resolve_default_device_falls_back_to_lowest_id_with_no_default_name() {
        let sinks = HashMap::from([(70, "bluez_output.headset".to_string()), (59, "alsa_output.pci-...analog-stereo".to_string())]);
        assert_eq!(resolve_default_device(None, names(&sinks)), Some(59));
    }

    #[test]
    fn resolve_default_device_falls_back_when_the_named_sink_is_not_tracked_yet() {
        let sinks = HashMap::from([(59, "alsa_output.pci-...analog-stereo".to_string())]);
        assert_eq!(resolve_default_device(Some("bluez_output.not-seen-yet"), names(&sinks)), Some(59));
    }

    #[test]
    fn resolve_default_device_is_none_with_no_sinks_tracked_at_all() {
        assert_eq!(resolve_default_device(None, names(&HashMap::new())), None);
    }

    // ---- cubed_channel_volumes ----

    #[test]
    fn cubed_channel_volumes_is_the_exact_inverse_of_the_read_direction() {
        let volumes = cubed_channel_volumes(0.3, 2).expect("two channels is not zero");
        let read_back = master_volume_from_props(&RawSinkProps { mute: false, channel_volumes: volumes });
        assert!((read_back.volume - 0.3).abs() < 1e-6, "expected ~0.3, got {}", read_back.volume);
    }

    #[test]
    fn cubed_channel_volumes_writes_one_entry_per_channel() {
        assert_eq!(cubed_channel_volumes(1.0, 6).map(|v| v.len()), Some(6));
    }

    #[test]
    fn cubed_channel_volumes_clamps_a_value_outside_the_specified_range() {
        assert_eq!(cubed_channel_volumes(2.0, 1), Some(vec![1.0]));
        assert_eq!(cubed_channel_volumes(-0.5, 1), Some(vec![0.0]));
    }

    #[test]
    fn cubed_channel_volumes_refuses_a_device_reporting_no_channels() {
        // An empty `channelVolumes` array is a Props object PipeWire accepts and ignores, so a
        // write built from one would report success and change nothing. The read direction has an
        // honest answer for the same input (0.0); this one does not, so it declines.
        assert_eq!(cubed_channel_volumes(0.5, 0), None);
    }

    // ---- route_object / extract_route_target ----

    #[test]
    fn a_route_object_round_trips_back_to_the_target_it_names() {
        let object = route_object(2, 7, Some(vec![0.027, 0.027]), Some(false));
        assert_eq!(extract_route_target(&object), Some((7, 2)));
    }

    #[test]
    fn a_props_object_carries_only_the_field_the_caller_is_changing() {
        // Sending both would lose a write: neither value is updated optimistically, so two
        // commands in the same tick each read the state from before the other. See
        // `props_object`'s own doc comment for the live observation that found it.
        let Value::Object(volume_only) = props_object(Some(vec![0.5]), None) else { panic!("props_object must build an object") };
        assert_eq!(volume_only.properties.len(), 1);
        assert_eq!(volume_only.properties[0].key, spa_sys::SPA_PROP_channelVolumes);

        let Value::Object(mute_only) = props_object(None, Some(true)) else { panic!("props_object must build an object") };
        assert_eq!(mute_only.properties.len(), 1);
        assert_eq!(mute_only.properties[0].key, spa_sys::SPA_PROP_mute);
    }

    #[test]
    fn a_route_object_carries_the_volume_in_a_nested_props_object() {
        // The shape a live `pw-cli enum-params <device> Route` shows: the volume is not on the
        // Route, it is on a `Props` object hanging off its `props` key.
        let Value::Object(route) = route_object(2, 7, Some(vec![0.125, 0.125]), Some(true)) else { panic!("route_object must build an object") };
        let nested = route.properties.iter().find(|property| property.key == spa_sys::SPA_PARAM_ROUTE_props).expect("a Route must carry props");
        let extracted = extract_sink_props(&nested.value).expect("the nested object must parse as sink props");
        assert!(extracted.mute);
        assert_eq!(extracted.channel_volumes, vec![0.125, 0.125]);
    }

    #[test]
    fn a_route_object_asks_for_the_setting_to_be_saved() {
        let Value::Object(route) = route_object(2, 7, Some(vec![0.5]), None) else { panic!("route_object must build an object") };
        let save = route.properties.iter().find(|property| property.key == spa_sys::SPA_PARAM_ROUTE_save).expect("a Route must carry save");
        assert_eq!(save.value, Value::Bool(true));
    }

    #[test]
    fn extract_route_target_skips_an_object_missing_either_field() {
        let index_only = Value::Object(Object {
            type_: spa_sys::SPA_TYPE_OBJECT_ParamRoute,
            id: spa_sys::SPA_PARAM_Route,
            properties: vec![Property::new(spa_sys::SPA_PARAM_ROUTE_index, Value::Int(2))],
        });
        assert_eq!(extract_route_target(&index_only), None);
        assert_eq!(extract_route_target(&Value::Bool(true)), None);
    }

    // ---- serialize_props ----

    #[test]
    fn a_serialized_props_object_reads_back_as_the_same_values() {
        // The write path hands these bytes to C through `Pod::from_bytes`. If the serializer and
        // the deserializer disagree, PipeWire gets a pod that parses as something else and the
        // symptom is a write that is accepted and ignored, which is exactly the failure this
        // capability already hit once.
        let bytes = serialize_props(&props_object(Some(vec![0.027, 0.027]), Some(true))).expect("a Props object must serialize");
        let (_, value) = pipewire::spa::pod::deserialize::PodDeserializer::deserialize_from::<Value>(&bytes).expect("the bytes must read back as a pod");
        let extracted = extract_sink_props(&value).expect("the round trip must still parse as sink props");
        assert!(extracted.mute);
        assert_eq!(extracted.channel_volumes, vec![0.027, 0.027]);
    }

    // ---- compute_master ----

    #[test]
    fn compute_master_combines_resolution_and_lookup() {
        let sinks = HashMap::from([(59, "alsa_output.pci-...analog-stereo".to_string())]);
        // 0.027 cubed-back is 0.3 linear, which is the conversion `compute_master` now runs
        // itself rather than receiving already applied.
        let props = HashMap::from([(59, RawSinkProps { mute: false, channel_volumes: vec![0.027, 0.027] })]);
        let master = compute_master(Some("alsa_output.pci-...analog-stereo"), names(&sinks), |id| props.get(&id).cloned());
        assert!((master.volume - 0.3).abs() < 1e-6, "expected ~0.3, got {}", master.volume);
        assert!(!master.muted);
    }

    #[test]
    fn compute_master_defaults_when_the_resolved_sink_has_no_props_yet() {
        // resolve_default_device can find a node id before that node's own Props param has arrived
        // (they're independent events) -- compute_master must not panic on that gap.
        let sinks = HashMap::from([(59, "alsa_output.pci-...analog-stereo".to_string())]);
        assert_eq!(compute_master(Some("alsa_output.pci-...analog-stereo"), names(&sinks), |_| None), MasterVolume::default());
    }

    #[test]
    fn compute_master_defaults_with_nothing_tracked() {
        assert_eq!(compute_master(None, names(&HashMap::new()), |_| None), MasterVolume::default());
    }
}
