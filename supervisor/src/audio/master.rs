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

use std::collections::HashMap;

use pipewire::spa::pod::Value;
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
pub fn resolve_default_device(default_name: Option<&str>, names: &HashMap<u32, String>) -> Option<u32> {
    if let Some(default_name) = default_name
        && let Some((&id, _)) = names.iter().find(|(_, name)| name.as_str() == default_name)
    {
        return Some(id);
    }
    names.keys().copied().min()
}

/// Combines [`resolve_default_device`] and the tracked per-sink [`MasterVolume`]s into the one
/// value `mixer.rs` publishes -- [`MasterVolume::default`] (see its doc comment) if no sink is
/// resolved yet, or a resolved sink's own `Props` haven't been parsed into `sink_volumes` yet
/// (the two maps update from separate PipeWire events and aren't guaranteed to catch up in the
/// same tick).
pub fn compute_master(default_name: Option<&str>, sink_names: &HashMap<u32, String>, sink_volumes: &HashMap<u32, MasterVolume>) -> MasterVolume {
    resolve_default_device(default_name, sink_names).and_then(|id| sink_volumes.get(&id).copied()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(resolve_default_device(Some("bluez_output.headset"), &sinks), Some(70));
    }

    #[test]
    fn resolve_default_device_falls_back_to_lowest_id_with_no_default_name() {
        let sinks = HashMap::from([(70, "bluez_output.headset".to_string()), (59, "alsa_output.pci-...analog-stereo".to_string())]);
        assert_eq!(resolve_default_device(None, &sinks), Some(59));
    }

    #[test]
    fn resolve_default_device_falls_back_when_the_named_sink_is_not_tracked_yet() {
        let sinks = HashMap::from([(59, "alsa_output.pci-...analog-stereo".to_string())]);
        assert_eq!(resolve_default_device(Some("bluez_output.not-seen-yet"), &sinks), Some(59));
    }

    #[test]
    fn resolve_default_device_is_none_with_no_sinks_tracked_at_all() {
        assert_eq!(resolve_default_device(None, &HashMap::new()), None);
    }

    // ---- compute_master ----

    #[test]
    fn compute_master_combines_resolution_and_lookup() {
        let sinks = HashMap::from([(59, "alsa_output.pci-...analog-stereo".to_string())]);
        let volumes = HashMap::from([(59, MasterVolume { volume: 0.3, muted: false })]);
        assert_eq!(compute_master(Some("alsa_output.pci-...analog-stereo"), &sinks, &volumes), MasterVolume { volume: 0.3, muted: false });
    }

    #[test]
    fn compute_master_defaults_when_the_resolved_sink_has_no_props_yet() {
        // resolve_default_device can find a node id before that node's own Props param has arrived
        // (they're independent events) -- compute_master must not panic on that gap.
        let sinks = HashMap::from([(59, "alsa_output.pci-...analog-stereo".to_string())]);
        assert_eq!(compute_master(Some("alsa_output.pci-...analog-stereo"), &sinks, &HashMap::new()), MasterVolume::default());
    }

    #[test]
    fn compute_master_defaults_with_nothing_tracked() {
        assert_eq!(compute_master(None, &HashMap::new(), &HashMap::new()), MasterVolume::default());
    }
}
