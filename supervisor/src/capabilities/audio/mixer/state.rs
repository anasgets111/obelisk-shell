//! Tracked `obelisk.audio` state and pure parsing helpers, testable against recorded `pw-dump`
//! properties without a live PipeWire proxy.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use pipewire as pw;
use pw::keys;
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use super::PropsLookup;
use super::streams::{AppStream, CaptureApp, VideoSourceApp, running};
use crate::capabilities::audio::master;

/// Full `obelisk.audio` payload (§ 2.4, ADR-0053 decision 3).
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct AudioState {
    /// Master output volume, range `[0.0, 1.0]`, derived from the default sink's `channelVolumes`.
    pub volume: f32,
    /// Master output mute.
    pub muted: bool,
    /// Default input volume, range `[0.0, 1.0]`, using the sink's cube-root conversion
    /// (`pw-cli enum-params <source> Props` has the same shape). `0.0` before first `Props` or
    /// with no input device.
    pub source_volume: f32,
    /// Default input mute, the microphone-mute click target for privacy indicators (§ 3.2).
    pub source_muted: bool,
    /// Every output device; `audio:set_default_sink(id)` takes [`AudioDevice::id`].
    pub sinks: Vec<AudioDevice>,
    /// Every input device, on the same terms as [`AudioState::sinks`].
    pub sources: Vec<AudioDevice>,
    /// One entry per app playing audio; empty is normal.
    pub apps: Vec<AppStream>,
    /// One entry per BlueZ audio device PipeWire knows, with its codecs; empty without one.
    pub bluetooth: Vec<BluetoothCodecs>,
}

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
fn bluetooth_codecs(cards: &HashMap<u32, BluezCard>) -> Vec<BluetoothCodecs> {
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
fn device_list<'a>(
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

/// All PipeWire inputs to `obelisk.privacy` in one snapshot (ADR-0137). One channel keeps the three
/// lists from arriving out of order when a config draws them together. All three lists change on
/// the same registry events; separate senders would add orderings where one list is a push behind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrivacySources {
    pub cameras: Vec<VideoSourceApp>,
    pub microphones: Vec<CaptureApp>,
    pub screencasts: Vec<CaptureApp>,
}

/// One § 3.2 write crossing from tokio to the PipeWire loop. A channel is required because the
/// proxies are `!Send`; `pipewire::channel` gives the loop an eventfd, so commands apply between
/// PipeWire events. Array ids resolve against live maps; stale ids are logged and dropped because
/// PipeWire recycles them.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioCommand {
    SetMasterVolume(f32),
    SetMasterMuted(bool),
    ToggleMasterMute,
    SetDefaultSink(u32),
    SetDefaultSource(u32),
    SetSourceVolume(f32),
    SetSourceMuted(bool),
    ToggleSourceMute,
    SetAppVolume { id: u32, volume: f32 },
    SetAppMuted { id: u32, muted: bool },
    SetBluetoothProfile { device: u32, index: i32 },
}

/// Listener-owned state and snapshot channels. Held for the thread lifetime. `sink_*`/`metadata*`
/// fields track § 2.4 master volume (ADR-0053 decision 3); sink proxies stay separate because
/// they use `param`, while `nodes` handles stream/video `info`.
pub(super) struct MixerState {
    /// False until PipeWire has answered for everything the listener asked for at startup, which
    /// `registry::run_inner` decides with two `core.sync` barriers. Both publishers build their
    /// payload from the maps and send nothing while it is false, so the first snapshot a config
    /// ever sees is a complete one rather than a default it will watch get corrected.
    ///
    /// The maps keep updating throughout. This gates sending, not tracking.
    pub(super) hydrated: bool,
    /// Maps are keyed by node id; `BTreeMap` publishes them in id order.
    pub(super) apps: BTreeMap<u32, AppStream>,
    pub(super) video_sources: BTreeMap<u32, VideoSourceApp>,
    /// `Stream/Input/Audio` nodes, idle ones included (ADR-0137).
    pub(super) microphones: BTreeMap<u32, CaptureApp>,
    /// `Stream/Output/Video` nodes, idle ones included (ADR-0137).
    pub(super) screencasts: BTreeMap<u32, CaptureApp>,
    pub(super) nodes: HashMap<u32, (pw::node::Node, pw::node::NodeListener)>,
    pub(super) updates: UnboundedSender<AudioState>,
    pub(super) privacy_updates: UnboundedSender<PrivacySources>,
    /// `Audio/Sink` id -> tracked sink data; its `param` listener replaces stream `info`.
    pub(super) sinks: HashMap<u32, DeviceEntry>,
    /// Bound sink proxies/listeners, separate from `sinks` so state stays plain test data.
    pub(super) sink_nodes: HashMap<u32, (pw::node::Node, pw::node::NodeListener)>,
    /// `Audio/Source` id -> the same `Props`-backed entry used for master volume.
    pub(super) sources: HashMap<u32, DeviceEntry>,
    pub(super) source_nodes: HashMap<u32, (pw::node::Node, pw::node::NodeListener)>,
    /// Bound ALSA `Device` proxies/listeners for writes. Hardware volume lives on `Route`, not
    /// the node; without these, `set_volume` is accepted and silently discarded.
    pub(super) devices: HashMap<u32, (Rc<pw::device::Device>, pw::device::DeviceListener)>,
    /// `(device global id, card.profile.device)` -> active `Route` index from the device.
    pub(super) device_routes: HashMap<(u32, i32), i32>,
    /// BlueZ `Device` id -> its MAC and profiles, for [`AudioState::bluetooth`].
    pub(super) bluez_cards: HashMap<u32, BluezCard>,
    /// Bound BlueZ `Device` proxies/listeners, separate from `bluez_cards` so state stays plain.
    pub(super) bluez_devices: HashMap<u32, (Rc<pw::device::Device>, pw::device::DeviceListener)>,
    /// `Stream/Output/Audio` id -> raw `Props`, using the sink's pod shape and parser.
    pub(super) app_props: HashMap<u32, master::RawSinkProps>,
    /// Names selected by `default.audio.sink`/`default.audio.source`, or `None` before arrival.
    pub(super) default_sink_name: Option<String>,
    pub(super) default_source_name: Option<String>,
    /// Bound `default` metadata and its registry id; `Metadata` has no id accessor, so removal
    /// needs this. `None` until `metadata.name == "default"` is found.
    pub(super) metadata: Option<(pw::metadata::Metadata, pw::metadata::MetadataListener)>,
    pub(super) metadata_id: Option<u32>,
}

impl MixerState {
    /// Empty maps, not yet hydrated.
    pub(super) fn new(updates: UnboundedSender<AudioState>, privacy_updates: UnboundedSender<PrivacySources>) -> Self {
        Self {
            hydrated: false,
            apps: BTreeMap::new(),
            video_sources: BTreeMap::new(),
            microphones: BTreeMap::new(),
            screencasts: BTreeMap::new(),
            nodes: HashMap::new(),
            updates,
            privacy_updates,
            sinks: HashMap::new(),
            sink_nodes: HashMap::new(),
            sources: HashMap::new(),
            source_nodes: HashMap::new(),
            devices: HashMap::new(),
            device_routes: HashMap::new(),
            bluez_cards: HashMap::new(),
            bluez_devices: HashMap::new(),
            app_props: HashMap::new(),
            default_sink_name: None,
            default_source_name: None,
            metadata: None,
            metadata_id: None,
        }
    }

    /// Entry map for one direction, shared by binding and writing.
    pub(super) fn device_entries_mut(&mut self, kind: DefaultDevice) -> &mut HashMap<u32, DeviceEntry> {
        match kind {
            DefaultDevice::Sink => &mut self.sinks,
            DefaultDevice::Source => &mut self.sources,
        }
    }

    pub(super) fn device_entries(&self, kind: DefaultDevice) -> &HashMap<u32, DeviceEntry> {
        match kind {
            DefaultDevice::Sink => &self.sinks,
            DefaultDevice::Source => &self.sources,
        }
    }

    /// `node.name` selected by `default` metadata for one direction.
    pub(super) fn default_name(&self, kind: DefaultDevice) -> Option<&str> {
        match kind {
            DefaultDevice::Sink => self.default_sink_name.as_deref(),
            DefaultDevice::Source => self.default_source_name.as_deref(),
        }
    }

    /// Publishes even if the receiver is absent; that is startup or shutdown, not a tracking error.
    ///
    /// Silent until [`MixerState::hydrated`]. A sink's `props` arrive on a later `param` event than
    /// the `global` that binds it, and `compute_master` reads a missing `Props` as `0.0`, so every
    /// publish before that lands claims a volume of zero. A config comparing against its previous
    /// payload reads the correction as the user having changed the volume: the OSD showed a card
    /// for the true volume at every shell start.
    pub(super) fn publish_audio(&self) {
        if !self.hydrated {
            return;
        }
        let master = master::compute_master(
            self.default_sink_name.as_deref(),
            self.sinks.iter().map(|(&id, sink)| (id, sink.names.node_name.as_str())),
            |id| self.sinks.get(&id).and_then(|sink| sink.props.clone()),
        );
        let source = master::compute_master(
            self.default_source_name.as_deref(),
            self.sources.iter().map(|(&id, source)| (id, source.names.node_name.as_str())),
            |id| self.sources.get(&id).and_then(|source| source.props.clone()),
        );
        // Join here: identity and volume arrive on unordered PipeWire `info` and `param` events;
        // folding volume in at info time could overwrite a reading already landed.
        let apps = self
            .apps
            .values()
            .cloned()
            .map(|app| match self.app_props.get(&app.id).map(master::master_volume_from_props) {
                Some(measured) => AppStream { volume: measured.volume, muted: measured.muted, ..app },
                None => app,
            })
            .collect();
        let state = AudioState {
            volume: master.volume,
            muted: master.muted,
            source_volume: source.volume,
            source_muted: source.muted,
            sinks: device_list(
                self.sinks.iter().map(|(&id, sink)| (id, &sink.names)),
                self.default_sink_name.as_deref(),
            ),
            sources: device_list(
                self.sources.iter().map(|(&id, source)| (id, &source.names)),
                self.default_source_name.as_deref(),
            ),
            apps,
            bluetooth: bluetooth_codecs(&self.bluez_cards),
        };
        let _ = self.updates.send(state);
    }

    /// Publishes all three privacy lists together (ADR-0137), even when only one changed.
    /// Recomputing the two it did not touch is three map walks.
    pub(super) fn publish_privacy(&self) {
        if !self.hydrated {
            return;
        }
        let _ = self.privacy_updates.send(PrivacySources {
            cameras: self.video_sources.values().cloned().collect(),
            microphones: running(&self.microphones),
            screencasts: running(&self.screencasts),
        });
    }
}

/// Which default-device name a metadata `property` event addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DefaultDevice {
    Sink,
    Source,
}

/// Metadata key naming § 2.4's master output device, confirmed in `pw-metadata`.
pub(super) const DEFAULT_AUDIO_SINK_KEY: &str = "default.audio.sink";

/// Metadata key naming § 2.4's default input device; same JSON shape as the sink key.
pub(super) const DEFAULT_AUDIO_SOURCE_KEY: &str = "default.audio.source";

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_stream(node_id: u32) -> AppStream {
        AppStream {
            id: node_id,
            pid: 100 + node_id as i32,
            name: Some(format!("app-{node_id}")),
            process_name: None,
            volume: 1.0,
            muted: false,
        }
    }

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

    fn sink_at(node_name: &str, description: Option<&str>, props: Option<master::RawSinkProps>) -> DeviceEntry {
        DeviceEntry {
            names: DeviceNames {
                node_name: node_name.to_string(),
                description: description.map(str::to_string),
                icon: None,
            },
            props,
            route: None,
        }
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
    fn audio_state_serializes_as_the_flat_shape_docs_adr_0053_specifies() {
        // 0.5 avoids serde_json's long widened-f32 tail, keeping this about field shape.
        let stream = sample_stream(1);
        let state = AudioState {
            volume: 0.5,
            muted: false,
            source_volume: 0.25,
            source_muted: true,
            sinks: vec![AudioDevice {
                id: 59,
                name: "Built-in Audio Analog Stereo".to_string(),
                active: true,
                icon: Some("audio-card-analog".to_string()),
            }],
            sources: vec![AudioDevice {
                id: 60,
                name: "Built-in Audio Analog Stereo".to_string(),
                active: true,
                icon: None,
            }],
            apps: vec![stream.clone()],
            bluetooth: vec![BluetoothCodecs {
                device: 80,
                mac: "AA:BB:CC:DD:EE:FF".to_string(),
                codecs: vec![CodecProfile {
                    index: 2,
                    codec: "AAC".to_string(),
                    description: "High Fidelity Playback (A2DP Sink, codec AAC)".to_string(),
                }],
                active: Some(2),
            }],
        };
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "volume": 0.5,
                "muted": false,
                "source_volume": 0.25,
                "source_muted": true,
                "sinks": [{ "id": 59, "name": "Built-in Audio Analog Stereo", "active": true, "icon": "audio-card-analog" }],
                "sources": [{ "id": 60, "name": "Built-in Audio Analog Stereo", "active": true }],
                "apps": [{
                    "id": stream.id,
                    "pid": stream.pid,
                    "name": stream.name,
                    "process_name": stream.process_name,
                    "volume": stream.volume,
                    "muted": stream.muted,
                }],
                "bluetooth": [{
                    "device": 80,
                    "mac": "AA:BB:CC:DD:EE:FF",
                    "codecs": [{ "index": 2, "codec": "AAC", "description": "High Fidelity Playback (A2DP Sink, codec AAC)" }],
                    "active": 2,
                }],
            })
        );
    }

    /// Raw `Props` at a linear volume, so tests name the linear value rather than its cube.
    fn props_at(linear: f32, muted: bool) -> master::RawSinkProps {
        master::RawSinkProps {
            mute: muted,
            channel_volumes: master::cubed_channel_volumes(linear, 2).expect("two channels is not zero"),
        }
    }

    /// `publish_audio` fixture with empty proxy maps; tests fill only fields they exercise.
    ///
    /// Hydrated, because every test below asserts on what a publish carries. The gate itself is
    /// pinned by `a_state_that_has_not_hydrated_publishes_nothing`.
    fn mixer_state(
        updates: UnboundedSender<AudioState>,
        privacy_updates: UnboundedSender<PrivacySources>,
    ) -> MixerState {
        MixerState { hydrated: true, ..MixerState::new(updates, privacy_updates) }
    }

    /// The startup gate. A sink whose `Props` have not arrived reads as volume zero, and a config
    /// comparing against its previous payload sees the correction as a volume change, so the very
    /// snapshot this suppresses is the one that put a spurious card on screen at every shell start.
    #[test]
    fn a_state_that_has_not_hydrated_publishes_nothing() {
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, mut privacy_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = mixer_state(updates, privacy_updates);
        state.hydrated = false;
        // A bound sink with no `Props` yet: exactly the shape the first `global` burst leaves.
        state.sinks = HashMap::from([(59, sink_at("alsa_output.pci-...analog-stereo", None, None))]);
        state.default_sink_name = Some("alsa_output.pci-...analog-stereo".to_string());

        state.publish_audio();
        state.publish_privacy();

        assert!(rx.try_recv().is_err(), "an unhydrated state must not publish its zeroed master volume");
        assert!(privacy_rx.try_recv().is_err(), "an unhydrated state must not publish its empty privacy lists");

        state.hydrated = true;
        state.publish_audio();
        state.publish_privacy();
        assert!(rx.try_recv().is_ok(), "opening the gate must publish audio on the next call");
        assert!(privacy_rx.try_recv().is_ok(), "opening the gate must publish privacy on the next call");
    }

    #[test]
    fn publish_audio_combines_master_volume_and_app_snapshot() {
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, _privacy_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = mixer_state(updates, privacy_updates);
        state.sinks =
            HashMap::from([(59, sink_at("alsa_output.pci-...analog-stereo", None, Some(props_at(0.3, false))))]);
        state.default_sink_name = Some("alsa_output.pci-...analog-stereo".to_string());
        state.apps.insert(1, sample_stream(1));

        state.publish_audio();

        let published = rx.try_recv().expect("publish_audio should have sent a snapshot");
        assert!((published.volume - 0.3).abs() < 1e-6, "expected ~0.3, got {}", published.volume);
        assert!(!published.muted);
        assert_eq!(published.apps, vec![sample_stream(1)]);
    }

    #[test]
    fn publish_audio_reports_the_master_volume_default_with_no_sink_tracked() {
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, _privacy_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = mixer_state(updates, privacy_updates);

        state.publish_audio();

        let published = rx.try_recv().expect("publish_audio should have sent a snapshot");
        assert_eq!(published.volume, master::MasterVolume::default().volume);
        assert_eq!(published.muted, master::MasterVolume::default().muted);
        assert!(published.apps.is_empty());
        assert!(published.sinks.is_empty());
        assert!(published.sources.is_empty());
    }

    #[test]
    fn publish_audio_lists_only_the_bluetooth_profiles_that_offer_a_codec_now() {
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, _privacy_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = mixer_state(updates, privacy_updates);
        let profile = |index: i32, name: &str, description: &str, available: bool| {
            let profile =
                master::Profile { index, name: name.to_string(), description: description.to_string(), available };
            (index, profile)
        };
        state.bluez_cards = HashMap::from([(
            80,
            BluezCard {
                mac: "AA:BB:CC:DD:EE:FF".to_string(),
                profiles: [
                    profile(0, "off", "Off", true),
                    profile(1, "a2dp-sink-sbc", "High Fidelity Playback (A2DP Sink, codec SBC)", true),
                    // A translated description names no codec in English; the name still does.
                    profile(2, "a2dp-sink-aac", "High-Fidelity-Wiedergabe (A2DP-Senke, Codec AAC)", true),
                    profile(3, "a2dp-sink-ldac", "High Fidelity Playback (A2DP Sink, codec LDAC)", false),
                ]
                .into_iter()
                .collect(),
                active: Some(2),
                ..BluezCard::default()
            },
        )]);

        state.publish_audio();

        let published = rx.try_recv().expect("publish_audio should have sent a snapshot");
        let card = &published.bluetooth[0];
        assert_eq!(card.mac, "AA:BB:CC:DD:EE:FF");
        let codecs: Vec<&str> = card.codecs.iter().map(|codec| codec.codec.as_str()).collect();
        assert_eq!(codecs, ["SBC", "AAC"], "off names no codec and LDAC is unavailable");
        assert_eq!(card.active, Some(2));
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

    #[test]
    fn publish_audio_overlays_a_streams_own_props_reading_onto_its_entry() {
        // Identity arrives in `info`, volume in `param`; this is their join point.
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, _privacy_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = mixer_state(updates, privacy_updates);
        state.apps.insert(1, sample_stream(1));
        state.app_props = HashMap::from([(1, props_at(0.42, true))]);

        state.publish_audio();

        let published = rx.try_recv().expect("publish_audio should have sent a snapshot");
        assert!((published.apps[0].volume - 0.42).abs() < 1e-6, "expected ~0.42, got {}", published.apps[0].volume);
        assert!(published.apps[0].muted);
    }

    #[test]
    fn publish_audio_leaves_a_stream_whose_props_have_not_arrived_at_pipewires_own_untouched_values() {
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, _privacy_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = mixer_state(updates, privacy_updates);
        state.apps.insert(1, sample_stream(1));
        state.app_props = HashMap::from([(99, props_at(0.42, true))]);

        state.publish_audio();

        let published = rx.try_recv().expect("publish_audio should have sent a snapshot");
        assert_eq!(published.apps[0].volume, 1.0, "another stream's reading must not leak onto this one");
        assert!(!published.apps[0].muted);
    }

    #[test]
    fn publish_audio_reports_sinks_and_sources_with_their_own_active_flags() {
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, _privacy_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = mixer_state(updates, privacy_updates);
        state.sinks = HashMap::from([
            (59, sink_at("alsa_output.analog", Some("Built-in Audio Analog Stereo"), None)),
            (70, sink_at("bluez_output.headset", Some("WH-1000XM4"), None)),
        ]);
        state.default_sink_name = Some("bluez_output.headset".to_string());
        state.sources =
            HashMap::from([(60, sink_at("alsa_input.analog", Some("Built-in Microphone"), Some(props_at(0.6, true))))]);
        state.default_source_name = Some("alsa_input.analog".to_string());

        state.publish_audio();

        let published = rx.try_recv().expect("publish_audio should have sent a snapshot");
        assert_eq!(published.sinks.iter().map(|sink| sink.active).collect::<Vec<_>>(), [false, true]);
        assert_eq!(published.sinks[1].name, "WH-1000XM4");
        assert_eq!(
            published.sources,
            vec![AudioDevice { id: 60, name: "Built-in Microphone".to_string(), active: true, icon: None }]
        );
        // The source's Props use the master's cube-root conversion.
        assert!((published.source_volume - 0.6).abs() < 1e-6, "expected ~0.6, got {}", published.source_volume);
        assert!(published.source_muted);
    }
}
