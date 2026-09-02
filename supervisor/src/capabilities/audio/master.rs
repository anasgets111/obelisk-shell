//! Master output volume/mute (§ 2.4), computed from the default sink's `SPA_PARAM_Props` pod,
//! plus the metadata lookup that decides which sink is "the default" one.
//!
//! Master volume is not a node property: it's absent from `info().props()`, arriving only via a
//! `param` event after `Node::subscribe_params(&[ParamType::Props])`. Verified live on sink 59:
//! `SPA_PROP_volume` is 65539, `SPA_PROP_mute` 65540, `SPA_PROP_channelVolumes` 65544, ids from
//! `libspa-sys`'s bindgen output (`props.h`'s enum ordering gives wrong numbers here). `libspa`
//! has no typed wrapper for them, so parsing matches raw `spa_sys::SPA_PROP_*` constants.
//!
//! **linear vs cubic**: the scalar `SPA_PROP_volume` stayed `1.0` with the sink at 30%, but
//! `SPA_PROP_channelVolumes` was `[0.027004944, 0.027004944]`, what `wpctl`/`pactl`/WirePlumber
//! actually show (`0.30`, and `0.3^3 = 0.027` to five decimals). So `channelVolumes` is the cube
//! of "the volume"; `wpctl set-mute` confirmed `SPA_PROP_mute` flips independently (§ 2.4's
//! independent fields), and [`master_volume_from_props`] takes the max channel (stereo isn't
//! guaranteed equal) and cube-roots it, instead of scalar `volume` or raw `channelVolumes`.

use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{Object, Property, Value, ValueArray};
use pipewire::spa::sys as spa_sys;
use serde::Serialize;

/// Master output volume/mute, as § 2.4 specifies (`audio.volume`, `audio.muted`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, schemars::JsonSchema)]
pub struct MasterVolume {
    pub volume: f32,
    pub muted: bool,
}

impl Default for MasterVolume {
    /// Only ever observed before the default sink's first `Props` param event has arrived.
    /// ponytail: it arrived on the next main-loop iteration in this codebase's live probe, so
    /// this is a startup value. Upgrade path if `Props` never fires: an explicit "not yet known".
    fn default() -> Self {
        Self { volume: 0.0, muted: false }
    }
}

/// The `SPA_PROP_mute`/`SPA_PROP_channelVolumes` values `mixer.rs` pulls out of one sink node's
/// `Props` param pod: everything [`master_volume_from_props`] needs, already stripped of the
/// pod/`Value` machinery so it stays testable without constructing a real pod.
#[derive(Debug, Clone, PartialEq)]
pub struct RawSinkProps {
    pub mute: bool,
    pub channel_volumes: Vec<f32>,
}

/// Pulls [`RawSinkProps`] out of a sink's `Props` param, already deserialized into `libspa`'s
/// generic [`Value`] by `mixer.rs` (always `Value::Object` here, checked live). A sink
/// advertises two separate `Props` objects, not one, back to back on every subscription and
/// mixer change (confirmed live): `index=0` carries the mixer keys this reads, `index=1`
/// unrelated ALSA settings (`device`, `deviceName`, `cardName`, a nested `params` struct) with
/// none of them. Treating a missing `channelVolumes` key as empty was a real bug: `index=1`
/// overwrote the tracked volume with a zeroed `MasterVolume` right after `index=0` set it. So
/// `channelVolumes`'s presence is now the signal for a real update; its absence returns `None`.
pub fn extract_sink_props(value: &Value) -> Option<RawSinkProps> {
    let Value::Object(object) = value else { return None };

    let mut mute = false;
    let mut channel_volumes = None;
    for property in &object.properties {
        match (property.key, &property.value) {
            (key, Value::Bool(value)) if key == spa_sys::SPA_PROP_mute => mute = *value,
            (key, Value::ValueArray(pipewire::spa::pod::ValueArray::Float(values)))
                if key == spa_sys::SPA_PROP_channelVolumes =>
            {
                channel_volumes = Some(values.clone());
            }
            _ => {}
        }
    }
    Some(RawSinkProps { mute, channel_volumes: channel_volumes? })
}

/// Converts one sink's raw `Props` values into the `MasterVolume` § 2.4 wants: the cube root of
/// the loudest channel, max not first because a stereo pair isn't guaranteed equal (see the
/// module doc for why cube root). An empty `channel_volumes` reports `0.0` instead of panicking.
pub fn master_volume_from_props(props: &RawSinkProps) -> MasterVolume {
    let peak_linear = props.channel_volumes.iter().copied().fold(0.0_f32, f32::max);
    MasterVolume { volume: peak_linear.cbrt(), muted: props.mute }
}

/// Parses a PipeWire metadata `default.audio.sink`/`default.audio.source` property value, raw
/// JSON of the shape `{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}`, into the device's
/// `node.name` (one function for both keys: a live `pw-metadata` dump confirmed they share this
/// shape). `None` for anything else; callers treat that as "no default known yet".
pub fn parse_default_device_name(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    value.get("name")?.as_str().map(str::to_string)
}

/// Which tracked device node id is the default right now, given the name PipeWire's metadata
/// reports (if any) and the `node_id -> node.name` map of `Audio/Sink` (or `Audio/Source`) nodes
/// `mixer.rs` has seen `global` events for. Matches by name, since the metadata key names a
/// device by `node.name`, not registry id. Public and device-kind-agnostic since § 2.4's
/// `sinks`/`sources` arrays landed: both need the same "which of these is active" answer.
///
/// ponytail: `default_name` can name a sink not yet tracked, at startup (metadata and the
/// sink's own `global` event have no ordering guarantee) or on removal (no fresh
/// `default.audio.sink` update with the node removal). Both fall back to the lowest tracked
/// sink id, correct only with one sink; upgrade path: track the previous resolved id.
pub fn resolve_default_device<'a>(
    default_name: Option<&str>,
    names: impl Iterator<Item = (u32, &'a str)>,
) -> Option<u32> {
    let mut matched: Option<u32> = None;
    let mut lowest: Option<u32> = None;
    for (id, name) in names {
        if default_name == Some(name) {
            // Lowest id wins a tie, not map iteration order: HashMap order isn't stable, and
            // without this the active device would flip between publishes of an unchanged map.
            matched = Some(matched.map_or(id, |current| current.min(id)));
        }
        lowest = Some(lowest.map_or(id, |low| low.min(id)));
    }
    matched.or(lowest)
}

/// Combines [`resolve_default_device`] and the tracked per-sink [`RawSinkProps`] into the value
/// `mixer.rs` publishes: [`MasterVolume::default`] if no sink is resolved, or its `Props` haven't
/// arrived (the two maps update from separate PipeWire events). `mixer.rs` keeps the whole
/// [`RawSinkProps`], not the derived [`MasterVolume`], because the write path needs channel count.
pub fn compute_master<'a>(
    default_name: Option<&str>,
    names: impl Iterator<Item = (u32, &'a str)>,
    props: impl Fn(u32) -> Option<RawSinkProps>,
) -> MasterVolume {
    resolve_default_device(default_name, names)
        .and_then(props)
        .map(|raw| master_volume_from_props(&raw))
        .unwrap_or_default()
}

/// The inverse of [`master_volume_from_props`]'s cube root, spread across `channels` channels.
/// PipeWire stores `channelVolumes` cubed, so a linear `0.3` is written as `0.027`; skipping this
/// writes 30% as 67%, the mistake the read direction already made once (ADR-0053). `linear` is
/// clamped to `[0.0, 1.0]` here (§ 3.2 states the range; this output leaves the process), and
/// every channel gets the same value, since § 2.4 has one volume per device, flattening any
/// uneven balance a user had set.
pub fn cubed_channel_volumes(linear: f32, channels: usize) -> Option<Vec<f32>> {
    if channels == 0 {
        // An empty channelVolumes array is a Props object PipeWire accepts and ignores, so a
        // set_volume would report success and change nothing. Refusing lets the caller say so.
        return None;
    }
    let clamped = linear.clamp(0.0, 1.0);
    Some(vec![clamped * clamped * clamped; channels])
}

/// Builds the `SPA_PARAM_Props` object `Node::set_param` takes, carrying only the changed field:
/// a partial object applies as a partial update (confirmed live: `pw-cli set-param <stream>
/// Props '{ mute: true }'` muted the stream, left volume alone). Sending both loses a write,
/// since neither updates optimistically: observed live, `set_app_volume(id, 0.42)` then
/// `set_app_muted(id, true)` in the same tick sent the second object with the pre-first volume.
pub fn props_object(channel_volumes: Option<Vec<f32>>, muted: Option<bool>) -> Value {
    props_object_with_id(spa_sys::SPA_PARAM_Props, channel_volumes, muted)
}

/// The same object under a caller-chosen param id. A `Props` nested inside a `Route` carries
/// `SPA_PARAM_Route` as its id, not `SPA_PARAM_Props` (confirmed live via `pw-cli enum-params
/// <device> Route`); building it with the standalone id is a pod PipeWire accepts and ignores.
fn props_object_with_id(id: u32, channel_volumes: Option<Vec<f32>>, muted: Option<bool>) -> Value {
    let mut properties = Vec::new();
    if let Some(muted) = muted {
        properties.push(Property::new(spa_sys::SPA_PROP_mute, Value::Bool(muted)));
    }
    if let Some(channel_volumes) = channel_volumes {
        properties.push(Property::new(
            spa_sys::SPA_PROP_channelVolumes,
            Value::ValueArray(ValueArray::Float(channel_volumes)),
        ));
    }
    Value::Object(Object { type_: spa_sys::SPA_TYPE_OBJECT_Props, id, properties })
}

/// Serializes [`props_object`]'s output into raw pod bytes `Pod::from_bytes` reads back.
/// Separate from `props_object` so the object shape stays unit-testable without a serializer,
/// and so `mixer.rs` holds the bytes alive as long as the `&Pod` borrowed from them is in use.
pub fn serialize_props(object: &Value) -> Option<Vec<u8>> {
    let (cursor, _) = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), object).ok()?;
    Some(cursor.into_inner())
}

/// Builds the `SPA_PARAM_Route` object a hardware sink's volume is actually written through
/// (see `mixer::write_master`). `index` names the card's route to write; `profile_device` is the
/// `card.profile.device` the sink reports, matched by a `Route`'s `device` field. `save: true`
/// matches every other mixer's write and survives a re-plug; volume/mute ride in a nested
/// `Props`, the shape a live `pw-cli enum-params <device> Route` shows.
pub fn route_object(index: i32, profile_device: i32, channel_volumes: Option<Vec<f32>>, muted: Option<bool>) -> Value {
    Value::Object(Object {
        type_: spa_sys::SPA_TYPE_OBJECT_ParamRoute,
        id: spa_sys::SPA_PARAM_Route,
        properties: vec![
            Property::new(spa_sys::SPA_PARAM_ROUTE_index, Value::Int(index)),
            Property::new(spa_sys::SPA_PARAM_ROUTE_device, Value::Int(profile_device)),
            Property::new(
                spa_sys::SPA_PARAM_ROUTE_props,
                props_object_with_id(spa_sys::SPA_PARAM_Route, channel_volumes, muted),
            ),
            Property::new(spa_sys::SPA_PARAM_ROUTE_save, Value::Bool(true)),
        ],
    })
}

/// Pulls `(card.profile.device, route index)` out of one `Route` object a device published: the
/// whole reason devices are tracked, since a card advertises several routes and only this param
/// says which index is active for a port, and a write to the wrong index goes nowhere. `None`
/// for an object missing either field, skipping `EnumRoute`-shaped entries and anything under a
/// different param rather than misreading them.
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

    /// Adapts a plain `id -> node.name` fixture map into the iterator both functions take.
    fn names(map: &HashMap<u32, String>) -> impl Iterator<Item = (u32, &str)> {
        map.iter().map(|(&id, name)| (id, name.as_str()))
    }

    // ---- extract_sink_props ----

    /// The shape observed live for the default sink's `Props` param, trimmed to just the three
    /// keys this module reads.
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
        // SPA_PROP_volume (Float(1.0) above) is PipeWire's default, not what pactl/wpctl show.
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

    /// Regression test: `wpctl get-volume` said 0.45, but the next `oblisk.audio` push reported
    /// `0.0` with no real change in between. Root cause, found with a live probe: a sink's Props
    /// param isn't one object, it's two, delivered back to back -- `index=0` (modeled by
    /// [`sample_props_object`]) carries `volume`/`mute`/`channelVolumes`; `index=1` carries
    /// unrelated ALSA device settings with none of those keys. This is `index=1`'s real shape,
    /// keys taken verbatim from that probe (257/258/261 are `device`/`deviceName`/`cardName`,
    /// 65550 is `SPA_PROP_latencyOffsetNsec`, 524289 a nested `params` struct). Before the fix
    /// this returned a "parsed" zero that clobbered the value `index=0` had just set; it must
    /// return `None` so the caller leaves the previously tracked volume alone.
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
        // The exact live value: wpctl showed 30%, channelVolumes carried 0.3^3 (float rounding).
        let props = RawSinkProps { mute: false, channel_volumes: vec![0.027004944, 0.027004944] };
        let master = master_volume_from_props(&props);
        assert!((master.volume - 0.3).abs() < 0.001, "expected ~0.3, got {}", master.volume);
        assert!(!master.muted);
    }

    #[test]
    fn master_volume_from_props_takes_the_max_channel_not_the_first() {
        let props = RawSinkProps { mute: false, channel_volumes: vec![0.0, 1.0] };
        assert_eq!(master_volume_from_props(&props).volume, 1.0);
    }

    #[test]
    fn master_volume_from_props_reports_mute_independently_of_channel_volumes() {
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
        let sinks = HashMap::from([
            (59, "alsa_output.pci-...analog-stereo".to_string()),
            (70, "bluez_output.headset".to_string()),
        ]);
        assert_eq!(resolve_default_device(Some("bluez_output.headset"), names(&sinks)), Some(70));
    }

    #[test]
    fn resolve_default_device_falls_back_to_lowest_id_with_no_default_name() {
        let sinks = HashMap::from([
            (70, "bluez_output.headset".to_string()),
            (59, "alsa_output.pci-...analog-stereo".to_string()),
        ]);
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
        // Sending both would lose a write -- see props_object's own doc comment.
        let Value::Object(volume_only) = props_object(Some(vec![0.5]), None) else {
            panic!("props_object must build an object")
        };
        assert_eq!(volume_only.properties.len(), 1);
        assert_eq!(volume_only.properties[0].key, spa_sys::SPA_PROP_channelVolumes);

        let Value::Object(mute_only) = props_object(None, Some(true)) else {
            panic!("props_object must build an object")
        };
        assert_eq!(mute_only.properties.len(), 1);
        assert_eq!(mute_only.properties[0].key, spa_sys::SPA_PROP_mute);
    }

    #[test]
    fn a_route_object_carries_the_volume_in_a_nested_props_object() {
        let Value::Object(route) = route_object(2, 7, Some(vec![0.125, 0.125]), Some(true)) else {
            panic!("route_object must build an object")
        };
        let nested = route
            .properties
            .iter()
            .find(|property| property.key == spa_sys::SPA_PARAM_ROUTE_props)
            .expect("a Route must carry props");
        let extracted = extract_sink_props(&nested.value).expect("the nested object must parse as sink props");
        assert!(extracted.mute);
        assert_eq!(extracted.channel_volumes, vec![0.125, 0.125]);
    }

    #[test]
    fn a_route_object_asks_for_the_setting_to_be_saved() {
        let Value::Object(route) = route_object(2, 7, Some(vec![0.5]), None) else {
            panic!("route_object must build an object")
        };
        let save = route
            .properties
            .iter()
            .find(|property| property.key == spa_sys::SPA_PARAM_ROUTE_save)
            .expect("a Route must carry save");
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
        // The write path hands these bytes to C through Pod::from_bytes. A serializer/
        // deserializer disagreement means a write accepted and silently ignored.
        let bytes = serialize_props(&props_object(Some(vec![0.027, 0.027]), Some(true)))
            .expect("a Props object must serialize");
        let (_, value) = pipewire::spa::pod::deserialize::PodDeserializer::deserialize_from::<Value>(&bytes)
            .expect("the bytes must read back as a pod");
        let extracted = extract_sink_props(&value).expect("the round trip must still parse as sink props");
        assert!(extracted.mute);
        assert_eq!(extracted.channel_volumes, vec![0.027, 0.027]);
    }

    // ---- compute_master ----

    #[test]
    fn compute_master_combines_resolution_and_lookup() {
        let sinks = HashMap::from([(59, "alsa_output.pci-...analog-stereo".to_string())]);
        let props = HashMap::from([(59, RawSinkProps { mute: false, channel_volumes: vec![0.027, 0.027] })]);
        let master =
            compute_master(Some("alsa_output.pci-...analog-stereo"), names(&sinks), |id| props.get(&id).cloned());
        assert!((master.volume - 0.3).abs() < 1e-6, "expected ~0.3, got {}", master.volume);
        assert!(!master.muted);
    }

    #[test]
    fn compute_master_defaults_when_the_resolved_sink_has_no_props_yet() {
        let sinks = HashMap::from([(59, "alsa_output.pci-...analog-stereo".to_string())]);
        assert_eq!(
            compute_master(Some("alsa_output.pci-...analog-stereo"), names(&sinks), |_| None),
            MasterVolume::default()
        );
    }

    #[test]
    fn compute_master_defaults_with_nothing_tracked() {
        assert_eq!(compute_master(None, names(&HashMap::new()), |_| None), MasterVolume::default());
    }
}
