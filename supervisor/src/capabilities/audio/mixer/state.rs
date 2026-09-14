//! Tracked `obelisk.audio` state and pure parsing helpers, testable against recorded `pw-dump`
//! properties without a live PipeWire proxy.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use pipewire as pw;
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use super::devices::{AudioDevice, BluetoothCodecs, BluezCard, DeviceEntry, bluetooth_codecs, device_list};
use super::streams::{AppStream, CaptureApp, VideoSourceApp, running};
use crate::capabilities::audio::master;

/// Full `obelisk.audio` payload (ADR-0053 decision 3).
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
    /// Default input mute, the microphone-mute click target for privacy indicators.
    pub source_muted: bool,
    /// Every output device; `:invoke("set_default_sink", id)` takes [`AudioDevice::id`].
    pub sinks: Vec<AudioDevice>,
    /// Every input device, on the same terms as [`AudioState::sinks`].
    pub sources: Vec<AudioDevice>,
    /// One entry per app playing audio; empty is normal.
    pub apps: Vec<AppStream>,
    /// One entry per BlueZ audio device PipeWire knows, with its codecs; empty without one.
    pub bluetooth: Vec<BluetoothCodecs>,
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

/// One audio write crossing from tokio to the PipeWire loop. A channel is required because the
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
/// fields track master volume (ADR-0053 decision 3); sink proxies stay separate because
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

/// Metadata key naming the master output device, confirmed in `pw-metadata`.
pub(super) const DEFAULT_AUDIO_SINK_KEY: &str = "default.audio.sink";

/// Metadata key naming the default input device; same JSON shape as the sink key.
pub(super) const DEFAULT_AUDIO_SOURCE_KEY: &str = "default.audio.source";

#[cfg(test)]
mod tests {
    use super::super::devices::{CodecProfile, DeviceNames};
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
