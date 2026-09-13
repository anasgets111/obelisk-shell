//! Audio devices for `obelisk.audio`: sink and source names, icons and hardware routes, and the
//! codec profiles of BlueZ cards.

use std::collections::HashMap;

use pipewire::keys;
use serde::Serialize;

use super::PropsLookup;
use crate::capabilities::audio::master;

/// One § 2.4 `sinks`/`sources` entry. `name` is the user-facing `node.description`, not routing
/// `node.name` (`"alsa_output.pci-0000_00_1f.3.analog-stereo"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct AudioDevice {
    /// PipeWire registry id used by `audio:set_default_sink(id)`.
    pub id: u32,
    /// Device description, e.g. `"Built-in Audio Analog Stereo"`; neither is reboot-stable.
    pub name: String,
    /// Whether `default.audio.sink`/`default.audio.source` currently routes here.
    pub active: bool,
    /// PipeWire's `device.icon-name` hint, such as `"audio-card-analog"`; not resolved here.
    /// `None` means the node carried no hint, as with a virtual sink.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

/// One BlueZ audio device's codec choices, joined to `obelisk.bluetooth` by MAC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct BluetoothCodecs {
    /// PipeWire device registry id, the first argument of `audio:set_bluetooth_profile(device, index)`.
    pub device: u32,
    /// MAC address from WirePlumber's `bluez_card.` name, spelled as `obelisk.bluetooth` spells it.
    pub mac: String,
    /// Available profiles that name a codec, in profile index order.
    pub codecs: Vec<CodecProfile>,
    /// `index` of the active profile, or `nil` before PipeWire reports it or when it names no codec.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<i32>,
}

/// One entry of [`BluetoothCodecs::codecs`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct CodecProfile {
    /// Profile index, the second argument of `audio:set_bluetooth_profile(device, index)`.
    pub index: i32,
    /// The codec the description names, e.g. `"AAC"`, `"LDAC"`, `"mSBC"`.
    pub codec: String,
    /// PipeWire's description, e.g. `"High Fidelity Playback (A2DP Sink, codec AAC)"`.
    pub description: String,
}

/// What the mixer tracks for one bound BlueZ device. Proxies live in `bluez_devices`, keeping this
/// plain test data like [`DeviceEntry`].
#[derive(Debug, Clone, Default)]
pub(super) struct BluezCard {
    pub(super) mac: String,
    /// The profiles from the last finished `EnumProfile`, keyed by index.
    pub(super) profiles: std::collections::BTreeMap<i32, master::Profile>,
    /// The index `Profile` reports.
    pub(super) active: Option<i32>,
    /// `EnumProfile` answers collected since the last `Profile` answer, swapped in when it arrives.
    pub(super) incoming: std::collections::BTreeMap<i32, master::Profile>,
}

impl BluezCard {
    /// Collects one `EnumProfile` answer.
    pub(super) fn enumerated(&mut self, profile: master::Profile) {
        self.incoming.insert(profile.index, profile);
    }

    /// Ends an enumeration with its `Profile` answer, replacing the list and making `index` active.
    /// Returns whether either changed, so re-enumerating the same card publishes nothing.
    pub(super) fn finish_enumeration(&mut self, index: i32) -> bool {
        let profiles = std::mem::take(&mut self.incoming);
        let changed = profiles != self.profiles || self.active != Some(index);
        self.profiles = profiles;
        self.active = Some(index);
        changed
    }
}

/// Builds [`AudioState::bluetooth`], ordered by device id for deterministic publishes.
pub(super) fn bluetooth_codecs(cards: &HashMap<u32, BluezCard>) -> Vec<BluetoothCodecs> {
    let mut out: Vec<BluetoothCodecs> = cards
        .iter()
        .map(|(&device, card)| {
            let codecs: Vec<CodecProfile> = card
                .profiles
                .values()
                .filter(|profile| profile.available)
                .filter_map(|profile| {
                    Some(CodecProfile {
                        index: profile.index,
                        codec: master::codec_from_name(&profile.name)
                            .or_else(|| master::codec_of(&profile.description))?,
                        description: profile.description.clone(),
                    })
                })
                .collect();
            let active = card.active.filter(|index| codecs.iter().any(|codec| codec.index == *index));
            BluetoothCodecs { device, mac: card.mac.clone(), codecs, active }
        })
        .collect();
    out.sort_by_key(|entry| entry.device);
    out
}

/// Device routing name and display description; metadata routes by `node_name`.
#[derive(Debug, Clone, Default)]
pub(super) struct DeviceNames {
    pub(super) node_name: String,
    pub(super) description: Option<String>,
    /// [`AudioDevice::icon`], read from the same `global` event.
    pub(super) icon: Option<String>,
}

impl DeviceNames {
    /// § 2.4's `name`, falling back to the routing name so an unnamed device remains selectable.
    fn display(&self) -> String {
        self.description.clone().unwrap_or_else(|| self.node_name.clone())
    }
}

/// Tracked `Audio/Sink` or `Audio/Source` data. PipeWire handles live in `sink_nodes`/
/// `source_nodes`, keeping this test-constructible. Both directions share the same `Props` and
/// `device.id`/`card.profile.device` shape; this machine's mic is `51`/`0`, speaker `51`/`7`.
#[derive(Debug, Clone, Default)]
pub(super) struct DeviceEntry {
    pub(super) names: DeviceNames,
    /// Last raw `Props`, or `None` before the first `param`; writes need channel count and mute.
    pub(super) props: Option<master::RawSinkProps>,
    /// Hardware route, or `None` for a virtual/null node whose volume is node-owned.
    pub(super) route: Option<DeviceRoute>,
}

/// Where an ALSA-backed volume lives: `device_id` is the node's `device.id` global;
/// `profile_device` is `card.profile.device`, matching a `Route.device`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DeviceRoute {
    pub(super) device_id: u32,
    pub(super) profile_device: i32,
}

/// Builds a § 2.4 device array, ordered by registry id for deterministic publishes.
pub(super) fn device_list<'a>(
    devices: impl Iterator<Item = (u32, &'a DeviceNames)> + Clone,
    default_name: Option<&str>,
) -> Vec<AudioDevice> {
    let active =
        master::resolve_default_device(default_name, devices.clone().map(|(id, names)| (id, names.node_name.as_str())));
    let mut list: Vec<AudioDevice> = devices
        .map(|(id, names)| AudioDevice {
            id,
            name: names.display(),
            active: active == Some(id),
            icon: names.icon.clone(),
        })
        .collect();
    list.sort_by_key(|device| device.id);
    list
}

/// § 2.4's display name: `node.description`, then `node.nick`, then `node.name`.
/// Live `pw-dump` shows the first two absent on streams and present on every sink/source.
pub(super) fn device_display_name(props: &impl PropsLookup) -> Option<String> {
    props
        .get_prop(*keys::NODE_DESCRIPTION)
        .or_else(|| props.get_prop(*keys::NODE_NICK))
        .or_else(|| props.get_prop(*keys::NODE_NAME))
        .map(str::to_string)
}

/// Reads [`DeviceNames`] from `global` props; `None` if `node.name` is absent.
pub(super) fn device_names(props: &impl PropsLookup) -> Option<DeviceNames> {
    Some(DeviceNames {
        node_name: props.get_prop(*keys::NODE_NAME)?.to_string(),
        description: device_display_name(props),
        icon: props.get_prop(*keys::DEVICE_ICON_NAME).map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analog_sink_props() -> HashMap<String, String> {
        HashMap::from([
            ("media.class".to_string(), "Audio/Sink".to_string()),
            ("node.name".to_string(), "alsa_output.pci-0000_00_1f.3.analog-stereo".to_string()),
            ("node.description".to_string(), "Built-in Audio Analog Stereo".to_string()),
            ("node.nick".to_string(), "ALC256 Analog".to_string()),
        ])
    }

    #[test]
    fn device_display_name_prefers_the_description_over_the_nick_and_the_node_name() {
        assert_eq!(device_display_name(&analog_sink_props()), Some("Built-in Audio Analog Stereo".to_string()));
    }

    #[test]
    fn device_display_name_falls_back_through_nick_to_node_name() {
        let mut props = analog_sink_props();
        props.remove("node.description");
        assert_eq!(device_display_name(&props), Some("ALC256 Analog".to_string()));

        props.remove("node.nick");
        assert_eq!(device_display_name(&props), Some("alsa_output.pci-0000_00_1f.3.analog-stereo".to_string()));
    }

    #[test]
    fn device_display_name_is_none_for_a_node_with_no_name_at_all() {
        let props = HashMap::from([("media.class".to_string(), "Audio/Sink".to_string())]);
        assert_eq!(device_display_name(&props), None);
    }

    fn tracked(entries: &[(u32, &str, Option<&str>)]) -> HashMap<u32, DeviceNames> {
        entries
            .iter()
            .map(|(id, node_name, description)| {
                (
                    *id,
                    DeviceNames {
                        node_name: node_name.to_string(),
                        description: description.map(str::to_string),
                        icon: None,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn device_names_reads_the_icon_hint_beside_the_two_names_and_needs_only_the_node_name() {
        let props = HashMap::from([
            ("node.name".to_string(), "alsa_input.pci-0000_00_1f.3.analog-stereo".to_string()),
            ("node.description".to_string(), "Built-in Audio Analog Stereo".to_string()),
            ("device.icon-name".to_string(), "audio-card-analog".to_string()),
        ]);
        let names = device_names(&props).expect("a node with a name is trackable");
        assert_eq!(names.description.as_deref(), Some("Built-in Audio Analog Stereo"));
        assert_eq!(names.icon.as_deref(), Some("audio-card-analog"));

        let bare = HashMap::from([("node.name".to_string(), "null-sink".to_string())]);
        assert_eq!(device_names(&bare).map(|names| names.icon), Some(None), "no icon is an answer, not a failure");
        assert!(device_names(&HashMap::new()).is_none());
    }

    #[test]
    fn device_list_marks_the_metadata_named_device_active_and_orders_by_id() {
        let names = tracked(&[
            (70, "bluez_output.headset", Some("WH-1000XM4")),
            (59, "alsa_output.analog", Some("Built-in Audio Analog Stereo")),
        ]);

        let devices = device_list(names.iter().map(|(&id, names)| (id, names)), Some("bluez_output.headset"));

        assert_eq!(
            devices,
            vec![
                AudioDevice { id: 59, name: "Built-in Audio Analog Stereo".to_string(), active: false, icon: None },
                AudioDevice { id: 70, name: "WH-1000XM4".to_string(), active: true, icon: None },
            ]
        );
    }

    #[test]
    fn device_list_falls_back_to_the_node_name_when_no_description_was_seen() {
        let names = tracked(&[(59, "alsa_output.analog", None)]);

        let devices = device_list(names.iter().map(|(&id, names)| (id, names)), None);

        assert_eq!(
            devices,
            vec![AudioDevice { id: 59, name: "alsa_output.analog".to_string(), active: true, icon: None }]
        );
    }

    #[test]
    fn device_list_is_empty_with_nothing_tracked() {
        assert_eq!(device_list(std::iter::empty(), Some("anything")), Vec::new());
    }

    #[test]
    fn a_profile_enumeration_replaces_the_list_and_reports_only_a_change() {
        let profile = |index: i32, name: &str| master::Profile {
            index,
            name: name.to_string(),
            description: String::new(),
            available: true,
        };
        let mut card = BluezCard::default();

        card.enumerated(profile(1, "a2dp-sink-sbc"));
        card.enumerated(profile(2, "a2dp-sink-aac"));
        assert!(card.finish_enumeration(1));
        assert_eq!(card.profiles.keys().copied().collect::<Vec<_>>(), [1, 2]);

        card.enumerated(profile(2, "a2dp-sink-aac"));
        assert!(card.finish_enumeration(2));
        assert_eq!(card.profiles.keys().copied().collect::<Vec<_>>(), [2], "SBC left with the old list");
        assert_eq!(card.active, Some(2));

        // A volume step re-enumerates the same card; nothing changed, so nothing publishes.
        card.enumerated(profile(2, "a2dp-sink-aac"));
        assert!(!card.finish_enumeration(2));
    }
}
