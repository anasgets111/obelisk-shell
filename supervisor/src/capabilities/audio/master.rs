//! Master output volume/mute (§ 2.4) from the default sink's `SPA_PARAM_Props` pod and the
//! metadata lookup that selects that sink.
//!
//! It is absent from `info().props()`: after `Node::subscribe_params(&[ParamType::Props])`, it
//! arrives only in `param`. Live sink 59 reported `SPA_PROP_volume` 65539, `SPA_PROP_mute` 65540,
//! and `SPA_PROP_channelVolumes` 65544. `libspa-sys` bindgen gets these from `props.h`'s wrong
//! enum ordering, and `libspa` has no typed wrapper, so parsing uses raw `spa_sys` constants.
//!
//! **Linear vs cubic:** at 30%, scalar `SPA_PROP_volume` stayed `1.0`, while
//! `SPA_PROP_channelVolumes` was `[0.027004944, 0.027004944]` (`0.3^3 = 0.027`; `wpctl`/`pactl`
//! show `0.30`). Live `wpctl set-mute` confirmed `SPA_PROP_mute` flips independently (§ 2.4). We
//! max and cube-root the channels, rather than using scalar `volume` or raw `channelVolumes`,
//! because stereo channels may differ.

use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{Object, Property, Value, ValueArray};
use pipewire::spa::sys as spa_sys;
use pipewire::spa::utils::Id;
use serde::Serialize;

/// Master output volume/mute, as § 2.4 specifies (`audio.volume`, `audio.muted`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, schemars::JsonSchema)]
pub struct MasterVolume {
    pub volume: f32,
    pub muted: bool,
}

impl Default for MasterVolume {
    /// Startup value before the default sink's first `Props` event. ponytail: a live probe saw
    /// that event on the next main-loop iteration; if it stops arriving, add an explicit
    /// "not yet known" state.
    fn default() -> Self {
        Self { volume: 0.0, muted: false }
    }
}

/// The `SPA_PROP_mute`/`SPA_PROP_channelVolumes` values `mixer.rs` pulls from a sink `Props` pod,
/// stripped of pod machinery so [`master_volume_from_props`] stays testable.
#[derive(Debug, Clone, PartialEq)]
pub struct RawSinkProps {
    pub mute: bool,
    pub channel_volumes: Vec<f32>,
}

/// Pulls [`RawSinkProps`] from `libspa`'s generic [`Value`] (always an object here, checked live).
/// Sinks emit two `Props` objects back to back on every subscription and mixer change (confirmed
/// live): `index=0` has mixer keys; `index=1` has unrelated ALSA `device`/`deviceName`/`cardName`/
/// `params`. A real bug treated the latter's missing
/// `channelVolumes` as empty and overwrote the value just set by `index=0`; absence now returns
/// `None`.
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

/// Converts raw `Props` to § 2.4's value: the cube root of the loudest channel. Empty channels
/// report `0.0` instead of panicking.
pub fn master_volume_from_props(props: &RawSinkProps) -> MasterVolume {
    let peak_linear = props.channel_volumes.iter().copied().fold(0.0_f32, f32::max);
    MasterVolume { volume: peak_linear.cbrt(), muted: props.mute }
}

/// Parses either `default.audio.sink` or `default.audio.source`, whose live `pw-metadata` shape is
/// `{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}`, into `node.name`. `None` means no
/// default is known yet.
pub fn parse_default_device_name(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    value.get("name")?.as_str().map(str::to_string)
}

/// Resolves the metadata name against tracked `Audio/Sink` or `Audio/Source` `node.name` values,
/// not registry ids, for § 2.4's `sinks`/`sources` active flag.
///
/// ponytail: startup metadata and `global` events have no ordering guarantee, and removal may
/// have no fresh `default.audio.sink` update. An untracked name falls back to the lowest id,
/// correct only with one sink; upgrade path: track the previous resolved id.
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

/// Combines default-name resolution with tracked [`RawSinkProps`]. Returns
/// [`MasterVolume::default`] when resolution or `Props` is still missing; the maps update from
/// separate PipeWire events. The raw props stay tracked because writes need channel count.
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

/// Inverse of [`master_volume_from_props`]'s cube root, spread across `channels`. PipeWire stores
/// `channelVolumes` cubed, so skipping this wrote 30% as 67%, the read mistake (ADR-0053).
/// Clamps `linear` to `[0.0, 1.0]` (§ 3.2). § 2.4 has one volume per device, so every channel gets
/// the same value, flattening balance.
pub fn cubed_channel_volumes(linear: f32, channels: usize) -> Option<Vec<f32>> {
    if channels == 0 {
        // PipeWire accepts and ignores an empty channelVolumes array, so set_volume would report
        // success without changing anything. Refuse it so the caller can report failure.
        return None;
    }
    let clamped = linear.clamp(0.0, 1.0);
    Some(vec![clamped * clamped * clamped; channels])
}

/// Builds the partial `SPA_PARAM_Props` object accepted by `Node::set_param`. Live
/// `pw-cli set-param <stream> Props '{ mute: true }'` left volume alone; sending both fields loses
/// a same-tick write because state is not optimistic (`set_app_volume(0.42)` then mute sent the
/// pre-write volume).
pub fn props_object(channel_volumes: Option<Vec<f32>>, muted: Option<bool>) -> Value {
    props_object_with_id(spa_sys::SPA_PARAM_Props, channel_volumes, muted)
}

/// The same object under a caller-chosen id. `Props` nested in a `Route` must use
/// `SPA_PARAM_Route`, not `SPA_PARAM_Props`; PipeWire accepts the standalone id and ignores it
/// (confirmed with `pw-cli enum-params <device> Route`).
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

/// Serializes [`props_object`]'s output into bytes for `Pod::from_bytes`; callers keep those
/// bytes alive while the borrowed `&Pod` is in use.
pub fn serialize_props(object: &Value) -> Option<Vec<u8>> {
    let (cursor, _) = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), object).ok()?;
    Some(cursor.into_inner())
}

/// Builds the `SPA_PARAM_Route` object for a hardware sink. `index` selects the card route and
/// `profile_device` matches the sink's `card.profile.device` to `Route.device`. `save: true`
/// matches other mixer writes and survives re-plug; volume/mute ride in nested `Props` (the live
/// `pw-cli enum-params <device> Route` shape).
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

/// Pulls `(card.profile.device, route index)` from a published `Route`. Cards advertise several
/// routes, and the wrong index writes nowhere. `None` skips objects missing either field, including
/// `EnumRoute`-shaped or otherwise unrelated params.
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

/// One BlueZ card profile from `EnumProfile` or `Profile`, e.g. index 2, `a2dp-sink-aac`, described
/// as `"High Fidelity Playback (A2DP Sink, codec AAC)"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub index: i32,
    /// PipeWire's untranslated id, empty when the object carries none.
    pub name: String,
    pub description: String,
    /// `false` only when PipeWire reports the profile unavailable; unknown counts as available.
    pub available: bool,
}

/// Pulls a [`Profile`] from an `EnumProfile` or `Profile` object. `None` skips an object missing
/// its index or description.
pub fn extract_profile(value: &Value) -> Option<Profile> {
    let Value::Object(object) = value else { return None };
    let mut index = None;
    let mut name = String::new();
    let mut description = None;
    let mut available = true;
    for property in &object.properties {
        match (property.key, &property.value) {
            (spa_sys::SPA_PARAM_PROFILE_index, Value::Int(value)) => index = Some(*value),
            (spa_sys::SPA_PARAM_PROFILE_name, Value::String(value)) => name = value.clone(),
            (spa_sys::SPA_PARAM_PROFILE_description, Value::String(value)) => description = Some(value.clone()),
            (spa_sys::SPA_PARAM_PROFILE_available, Value::Id(Id(value))) => {
                available = *value != spa_sys::SPA_PARAM_AVAILABILITY_no;
            }
            _ => {}
        }
    }
    Some(Profile { index: index?, name, description: description?, available })
}

/// The codec a BlueZ profile name carries after its role, e.g. `"AAC"` from `a2dp-sink-aac`, or
/// `None` for a name with no codec, such as `off` or a bare `a2dp-sink`. Read first because the
/// name is never translated, where the description is.
pub fn codec_from_name(name: &str) -> Option<String> {
    let codec = ["a2dp-sink-", "a2dp-duplex-", "headset-head-unit-", "bap-sink-", "bap-duplex-"]
        .iter()
        .find_map(|role| name.strip_prefix(role))?;
    let spelled = match codec {
        "aptx" => "aptX".to_string(),
        "aptx_hd" => "aptX HD".to_string(),
        "aptx_ll" => "aptX LL".to_string(),
        "msbc" => "mSBC".to_string(),
        "faststream" => "FastStream".to_string(),
        other => other.replace('_', "-").to_uppercase(),
    };
    (!spelled.is_empty()).then_some(spelled)
}

/// The codec a BlueZ profile description names, e.g. `"AAC"` from
/// `"High Fidelity Playback (A2DP Sink, codec AAC)"`, or `None` for a profile without one, such as
/// `Off`. The fallback for a name [`codec_from_name`] cannot read; it matches only an English
/// description, as `BluetoothService.qml`'s `pactl` parse did.
pub fn codec_of(description: &str) -> Option<String> {
    let rest = &description[description.find("codec ")? + "codec ".len()..];
    let codec = rest.split([')', ',']).next()?.trim();
    (!codec.is_empty()).then(|| codec.to_string())
}

/// Builds the `SPA_PARAM_Profile` object that switches a BlueZ device to profile `index`.
/// `save: true` keeps the choice for the device's next connection, as the other mixer writes do.
pub fn profile_object(index: i32) -> Value {
    Value::Object(Object {
        type_: spa_sys::SPA_TYPE_OBJECT_ParamProfile,
        id: spa_sys::SPA_PARAM_Profile,
        properties: vec![
            Property::new(spa_sys::SPA_PARAM_PROFILE_index, Value::Int(index)),
            Property::new(spa_sys::SPA_PARAM_PROFILE_save, Value::Bool(true)),
        ],
    })
}

/// The MAC in WirePlumber's BlueZ card name, `bluez_card.AA_BB_CC_DD_EE_FF` to
/// `AA:BB:CC:DD:EE:FF`, for a device whose global props do not carry `api.bluez5.address`.
pub fn mac_from_card_name(name: &str) -> Option<String> {
    let address = name.strip_prefix("bluez_card.")?;
    (address.len() == 17).then(|| address.replace('_', ":"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn names(map: &HashMap<u32, String>) -> impl Iterator<Item = (u32, &str)> {
        map.iter().map(|(&id, name)| (id, name.as_str()))
    }

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

    /// Regression test: `wpctl get-volume` said 0.45, but the next `obelisk.audio` push reported
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

    #[test]
    fn a_route_object_round_trips_back_to_the_target_it_names() {
        let object = route_object(2, 7, Some(vec![0.027, 0.027]), Some(false));
        assert_eq!(extract_route_target(&object), Some((7, 2)));
    }

    #[test]
    fn a_props_object_carries_only_the_field_the_caller_is_changing() {
        // Sending both loses a write; see props_object's doc comment.
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

    #[test]
    fn a_profile_object_names_the_index_and_asks_to_be_saved() {
        // Round-tripped through bytes, because those bytes are what `set_param` hands to C.
        let bytes = serialize_props(&profile_object(3)).expect("a Profile object must serialize");
        let (_, value) = pipewire::spa::pod::deserialize::PodDeserializer::deserialize_from::<Value>(&bytes)
            .expect("the bytes must read back as a pod");
        let Value::Object(object) = value else { panic!("a Profile must be an object") };
        assert_eq!(object.id, spa_sys::SPA_PARAM_Profile);
        let has = |key, expected: Value| object.properties.iter().any(|p| p.key == key && p.value == expected);
        assert!(has(spa_sys::SPA_PARAM_PROFILE_index, Value::Int(3)));
        assert!(has(spa_sys::SPA_PARAM_PROFILE_save, Value::Bool(true)));
    }

    fn enum_profile(index: i32, description: &str, availability: u32) -> Value {
        Value::Object(Object {
            type_: spa_sys::SPA_TYPE_OBJECT_ParamProfile,
            id: spa_sys::SPA_PARAM_EnumProfile,
            properties: vec![
                Property::new(spa_sys::SPA_PARAM_PROFILE_index, Value::Int(index)),
                Property::new(spa_sys::SPA_PARAM_PROFILE_name, Value::String("a2dp-sink-aac".to_string())),
                Property::new(spa_sys::SPA_PARAM_PROFILE_description, Value::String(description.to_string())),
                Property::new(spa_sys::SPA_PARAM_PROFILE_available, Value::Id(Id(availability))),
            ],
        })
    }

    #[test]
    fn extract_profile_reads_index_description_and_availability() {
        let aac = "High Fidelity Playback (A2DP Sink, codec AAC)";
        assert_eq!(
            extract_profile(&enum_profile(2, aac, spa_sys::SPA_PARAM_AVAILABILITY_yes)),
            Some(Profile {
                index: 2,
                name: "a2dp-sink-aac".to_string(),
                description: aac.to_string(),
                available: true
            })
        );
        let gone = enum_profile(3, aac, spa_sys::SPA_PARAM_AVAILABILITY_no);
        assert_eq!(extract_profile(&gone).map(|profile| profile.available), Some(false));
        assert_eq!(extract_profile(&Value::Bool(true)), None);
    }

    #[test]
    fn codec_of_reads_the_codec_out_of_a_bluez_profile_description() {
        assert_eq!(codec_of("High Fidelity Playback (A2DP Sink, codec AAC)"), Some("AAC".to_string()));
        assert_eq!(codec_of("High Fidelity Playback (A2DP Sink, codec SBC-XQ)"), Some("SBC-XQ".to_string()));
        assert_eq!(codec_of("Headset Head Unit (HSP/HFP, codec mSBC)"), Some("mSBC".to_string()));
        assert_eq!(codec_of("Off"), None);
    }

    #[test]
    fn codec_from_name_reads_the_codec_after_the_profile_role() {
        assert_eq!(codec_from_name("a2dp-sink-aac"), Some("AAC".to_string()));
        assert_eq!(codec_from_name("a2dp-sink-sbc_xq"), Some("SBC-XQ".to_string()));
        assert_eq!(codec_from_name("a2dp-sink-aptx_hd"), Some("aptX HD".to_string()));
        assert_eq!(codec_from_name("headset-head-unit-msbc"), Some("mSBC".to_string()));
        assert_eq!(codec_from_name("a2dp-sink"), None, "a bare role names no codec");
        assert_eq!(codec_from_name("off"), None);
    }

    #[test]
    fn mac_from_card_name_reads_the_address_out_of_a_bluez_card_name() {
        assert_eq!(mac_from_card_name("bluez_card.AA_BB_CC_DD_EE_FF"), Some("AA:BB:CC:DD:EE:FF".to_string()));
        assert_eq!(mac_from_card_name("alsa_card.pci-0000_2b_00.1"), None);
    }

    #[test]
    fn a_serialized_props_object_reads_back_as_the_same_values() {
        // The write path hands these bytes to C; disagreement would make an accepted write no-op.
        let bytes = serialize_props(&props_object(Some(vec![0.027, 0.027]), Some(true)))
            .expect("a Props object must serialize");
        let (_, value) = pipewire::spa::pod::deserialize::PodDeserializer::deserialize_from::<Value>(&bytes)
            .expect("the bytes must read back as a pod");
        let extracted = extract_sink_props(&value).expect("the round trip must still parse as sink props");
        assert!(extracted.mute);
        assert_eq!(extracted.channel_volumes, vec![0.027, 0.027]);
    }

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
