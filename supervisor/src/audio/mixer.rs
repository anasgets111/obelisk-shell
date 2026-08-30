//! PipeWire registry listener for per-app audio streams (build-steps.md Phase 6, point 2).
//!
//! A stream node's owning process pid is `application.process.id` (`PW_KEY_APP_PROCESS_ID`),
//! set on the stream node's own properties -- not `sec.pid`/`node.client-id` (not real
//! `pipewire-rs` keys) and not `pipewire.sec.pid` (set on the Client object, but for a stream
//! routed through `pipewire-pulse` that's `pipewire-pulse`'s own pid, not the application's).
//! Matches `/proc/{pid}/comm` for the real app in every case checked. See docs/adr/0016.
//!
//! `media.class == "Stream/Output/Audio"` identifies a playback stream, verified against real
//! `pw-dump` output.
//!
//! [`on_global`] checks `media.class` at registry `global` time and always binds a matching
//! node, without also requiring `application.process.id` yet: a `pipewire-pulse`-routed
//! stream's `global` event fires before `pipewire-pulse` pushes
//! `application.process.id`/`application.name` onto the node, so filtering on the full parse at
//! `global` time misses every such stream. The pid arrives moments later through the bound
//! node's own `info` event, which [`build_app_stream`] runs against.
//!
//! `info` fires on any change PipeWire tracks for the node, not just a props change -- state
//! transitions (RUNNING <-> IDLE/SUSPENDED), params, and ports changes all trigger it too.
//! `NodeInfoRef::props()` returns `Some(&DictRef)` on every `info` call, but PipeWire's C
//! marshaller only fills real entries into that dict when `change_mask` includes
//! `PW_NODE_CHANGE_MASK_PROPS`; any other kind of `info` event carries a non-null but empty
//! dict. So the node listener checks `NodeInfoRef::change_mask()` for `NodeChangeMask::PROPS`
//! before running [`build_app_stream`], leaving a non-PROPS event's tracked entry untouched
//! rather than misreading it as the stream disappearing.
//!
//! The very first `info` call after `registry.bind()` is guaranteed to carry
//! `NodeChangeMask::PROPS`: upstream PipeWire's `global_bind` (`impl-node.c`) unconditionally
//! sets `change_mask = PW_NODE_CHANGE_MASK_ALL` before any other event can reach a freshly
//! bound resource, and `protocol-native.c`'s marshaller only nulls the props dict when the
//! PROPS bit is unset, so `ALL` always carries a real dict. Later `info` calls go through
//! `emit_info_changed` instead, which forwards only the bits that actually changed. Confirmed
//! live too: `pw-mon` attached before starting `paplay` showed the node's first `added:` block
//! already carrying a full `properties:` section, with no earlier emptier `info` event.
//!
//! ponytail: the `media.class` filter runs once, at `global` time. A node whose `media.class`
//! starts as something else and only later changes to `Stream/Output/Audio` won't be picked up
//! -- real clients set `media.class` once at creation and don't mutate it. If that stops being
//! true, the fix is binding every `ObjectType::Node` unconditionally and filtering inside the
//! `info` callback instead.
//!
//! Master output volume/mute (§ 2.4, docs/adr/0053 decision 3) is a second, mostly independent
//! tracking job on the same registry listener: `Audio/Sink` nodes' `SPA_PARAM_Props` param (via
//! `param` events, not `info`) and the `default` `Metadata` object's `default.audio.sink` key
//! (which names the master sink by `node.name`). This file only wires the PipeWire event
//! plumbing; [`master`] holds the pure parsing and resolution logic, including the linear/cubic
//! volume conversion.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::rc::Rc;

use pipewire as pw;
use pw::keys;
use pw::registry::GlobalObject;
use pw::spa::pod::deserialize::PodDeserializer;
use pw::spa::pod::Value;
use pw::spa::utils::dict::DictRef;
use pw::types::ObjectType;
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use super::master;

/// `media.class` value stream playback nodes carry. Verified against real `pw-dump` output --
/// see the module doc comment.
const STREAM_OUTPUT_AUDIO: &str = "Stream/Output/Audio";

/// `media.class` value a camera-capture PipeWire node carries (docs/adr/0034). Tracked here,
/// not a second PipeWire connection, purely as a name-enrichment source for `oblisk.privacy`'s
/// camera detection -- PipeWire only sees the portal-routed subset of camera users, so this is
/// supplementary to that capability's own kernel-level detection, never primary.
const VIDEO_SOURCE: &str = "Video/Source";

/// `media.class` value an input capture device node carries (§ 2.4's `sources`). Unlike
/// [`AUDIO_SINK`], a source is never bound: § 2.4's source object is `id`/`name`/`active`, all
/// answerable from the `global` event's own props plus the default-source metadata key.
///
/// No monitor filter, deliberately: PulseAudio synthesizes a `.monitor` source per sink, but a
/// native PipeWire registry does not, and `pw-dump` on this machine lists exactly one
/// `Audio/Source` beside one `Audio/Sink`, no monitor node between them.
const AUDIO_SOURCE: &str = "Audio/Source";

/// The sink node property naming which of its device's routes it plays through. Not in
/// `pipewire-rs`'s `keys` module, so spelled out here; read live off `pw-dump`, where this
/// machine's analog output carries `card.profile.device = 7` and the matching `Route` object on
/// device 49 carries `device: 7`.
const CARD_PROFILE_DEVICE: &str = "card.profile.device";

/// One playback stream node PipeWire has advertised, filtered to
/// `media.class == "Stream/Output/Audio"` and resolved to its owning process.
///
/// `Serialize`: this is what `main.rs` puts in a `StateSnapshot`'s `payload`, pushed to the
/// Renderer over the control socket as-is. Field names match § 2.4's spelling (`id`, `name`,
/// docs/adr/0053 decision 3). `pid`/`process_name` are kept even though § 2.4 doesn't list them
/// -- docs/adr/0016 exists because finding the owning process was genuinely hard, and discarding
/// that answer would throw away the one part of this payload that took real work.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AppStream {
    /// PipeWire registry id of the stream node -- the key [`AudioApps`] tracks entries by.
    pub id: u32,
    /// `application.process.id`: the pid PipeWire recorded for the stream's owning process.
    pub pid: i32,
    /// `application.name`, if the client set one.
    pub name: Option<String>,
    /// `/proc/{pid}/comm` for `pid`, if the process still existed when this stream was seen.
    pub process_name: Option<String>,
    /// § 2.4's per-app volume, range `[0.0, 1.0]`. Read from this stream node's own
    /// `SPA_PARAM_Props` through the same cube-root conversion the master sink uses (see
    /// [`master`]): a stream stores `channelVolumes` cubed exactly as a sink does, confirmed
    /// with `pw-cli enum-params <id> Props` against a live playback stream. `1.0` until that
    /// param arrives, PipeWire's own untouched value for a stream never adjusted.
    pub volume: f32,
    /// § 2.4's per-app mute, from the same `Props` param as `volume`.
    pub muted: bool,
}

/// String key/value lookup PipeWire property dicts implement -- lets the parsing below run
/// against both a live `DictRef` and, in tests, a plain map built from recorded `pw-dump`
/// output.
trait PropsLookup {
    fn get_prop(&self, key: &str) -> Option<&str>;
}

impl PropsLookup for DictRef {
    fn get_prop(&self, key: &str) -> Option<&str> {
        self.get(key)
    }
}

impl PropsLookup for HashMap<String, String> {
    fn get_prop(&self, key: &str) -> Option<&str> {
        self.get(key).map(String::as_str)
    }
}

/// A stream node's `media.class`/pid/name properties, parsed but not yet resolved to a
/// process name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedStream {
    pid: i32,
    app_name: Option<String>,
}

/// Whether `props` is a playback stream node (`media.class == "Stream/Output/Audio"`).
fn is_stream_output_audio(props: &impl PropsLookup) -> bool {
    props.get_prop(*keys::MEDIA_CLASS) == Some(STREAM_OUTPUT_AUDIO)
}

/// Which of the two node kinds this listener tracks a `global` event as, decided once at
/// `global` time and carried into the bound node's `info` closure -- an `info` event's own
/// props dict can be empty (a state-only change), so the kind can't be re-derived from every
/// `info` call, only remembered from the classification made here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    Audio,
    Video,
}

/// Classifies a `global` event's properties by `media.class`, or `None` for anything neither
/// half of this listener cares about (sinks, sources, non-stream nodes, etc.).
fn classify(props: &impl PropsLookup) -> Option<NodeKind> {
    match props.get_prop(*keys::MEDIA_CLASS) {
        Some(STREAM_OUTPUT_AUDIO) => Some(NodeKind::Audio),
        Some(VIDEO_SOURCE) => Some(NodeKind::Video),
        _ => None,
    }
}

/// Parses `props` into a [`ParsedStream`] if it's a `Stream/Output/Audio` node with a valid
/// `application.process.id`. `None` for anything else, including a stream node PipeWire hasn't
/// finished populating yet (see the module doc comment).
fn parse_stream_props(props: &impl PropsLookup) -> Option<ParsedStream> {
    if !is_stream_output_audio(props) {
        return None;
    }
    let pid = props.get_prop(*keys::APP_PROCESS_ID)?.parse().ok()?;
    let app_name = props.get_prop(*keys::APP_NAME).map(str::to_string);
    Some(ParsedStream { pid, app_name })
}

/// Reads `/proc/{pid}/comm` for the process name `pid` maps to. `None` if the process has
/// already exited or `/proc` isn't readable.
fn resolve_process_name(pid: i32) -> Option<String> {
    let comm = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(comm.trim_end().to_string())
}

/// Parses `props` and resolves the owning process's name in one step -- what both the
/// registry `global` handler and the bound node's `info` handler call.
fn build_app_stream(node_id: u32, props: &impl PropsLookup) -> Option<AppStream> {
    let parsed = parse_stream_props(props)?;
    let process_name = resolve_process_name(parsed.pid);
    Some(AppStream {
        id: node_id,
        pid: parsed.pid,
        name: parsed.app_name,
        process_name,
        // What PipeWire itself reports for a stream nobody has touched, confirmed with a live
        // pw-cli enum-params <id> Props. Replaced as soon as this node's own Props param
        // arrives. Not MasterVolume::default()'s 0.0: that's the right unknown for a master
        // volume nothing has resolved yet, wrong here, where silence is a state a stream can
        // really be in.
        volume: 1.0,
        muted: false,
    })
}

/// Applies one bound node's `info` event to `apps`. `has_props_change` is whether the event's
/// `change_mask` included `NodeChangeMask::PROPS` -- a state-only or params/ports-only info
/// event carries an empty props dict instead (see the module doc comment). Skipping the
/// upsert/remove decision on those keeps a still-live stream from being dropped over an
/// unrelated notification. Safe to gate unconditionally: PipeWire's `global_bind` always sends
/// the first `info` call for a freshly bound node with every change-mask bit set, PROPS
/// included.
fn apply_info_event(apps: &mut AudioApps, node_id: u32, has_props_change: bool, props: Option<&impl PropsLookup>) {
    if !has_props_change {
        return;
    }
    match props.and_then(|props| build_app_stream(node_id, props)) {
        Some(app) => apps.upsert(app),
        None => {
            apps.remove(node_id);
        }
    }
}

/// Live per-app audio stream list, keyed by PipeWire node id so add/remove/property-change
/// events can update it in place.
#[derive(Debug, Default)]
pub struct AudioApps {
    streams: HashMap<u32, AppStream>,
}

impl AudioApps {
    pub fn new() -> Self {
        Self::default()
    }

    /// Node-added or node-properties-changed: insert or replace `stream`'s entry.
    pub fn upsert(&mut self, stream: AppStream) {
        self.streams.insert(stream.id, stream);
    }

    /// Node removed from the registry, or its properties no longer parse as an audio stream.
    pub fn remove(&mut self, node_id: u32) {
        self.streams.remove(&node_id);
    }

    /// The current streams, sorted by node id for a deterministic snapshot order.
    pub fn snapshot(&self) -> Vec<AppStream> {
        let mut apps: Vec<AppStream> = self.streams.values().cloned().collect();
        apps.sort_by_key(|app| app.id);
        apps
    }
}

/// The full `oblisk.audio` payload (§ 2.4, docs/adr/0053 decision 3): master output
/// volume/mute plus the per-app stream list.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AudioState {
    /// Master output volume, range `[0.0, 1.0]` -- see [`master`]'s module doc comment for how
    /// this is derived from the default sink's `channelVolumes`.
    pub volume: f32,
    /// Master output mute.
    pub muted: bool,
    pub sinks: Vec<AudioDevice>,
    pub sources: Vec<AudioDevice>,
    pub apps: Vec<AppStream>,
}

/// One § 2.4 `sinks`/`sources` entry. Both arrays are the same three fields, so they are the
/// same type: an output and an input differ in which `media.class` produced them and in
/// nothing a config reads.
///
/// `name` is § 2.4's "user-friendly description", which is `node.description` (`"Built-in Audio
/// Analog Stereo"`), not the `node.name` the metadata keys route by
/// (`"alsa_output.pci-0000_00_1f.3.analog-stereo"`). Both exist on every device this machine
/// advertises, and only one is meant for a person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioDevice {
    /// PipeWire registry id, which is what `audio:set_default_sink(id)` takes.
    pub id: u32,
    pub name: String,
    /// Whether this is the device the `default.audio.sink`/`default.audio.source` metadata key
    /// currently routes to.
    pub active: bool,
}

/// The two names every tracked device carries, one per purpose. `node_name` is what the
/// `default.audio.*` metadata keys route by; `description` is what § 2.4's arrays show a person.
#[derive(Debug, Clone, Default)]
struct DeviceNames {
    node_name: String,
    description: Option<String>,
}

impl DeviceNames {
    /// § 2.4's `name`, falling back to the routing name when nothing friendlier was advertised:
    /// a device with no readable name is still one a config can switch to.
    fn display(&self) -> String {
        self.description.clone().unwrap_or_else(|| self.node_name.clone())
    }
}

/// One tracked `Audio/Sink`, as data. Everything the read path publishes and the write path
/// needs to find the hardware, with no PipeWire handle in it: the bound proxies live beside
/// this in `sink_nodes`, keeping this struct constructible in a test.
#[derive(Debug, Clone, Default)]
struct SinkEntry {
    names: DeviceNames,
    /// The raw `Props` last read off this node, or `None` before its first `param` event. Raw,
    /// not the derived [`master::MasterVolume`]: the write path needs the channel count and the
    /// mute the caller is not changing.
    props: Option<master::RawSinkProps>,
    /// The hardware device behind this sink, for sinks that have one. `None` for a virtual or
    /// null sink, whose volume really does live on the node.
    route: Option<SinkRoute>,
}

/// Where an ALSA-backed sink's volume actually lives. `device_id` is the PipeWire `Device` global
/// the sink node's own `device.id` property names; `profile_device` is its `card.profile.device`,
/// which is what a `Route` object's `device` field matches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SinkRoute {
    device_id: u32,
    profile_device: i32,
}

/// Builds one of § 2.4's device arrays. Ordered by registry id so two publishes of the same
/// registry produce the same list, for the reason `AudioApps::snapshot` already sorts.
fn device_list<'a>(devices: impl Iterator<Item = (u32, &'a DeviceNames)> + Clone, default_name: Option<&str>) -> Vec<AudioDevice> {
    let active = master::resolve_default_device(default_name, devices.clone().map(|(id, names)| (id, names.node_name.as_str())));
    let mut list: Vec<AudioDevice> = devices.map(|(id, names)| AudioDevice { id, name: names.display(), active: active == Some(id) }).collect();
    list.sort_by_key(|device| device.id);
    list
}

/// § 2.4's "user-friendly description" for a device node, in PipeWire's own order of
/// friendliness: `node.description` ("Built-in Audio Analog Stereo"), then `node.nick` ("ALC256
/// Analog"), then `node.name` as the last resort. All three read off this machine's live
/// `pw-dump`; the first two are absent on stream nodes and present on every sink and source.
fn device_display_name(props: &impl PropsLookup) -> Option<String> {
    props
        .get_prop(*keys::NODE_DESCRIPTION)
        .or_else(|| props.get_prop(*keys::NODE_NICK))
        .or_else(|| props.get_prop(*keys::NODE_NAME))
        .map(str::to_string)
}

/// One `Video/Source` node PipeWire has advertised, resolved just enough for `oblisk.privacy`'s
/// name-enrichment (docs/adr/0034): `pid` is what a kernel-detected `/dev/videoN` opener's own
/// pid gets matched against; `app_name` is the nicer name PipeWire supplies for the match. No
/// `process_name` field (unlike [`AppStream`]) -- `oblisk.privacy`'s own `/proc/{pid}/comm`
/// fallback already covers that case for pids PipeWire doesn't see at all, so resolving it here
/// too would be redundant work this capability never reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VideoSourceApp {
    pub node_id: u32,
    pub pid: i32,
    pub app_name: Option<String>,
}

/// Parses `props` into a [`VideoSourceApp`] if it's a `Video/Source` node with a valid
/// `application.process.id` -- mirrors [`parse_stream_props`], one field simpler.
fn parse_video_source_props(node_id: u32, props: &impl PropsLookup) -> Option<VideoSourceApp> {
    if props.get_prop(*keys::MEDIA_CLASS) != Some(VIDEO_SOURCE) {
        return None;
    }
    let pid = props.get_prop(*keys::APP_PROCESS_ID)?.parse().ok()?;
    let app_name = props.get_prop(*keys::APP_NAME).map(str::to_string);
    Some(VideoSourceApp { node_id, pid, app_name })
}

/// Applies one bound `Video/Source` node's `info` event -- mirrors [`apply_info_event`]'s
/// props-change gating exactly (the same PipeWire event-ordering quirks, not specific to audio).
fn apply_video_info_event(sources: &mut VideoSourceApps, node_id: u32, has_props_change: bool, props: Option<&impl PropsLookup>) {
    if !has_props_change {
        return;
    }
    match props.and_then(|props| parse_video_source_props(node_id, props)) {
        Some(source) => sources.upsert(source),
        None => {
            sources.remove(node_id);
        }
    }
}

/// Live `Video/Source` node list, keyed by PipeWire node id -- mirrors [`AudioApps`]'s shape
/// (duplicated rather than made generic over the two: this codebase keeps small near-identical
/// collections boring and separate rather than generic).
#[derive(Debug, Default)]
pub struct VideoSourceApps {
    sources: HashMap<u32, VideoSourceApp>,
}

impl VideoSourceApps {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert(&mut self, source: VideoSourceApp) {
        self.sources.insert(source.node_id, source);
    }

    /// Returns whether an entry was actually present and removed -- lets `global_remove` (which
    /// doesn't know in advance whether a removed node id was an audio stream or a video source)
    /// decide which of the two update channels to publish on, without tracking node kind
    /// separately from which collection holds it.
    pub fn remove(&mut self, node_id: u32) -> bool {
        self.sources.remove(&node_id).is_some()
    }

    pub fn snapshot(&self) -> Vec<VideoSourceApp> {
        let mut sources: Vec<VideoSourceApp> = self.sources.values().cloned().collect();
        sources.sort_by_key(|source| source.node_id);
        sources
    }
}

/// One § 3.2 audio write, crossing from the tokio runtime into the PipeWire loop.
///
/// A channel, not a method call: every proxy this thread holds is `!Send`, and the loop that
/// dispatches them is the thread's own blocking `main_loop.run()`. `pipewire::channel` hands the
/// loop an eventfd to poll beside its own sources, so a command applies between two PipeWire
/// events rather than racing them.
///
/// Ids are PipeWire registry ids, matching § 2.4's `sinks`/`sources`/`apps` arrays, resolved
/// against the live maps at apply time. An id whose node has gone away since the config read it
/// is logged and dropped: PipeWire recycles registry ids, so acting on a stale one is how a
/// volume change lands on somebody else's stream.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioCommand {
    SetMasterVolume(f32),
    SetMasterMuted(bool),
    ToggleMasterMute,
    SetDefaultSink(u32),
    SetDefaultSource(u32),
    SetAppVolume { id: u32, volume: f32 },
    SetAppMuted { id: u32, muted: bool },
}

/// Shared state the registry/node listener closures mutate: the running app/video-source lists,
/// the bound `Node` proxies (and their listeners) that keep property-change events flowing, and
/// where updated snapshots get sent. Held for the thread's whole lifetime -- see the `run` doc
/// comment for what a graceful shutdown would need that doesn't exist yet.
///
/// The `sink_*`/`metadata*`/`default_sink_name` fields are § 2.4's master-volume tracking
/// (docs/adr/0053 decision 3) -- see [`master`]. `sink_nodes` is separate from `nodes` (which
/// only holds stream/video-source proxies) because a sink's proxy outlives its own,
/// differently-shaped `param` listener rather than the `info` listener `nodes` entries carry.
struct MixerState {
    apps: AudioApps,
    video_sources: VideoSourceApps,
    nodes: HashMap<u32, (pw::node::Node, pw::node::NodeListener)>,
    updates: UnboundedSender<AudioState>,
    video_updates: UnboundedSender<Vec<VideoSourceApp>>,
    /// `Audio/Sink` node id -> everything tracked about that sink (see [`bind_sink`] for why
    /// `info` isn't needed here the way it is for stream nodes).
    sinks: HashMap<u32, SinkEntry>,
    /// Bound sink proxies and their `param` listeners, kept alive for the same reason `nodes`
    /// keeps stream/video-source proxies alive. Separate from `sinks` so that struct stays plain
    /// data a test can build.
    sink_nodes: HashMap<u32, (pw::node::Node, pw::node::NodeListener)>,
    /// `Audio/Source` node id -> its two names. No proxy and no listener beside them: § 2.4's
    /// source object needs nothing a `global` event doesn't already carry (see [`AUDIO_SOURCE`]).
    sources: HashMap<u32, DeviceNames>,
    /// Bound ALSA `Device` proxies and their `param` listeners. Bound only for the write path:
    /// a hardware sink's volume lives on its device's `Route` param, not on the node, so without
    /// these a `set_volume` is accepted and silently discarded (see [`write_master`]).
    devices: HashMap<u32, (pw::device::Device, pw::device::DeviceListener)>,
    /// `(device global id, card.profile.device)` -> the `Route` index currently active for it.
    /// A write needs the index, and only the device's own `Route` param reports it.
    device_routes: HashMap<(u32, i32), i32>,
    /// `Stream/Output/Audio` node id -> that stream's own raw `Props` values. The same type a
    /// sink uses, since it's the same pod shape parsed by the same function.
    app_props: HashMap<u32, master::RawSinkProps>,
    /// The `node.name` the `default` metadata object's `default.audio.sink` and
    /// `default.audio.source` keys currently name, or `None` before that property has ever
    /// arrived this run.
    default_sink_name: Option<String>,
    default_source_name: Option<String>,
    /// The bound `default` `Metadata` proxy/listener, and the registry id it was bound from (so
    /// `global_remove` can tell whether a removed id was this object -- `Metadata` exposes no id
    /// accessor). `None` until `on_global` finds the `metadata.name == "default"` global.
    metadata: Option<(pw::metadata::Metadata, pw::metadata::MetadataListener)>,
    metadata_id: Option<u32>,
}

impl MixerState {
    /// A dropped receiver means nothing is draining yet, or the process is mid-shutdown -- not
    /// a reason to stop tracking streams.
    fn publish_audio(&self) {
        let master = master::compute_master(
            self.default_sink_name.as_deref(),
            self.sinks.iter().map(|(&id, sink)| (id, sink.names.node_name.as_str())),
            |id| self.sinks.get(&id).and_then(|sink| sink.props.clone()),
        );
        // Applied here, not inside AudioApps: a stream's identity and volume arrive on two
        // different PipeWire events (info and param) with no ordering between them, and folding
        // volume into the entry at info time would overwrite a reading already landed.
        let apps = self
            .apps
            .snapshot()
            .into_iter()
            .map(|app| match self.app_props.get(&app.id).map(master::master_volume_from_props) {
                Some(measured) => AppStream { volume: measured.volume, muted: measured.muted, ..app },
                None => app,
            })
            .collect();
        let state = AudioState {
            volume: master.volume,
            muted: master.muted,
            sinks: device_list(self.sinks.iter().map(|(&id, sink)| (id, &sink.names)), self.default_sink_name.as_deref()),
            sources: device_list(self.sources.iter().map(|(&id, names)| (id, names)), self.default_source_name.as_deref()),
            apps,
        };
        let _ = self.updates.send(state);
    }

    fn publish_video(&self) {
        let _ = self.video_updates.send(self.video_sources.snapshot());
    }
}

/// Runs the PipeWire registry listener until the process exits, sending an updated
/// [`AudioState`] (per-app streams plus § 2.4 master volume/mute) over `updates` on every
/// relevant node-added/-properties-changed/-removed/param-changed event, and an updated snapshot
/// of [`VideoSourceApp`]s over `video_updates` on the equivalent video events (docs/adr/0034) --
/// one PipeWire connection, two independent capabilities' worth of data, each publishing only on
/// its own changes. Blocks the calling thread -- call from a dedicated `std::thread::spawn`,
/// never an async task: `pipewire-rs`'s event loop and the `Rc`-based listener state here are
/// single-threaded and non-`Send`.
///
/// `updates` only reaches a log line in `main()` for now, not Lua -- see docs/adr/0017 for why
/// and what unblocks it. `video_updates` feeds `privacy::PrivacyController`'s name-enrichment
/// (docs/adr/0034), not a log line.
///
/// ponytail: no shutdown path -- `main_loop.run()` returns only when the process exits. Phase
/// 7/8's reload orchestrator is what would give this a `main_loop.quit()` trigger to react to;
/// nothing calls for that yet.
///
/// Logs and returns if PipeWire can't be reached at all (no daemon running) rather than
/// panicking: audio/video-source tracking is one optional subsystem, not a reason to take the
/// whole supervisor down.
pub fn run(updates: UnboundedSender<AudioState>, video_updates: UnboundedSender<Vec<VideoSourceApp>>, commands: AudioCommandReceiver) {
    if let Err(err) = run_inner(updates, video_updates, commands) {
        eprintln!("pipewire registry listener stopped: {err}");
    }
}

/// The write half of § 3.2's audio actions. `main.rs` holds the [`AudioCommandSender`] and hands
/// the receiver to [`run`], which attaches it to the PipeWire loop -- see [`AudioCommand`] for
/// why this is a channel rather than a method on a controller.
pub type AudioCommandSender = pw::channel::Sender<AudioCommand>;
pub type AudioCommandReceiver = pw::channel::Receiver<AudioCommand>;

/// Built here rather than in `main.rs` so nothing outside this module needs to name
/// `pipewire::channel` at all.
pub fn command_channel() -> (AudioCommandSender, AudioCommandReceiver) {
    pw::channel::channel()
}

fn run_inner(updates: UnboundedSender<AudioState>, video_updates: UnboundedSender<Vec<VideoSourceApp>>, commands: AudioCommandReceiver) -> Result<(), pw::Error> {
    pw::init();

    let main_loop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&main_loop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    let state = Rc::new(RefCell::new(MixerState {
        apps: AudioApps::new(),
        video_sources: VideoSourceApps::new(),
        nodes: HashMap::new(),
        updates,
        video_updates,
        sinks: HashMap::new(),
        sink_nodes: HashMap::new(),
        sources: HashMap::new(),
        devices: HashMap::new(),
        device_routes: HashMap::new(),
        app_props: HashMap::new(),
        default_sink_name: None,
        default_source_name: None,
        metadata: None,
        metadata_id: None,
    }));

    // Weak, not a clone of registry itself: the listener this builds is a hook stored on
    // registry's own C object, so a strong RegistryRc captured here would keep itself alive
    // forever.
    let registry_weak = registry.downgrade();
    let state_for_global = Rc::clone(&state);
    let state_for_remove = Rc::clone(&state);

    let _registry_listener = registry
        .add_listener_local()
        .global(move |obj| {
            if let Some(registry) = registry_weak.upgrade() {
                on_global(&state_for_global, &registry, obj);
            }
        })
        .global_remove(move |id| {
            let mut state = state_for_remove.borrow_mut();
            state.nodes.remove(&id);
            state.sinks.remove(&id);
            state.sink_nodes.remove(&id);
            state.sources.remove(&id);
            state.app_props.remove(&id);
            if state.devices.remove(&id).is_some() {
                // A removed device takes every route index it published with it, keyed by its
                // own global id, so a later device reusing that id doesn't inherit route indices
                // for hardware that's no longer plugged in.
                state.device_routes.retain(|&(device_id, _), _| device_id != id);
            }
            if state.metadata_id == Some(id) {
                state.metadata = None;
                state.metadata_id = None;
                state.default_sink_name = None;
                state.default_source_name = None;
            }
            // Audio publishes unconditionally on every removal, matching this capability's
            // already-shipped behavior (ADR-0034 scopes this extension as privacy name-
            // enrichment, not a change to audio's publish cadence). Video is new code, so it
            // publishes only when `id` was actually a tracked video source (see
            // VideoSourceApps::remove). Sink/metadata cleanup runs unconditionally for the same
            // reason: publish_audio recomputes master state from whatever's left on every call,
            // so a stale sink entry would otherwise linger until some other event overwrote it.
            state.apps.remove(id);
            state.publish_audio();
            if state.video_sources.remove(id) {
                state.publish_video();
            }
        })
        .register();

    // Held for the loop's lifetime: dropping the AttachedReceiver detaches the eventfd source
    // and every later command is silently discarded.
    let state_for_command = Rc::clone(&state);
    let _attached_commands = commands.attach(main_loop.loop_(), move |command| apply_command(&state_for_command, command));

    main_loop.run();
    Ok(())
}

/// Handles one registry `global` event: routes `Node` globals to [`on_node_global`] (audio
/// stream, video source, or `Audio/Sink` -- see [`classify`] and [`bind_sink`]) and the `default`
/// `Metadata` global to [`bind_default_metadata`] (§ 2.4's default-sink routing). Anything else
/// (other object types, other `Metadata` objects like `settings`/`filters`/`route-settings`,
/// checked live via `pw-mon`) is ignored.
fn on_global(state: &Rc<RefCell<MixerState>>, registry: &pw::registry::RegistryRc, obj: &GlobalObject<&DictRef>) {
    match obj.type_ {
        ObjectType::Node => on_node_global(state, registry, obj),
        ObjectType::Device => bind_device(state, registry, obj),
        ObjectType::Metadata => bind_default_metadata(state, registry, obj),
        _ => {}
    }
}

/// `media.class` value an output audio device node carries (§ 2.4's master volume lives here,
/// not on any `Stream/Output/Audio` node). Verified against real `pw-dump` output -- see
/// [`master`]'s module doc comment.
const AUDIO_SINK: &str = "Audio/Sink";

/// PipeWire's own key for a `Metadata` object's name (`"settings"`, `"default"`,
/// `"route-settings"`, etc. -- a machine advertises several). Not in `pipewire-rs`'s `keys`
/// module, so spelled out directly; confirmed against live `pw-mon` output (`metadata.name =
/// "default"` on the global this file needs).
const METADATA_NAME: &str = "metadata.name";

/// The metadata key naming § 2.4's master output device. Confirmed live with `pw-metadata`'s own
/// dump (`key:'default.audio.sink' value:'{"name":"alsa_output...stereo"}'`).
const DEFAULT_AUDIO_SINK_KEY: &str = "default.audio.sink";

/// The metadata key naming § 2.4's default input device. Same JSON shape as
/// [`DEFAULT_AUDIO_SINK_KEY`], confirmed on the same live `pw-metadata` dump.
const DEFAULT_AUDIO_SOURCE_KEY: &str = "default.audio.source";

/// The `type_` a `default.audio.*` metadata value carries. Read straight off `pw-metadata`'s own
/// dump (`type:'Spa:String:JSON'`) rather than inferred from the value looking like JSON.
const METADATA_JSON_TYPE: &str = "Spa:String:JSON";

/// Which of the two default-device names a metadata `property` event is about. An enum rather
/// than a bool so the arm that reads the key and the arm that writes the field name the same
/// thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DefaultDevice {
    Sink,
    Source,
}

/// Handles one `Node` `global` event: classifies it by `media.class` (audio stream, video
/// source, `Audio/Sink`, or neither -- see [`classify`]). `Audio/Sink` is handled separately by
/// [`bind_sink`], not folded into [`classify`]'s [`NodeKind`]: a sink needs a `param` listener
/// (§ 2.4's master volume) instead of the `info` listener every [`NodeKind`] variant here uses.
/// For the stream/video-source path: binds the node so its `info` event -- gated to only the
/// calls that carry a props change, see the module doc comment -- extracts the pid and keeps
/// the matching list current. Doesn't require the full parse to succeed here:
/// `application.process.id` can still be missing at `global` time and shows up in a later `info`
/// call instead.
fn on_node_global(state: &Rc<RefCell<MixerState>>, registry: &pw::registry::RegistryRc, obj: &GlobalObject<&DictRef>) {
    if obj.props.and_then(|props| props.get_prop(*keys::MEDIA_CLASS)) == Some(AUDIO_SINK) {
        bind_sink(state, registry, obj);
        return;
    }
    if obj.props.and_then(|props| props.get_prop(*keys::MEDIA_CLASS)) == Some(AUDIO_SOURCE) {
        track_source(state, obj);
        return;
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
                    apply_info_event(&mut state_mut.apps, node_id, has_props_change, info.props());
                    state_mut.publish_audio();
                }
                NodeKind::Video => {
                    apply_video_info_event(&mut state_mut.video_sources, node_id, has_props_change, info.props());
                    state_mut.publish_video();
                }
            }
        })
        // § 2.4's per-app volume/muted: the same pod, parse, and cube root bind_sink already
        // runs for the master sink, on a node that happens to be a stream -- verified with
        // pw-cli enum-params <id> Props against a live playback stream, which carries
        // channelVolumes and mute exactly as a sink does.
        //
        // A video source has no Props param worth reading and never gets subscribed below, so
        // this callback simply never fires for one.
        .param(move |_seq, param_type, _index, _next, param| {
            if param_type != pw::spa::param::ParamType::Props {
                return;
            }
            let Some(pod) = param else { return };
            let Ok((_, value)) = PodDeserializer::deserialize_from::<Value>(pod.as_bytes()) else {
                return;
            };
            // Same channelVolumes-must-be-present filter the master sink needs: a node can
            // advertise more than one object under ParamType::Props, and reading a non-mixer one
            // as a mixer object reports a volume of zero. See extract_sink_props's own doc
            // comment for the live probe that found it.
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

/// Binds an `Audio/Sink` node (§ 2.4's master volume) and subscribes to its `Props` param, which
/// is where PipeWire keeps volume/mute -- not the `info` props dict `on_node_global`'s
/// stream/video-source path reads (see [`master`]). `node.name` is read once, straight from the
/// `global` event's already-known props: checked live, a sink's `node.name` is present from its
/// very first `global` event, unlike a stream node's `application.process.id` -- so, unlike
/// streams, no `info` listener is needed here at all, only `param`.
///
/// The `param` callback below relies on [`master::extract_sink_props`] returning `None` for a
/// `Props` event that isn't the mixer one -- a sink delivers two separate `Props` objects per
/// subscription/change (mixer keys, then unrelated ALSA device settings with no mixer keys at
/// all), and writing the second one's absence-of-data into `sink_props` as a zeroed volume was
/// a real bug this codebase shipped and fixed.
fn bind_sink(state: &Rc<RefCell<MixerState>>, registry: &pw::registry::RegistryRc, obj: &GlobalObject<&DictRef>) {
    let node_id = obj.id;
    let Some(props) = obj.props else { return };
    let Some(node_name) = props.get_prop(*keys::NODE_NAME) else { return };
    let names = DeviceNames { node_name: node_name.to_string(), description: device_display_name(props) };

    // Bound before anything is recorded, not after: a sink whose bind fails never gets a Props
    // subscription and so never gets a volume either. Recording its name unconditionally would
    // leave resolve_default_device free to resolve to a node whose props can never arrive, and
    // compute_master would then report MasterVolume::default()'s 0.0 forever -- a number that
    // looks like a real 0% on a capability with no "unknown" sentinel (docs/adr/0053).
    let node: pw::node::Node = match registry.bind(obj) {
        Ok(node) => node,
        Err(err) => {
            eprintln!("audio: failed to bind Audio/Sink node {node_id}: {err}");
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
            if let Some(sink) = state_mut.sinks.get_mut(&node_id) {
                sink.props = Some(raw);
            }
            state_mut.publish_audio();
        })
        // The route comes from info, not from the global event, and that is not a style choice.
        // Checked live: a sink's global props carry media.class and node.name but not device.id
        // or card.profile.device, both present only under the node's full info props. Reading
        // them at global time returns None, write_master then takes the node path, and the
        // write is accepted and silently discarded -- the same "global carries a subset" trap
        // the module doc comment records for a stream node's application.process.id.
        .info(move |info| {
            if !info.change_mask().contains(pw::node::NodeChangeMask::PROPS) {
                return;
            }
            let Some(route) = info.props().and_then(parse_sink_route) else { return };
            if let Some(sink) = state_for_info.borrow_mut().sinks.get_mut(&node_id) {
                sink.route = Some(route);
            }
        })
        .register();

    // Requested after registering the listener above, matching this file's existing bind-then-
    // listen-then-subscribe ordering elsewhere; nothing dispatches either call until
    // main_loop.run(), so the two orderings are behaviorally identical here.
    node.subscribe_params(&[pw::spa::param::ParamType::Props]);

    let mut state_mut = state.borrow_mut();
    state_mut.sinks.insert(node_id, SinkEntry { names, props: None, route: None });
    state_mut.sink_nodes.insert(node_id, (node, listener));
    state_mut.publish_audio();
}

/// The hardware device behind a sink, from the sink node's own `global` props. Both keys are
/// present together or not at all: this machine's analog output carries `device.id = 49` and
/// `card.profile.device = 7`, and a sink with no hardware behind it carries neither. `None`
/// means the sink's volume really does live on its node.
fn parse_sink_route(props: &impl PropsLookup) -> Option<SinkRoute> {
    let device_id = props.get_prop(*keys::DEVICE_ID)?.parse().ok()?;
    let profile_device = props.get_prop(CARD_PROFILE_DEVICE)?.parse().ok()?;
    Some(SinkRoute { device_id, profile_device })
}

/// Records an `Audio/Source` node's two names. No `registry.bind`, no proxy, no listener: § 2.4's
/// source object is `id`, `name` and `active`, and the `global` event already carries the first
/// two while the third comes from the `default.audio.source` metadata key. The asymmetry with
/// [`bind_sink`] is deliberate, not an oversight: a sink is bound because § 2.4 asks for the
/// master `volume`/`muted`, which only its `Props` param has. A source has no such field.
fn track_source(state: &Rc<RefCell<MixerState>>, obj: &GlobalObject<&DictRef>) {
    let Some(props) = obj.props else { return };
    let Some(node_name) = props.get_prop(*keys::NODE_NAME) else { return };
    let mut state_mut = state.borrow_mut();
    state_mut.sources.insert(obj.id, DeviceNames { node_name: node_name.to_string(), description: device_display_name(props) });
    state_mut.publish_audio();
}

/// Binds the one `Metadata` global whose `metadata.name` is `"default"` -- the object that
/// publishes `default.audio.sink` (§ 2.4's routing), among several other `default.*` keys this
/// capability doesn't need. A machine advertises multiple `Metadata` objects (`settings`,
/// `filters`, `route-settings`, confirmed live via `pw-mon`); every non-matching one is left
/// unbound.
fn bind_default_metadata(state: &Rc<RefCell<MixerState>>, registry: &pw::registry::RegistryRc, obj: &GlobalObject<&DictRef>) {
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
            // The default metadata carries several keys this capability does not read
            // (default.configured.audio.sink, default.video.source). default.configured.* is
            // not the same fact: on this machine it names a Bluetooth device that isn't
            // connected, while default.audio.sink names the analog output actually in use.
            let field = match key {
                Some(DEFAULT_AUDIO_SINK_KEY) => DefaultDevice::Sink,
                Some(DEFAULT_AUDIO_SOURCE_KEY) => DefaultDevice::Source,
                _ => return 0,
            };
            let mut state_mut = state_for_property.borrow_mut();
            // value: None means the property was cleared -- treated the same as "no default
            // known", which resolve_default_device already falls back from.
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

/// Applies one [`AudioCommand`] on the PipeWire loop's own thread. Every arm resolves its target
/// against the live maps first and logs rather than guessing when it cannot: a registry id a
/// config read moments ago can name a different node by the time the command lands, because
/// PipeWire recycles ids.
///
/// No optimistic state update anywhere here. A successful write comes back as a `param` or
/// `property` event on the same loop, which publishes the new value -- also what makes a volume
/// changed by `wpctl` or a headset button look identical to one this shell asked for.
fn apply_command(state: &Rc<RefCell<MixerState>>, command: AudioCommand) {
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

/// Binds an ALSA `Device` global and subscribes to its `Route` param, the only place a route's
/// active index is published. Bound purely for the write path: nothing in § 2.4 reads a device,
/// and without this a `set_volume` on a hardware sink would be accepted and silently dropped
/// (see [`write_master`]).
///
/// Only `device.api == "alsa"` devices are bound. This machine also advertises `v4l2` and
/// `libcamera` devices, which have no audio routes at all.
fn bind_device(state: &Rc<RefCell<MixerState>>, registry: &pw::registry::RegistryRc, obj: &GlobalObject<&DictRef>) {
    if obj.props.and_then(|props| props.get_prop(*keys::DEVICE_API)) != Some("alsa") {
        return;
    }
    let device_id = obj.id;
    let device: pw::device::Device = match registry.bind(obj) {
        Ok(device) => device,
        Err(err) => {
            eprintln!("audio: failed to bind ALSA device {device_id}: {err}");
            return;
        }
    };

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
        .register();

    device.subscribe_params(&[pw::spa::param::ParamType::Route]);
    state.borrow_mut().devices.insert(device_id, (device, listener));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Video/Source` node's properties, shaped after real `pw-dump` output for a portal-
    /// routed camera stream.
    fn camera_stream_props() -> HashMap<String, String> {
        HashMap::from([
            ("media.class".to_string(), "Video/Source".to_string()),
            ("application.name".to_string(), "Firefox".to_string()),
            ("application.process.id".to_string(), "42".to_string()),
            ("node.name".to_string(), "Firefox".to_string()),
        ])
    }

    // ---- classify ----

    #[test]
    fn classify_recognizes_an_audio_stream() {
        assert_eq!(classify(&zen_browser_stream_props()), Some(NodeKind::Audio));
    }

    #[test]
    fn classify_recognizes_a_video_source() {
        assert_eq!(classify(&camera_stream_props()), Some(NodeKind::Video));
    }

    #[test]
    fn classify_is_none_for_an_unrelated_media_class() {
        let props = HashMap::from([("media.class".to_string(), "Audio/Sink".to_string())]);
        assert_eq!(classify(&props), None);
    }

    // ---- parse_video_source_props ----

    #[test]
    fn parse_video_source_props_matches_a_real_video_source_node() {
        let source = parse_video_source_props(7, &camera_stream_props()).expect("should parse as a video source");
        assert_eq!(source, VideoSourceApp { node_id: 7, pid: 42, app_name: Some("Firefox".to_string()) });
    }

    #[test]
    fn parse_video_source_props_rejects_a_non_video_source_media_class() {
        assert!(parse_video_source_props(7, &zen_browser_stream_props()).is_none());
    }

    #[test]
    fn parse_video_source_props_rejects_a_stream_missing_the_pid() {
        let props = HashMap::from([("media.class".to_string(), "Video/Source".to_string())]);
        assert!(parse_video_source_props(7, &props).is_none());
    }

    // ---- VideoSourceApps ----

    #[test]
    fn video_source_apps_upsert_then_snapshot_returns_the_source() {
        let mut sources = VideoSourceApps::new();
        let source = VideoSourceApp { node_id: 1, pid: 42, app_name: Some("Firefox".to_string()) };
        sources.upsert(source.clone());
        assert_eq!(sources.snapshot(), vec![source]);
    }

    #[test]
    fn video_source_apps_remove_reports_whether_an_entry_was_present() {
        let mut sources = VideoSourceApps::new();
        sources.upsert(VideoSourceApp { node_id: 1, pid: 42, app_name: None });
        assert!(sources.remove(1));
        assert!(!sources.remove(1), "already removed -- nothing left to remove");
    }

    // ---- apply_video_info_event ----

    #[test]
    fn apply_video_info_event_ignores_a_state_only_event() {
        let mut sources = VideoSourceApps::new();
        apply_video_info_event(&mut sources, 1, true, Some(&camera_stream_props()));
        let empty: HashMap<String, String> = HashMap::new();
        apply_video_info_event(&mut sources, 1, false, Some(&empty));
        assert_eq!(sources.snapshot().len(), 1, "a non-PROPS info event must not drop an already-tracked source");
    }

    #[test]
    fn apply_video_info_event_removes_when_a_props_bearing_event_no_longer_parses() {
        let mut sources = VideoSourceApps::new();
        apply_video_info_event(&mut sources, 1, true, Some(&camera_stream_props()));
        assert_eq!(sources.snapshot().len(), 1);

        let non_video_props = HashMap::from([("media.class".to_string(), "Audio/Sink".to_string())]);
        apply_video_info_event(&mut sources, 1, true, Some(&non_video_props));

        assert!(sources.snapshot().is_empty());
    }

    /// A real `Stream/Output/Audio` node's properties, recorded via `pw-dump` from a Zen
    /// browser playback stream routed through `pipewire-pulse` on a live system.
    fn zen_browser_stream_props() -> HashMap<String, String> {
        HashMap::from([
            ("media.class".to_string(), "Stream/Output/Audio".to_string()),
            ("application.name".to_string(), "Zen".to_string()),
            ("application.process.id".to_string(), "1538319".to_string()),
            ("application.process.binary".to_string(), "zen-bin".to_string()),
            ("client.id".to_string(), "97".to_string()),
            ("node.name".to_string(), "Zen".to_string()),
        ])
    }

    #[test]
    fn parse_stream_props_matches_a_real_stream_output_audio_node() {
        let parsed = parse_stream_props(&zen_browser_stream_props()).expect("should parse as an audio stream");
        assert_eq!(parsed.pid, 1538319);
        assert_eq!(parsed.app_name, Some("Zen".to_string()));
    }

    #[test]
    fn parse_stream_props_rejects_non_stream_media_class() {
        let props = HashMap::from([
            ("media.class".to_string(), "Audio/Sink".to_string()),
            ("application.process.id".to_string(), "1234".to_string()),
        ]);
        assert!(parse_stream_props(&props).is_none());
    }

    #[test]
    fn parse_stream_props_rejects_a_stream_missing_the_pid() {
        let props = HashMap::from([("media.class".to_string(), "Stream/Output/Audio".to_string())]);
        assert!(parse_stream_props(&props).is_none());
    }

    #[test]
    fn parse_stream_props_rejects_an_unparseable_pid() {
        let props = HashMap::from([
            ("media.class".to_string(), "Stream/Output/Audio".to_string()),
            ("application.process.id".to_string(), "not-a-pid".to_string()),
        ]);
        assert!(parse_stream_props(&props).is_none());
    }

    #[test]
    fn parse_stream_props_allows_a_missing_app_name() {
        let props = HashMap::from([
            ("media.class".to_string(), "Stream/Output/Audio".to_string()),
            ("application.process.id".to_string(), "1234".to_string()),
        ]);
        let parsed = parse_stream_props(&props).expect("pid alone is enough to parse");
        assert_eq!(parsed.app_name, None);
    }

    #[test]
    fn resolve_process_name_reads_proc_comm_for_a_real_process() {
        // Uses this test process's own pid rather than spawning a child: a fresh child's pid
        // raced, under this sandbox's parallel test threads, with transiently aliasing another
        // thread in this same binary. The test doesn't need a spawned child either way.
        let pid = std::process::id() as i32;
        let name = resolve_process_name(pid).expect("this process's own /proc entry must be readable");
        assert!(!name.is_empty());
        assert!(!name.ends_with('\n'), "trim_end should have stripped comm's trailing newline");
    }

    #[test]
    fn resolve_process_name_returns_none_for_a_pid_that_does_not_exist() {
        assert_eq!(resolve_process_name(i32::MAX), None);
    }

    #[test]
    fn build_app_stream_combines_parsing_and_pid_resolution() {
        let pid = std::process::id();
        let expected_process_name = resolve_process_name(pid as i32);

        let props = HashMap::from([
            ("media.class".to_string(), "Stream/Output/Audio".to_string()),
            ("application.process.id".to_string(), pid.to_string()),
            ("application.name".to_string(), "Test App".to_string()),
        ]);

        let app = build_app_stream(42, &props).expect("should build an AppStream");
        assert_eq!(app.id, 42);
        assert_eq!(app.pid, pid as i32);
        assert_eq!(app.name, Some("Test App".to_string()));
        assert_eq!(app.process_name, expected_process_name);
        // PipeWire's own untouched values: build_app_stream reads the node's property dict, and
        // volume lives on a param instead. publish_audio is where the two meet.
        assert_eq!(app.volume, 1.0);
        assert!(!app.muted);
    }

    #[test]
    fn build_app_stream_rejects_a_non_audio_node() {
        let props = HashMap::from([("media.class".to_string(), "Video/Source".to_string())]);
        assert!(build_app_stream(1, &props).is_none());
    }

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

    #[test]
    fn audio_apps_upsert_then_snapshot_returns_the_stream() {
        let mut apps = AudioApps::new();
        apps.upsert(sample_stream(1));
        assert_eq!(apps.snapshot(), vec![sample_stream(1)]);
    }

    #[test]
    fn audio_apps_upsert_replaces_the_existing_entry_for_the_same_node_id() {
        let mut apps = AudioApps::new();
        apps.upsert(sample_stream(1));
        let renamed = AppStream { name: Some("renamed".to_string()), ..sample_stream(1) };
        apps.upsert(renamed.clone());
        assert_eq!(apps.snapshot(), vec![renamed]);
    }

    #[test]
    fn audio_apps_remove_drops_the_entry() {
        let mut apps = AudioApps::new();
        apps.upsert(sample_stream(1));
        apps.remove(1);
        assert!(apps.snapshot().is_empty());
    }

    #[test]
    fn apply_info_event_keeps_a_tracked_stream_through_a_state_only_info_event() {
        let mut apps = AudioApps::new();
        // First info event: PipeWire's PROPS-bearing bind-time event, full props -- guaranteed
        // by upstream global_bind, see the module doc comment.
        apply_info_event(&mut apps, 1, true, Some(&zen_browser_stream_props()));
        assert_eq!(apps.snapshot().len(), 1, "the initial props-bearing info event should track the stream");

        // Second info event: a state-only transition (e.g. RUNNING -> IDLE), which sends an
        // empty props dict -- not evidence the node stopped being an audio stream.
        let state_only_props: HashMap<String, String> = HashMap::new();
        apply_info_event(&mut apps, 1, false, Some(&state_only_props));

        assert_eq!(
            apps.snapshot().len(),
            1,
            "a state-only info event (no PROPS change) must not drop an already-tracked stream"
        );
    }

    #[test]
    fn apply_info_event_upserts_on_a_props_bearing_event() {
        let mut apps = AudioApps::new();
        apply_info_event(&mut apps, 1, true, Some(&zen_browser_stream_props()));
        assert_eq!(apps.snapshot(), vec![build_app_stream(1, &zen_browser_stream_props()).unwrap()]);
    }

    #[test]
    fn apply_info_event_removes_when_a_props_bearing_event_no_longer_parses() {
        let mut apps = AudioApps::new();
        apply_info_event(&mut apps, 1, true, Some(&zen_browser_stream_props()));
        assert_eq!(apps.snapshot().len(), 1);

        let non_stream_props = HashMap::from([("media.class".to_string(), "Audio/Sink".to_string())]);
        apply_info_event(&mut apps, 1, true, Some(&non_stream_props));

        assert!(apps.snapshot().is_empty(), "a PROPS-bearing event that no longer parses as a stream should remove it");
    }

    #[test]
    fn audio_apps_snapshot_is_sorted_by_node_id() {
        let mut apps = AudioApps::new();
        apps.upsert(sample_stream(3));
        apps.upsert(sample_stream(1));
        apps.upsert(sample_stream(2));
        let ids: Vec<u32> = apps.snapshot().iter().map(|app| app.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn app_stream_serializes_with_the_spec_field_spelling() {
        // docs/adr/0053 decision 3: node_id -> id, app_name -> name; pid/process_name kept
        // (docs/adr/0016).
        let stream = AppStream { id: 7, pid: 999, name: Some("Zen".to_string()), process_name: None, volume: 1.0, muted: false };
        let json = serde_json::to_value(&stream).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "id": 7,
                "pid": 999,
                "name": "Zen",
                "process_name": null,
                "volume": 1.0,
                "muted": false,
            })
        );
    }

    // ---- device_display_name / device_list ----

    /// The two names a live `pw-dump` shows on this machine's analog output, verbatim: one
    /// routes, the other is read by a person.
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

    /// One tracked sink, from the three things a test ever cares to vary about it.
    fn sink_at(node_name: &str, description: Option<&str>, props: Option<master::RawSinkProps>) -> SinkEntry {
        SinkEntry { names: DeviceNames { node_name: node_name.to_string(), description: description.map(str::to_string) }, props, route: None }
    }

    /// `id -> DeviceNames` the way `MixerState` holds it, from pairs a test can read at a glance.
    fn tracked(entries: &[(u32, &str, Option<&str>)]) -> HashMap<u32, DeviceNames> {
        entries
            .iter()
            .map(|(id, node_name, description)| (*id, DeviceNames { node_name: node_name.to_string(), description: description.map(str::to_string) }))
            .collect()
    }

    #[test]
    fn device_list_marks_the_metadata_named_device_active_and_orders_by_id() {
        let names = tracked(&[(70, "bluez_output.headset", Some("WH-1000XM4")), (59, "alsa_output.analog", Some("Built-in Audio Analog Stereo"))]);

        let devices = device_list(names.iter().map(|(&id, names)| (id, names)), Some("bluez_output.headset"));

        assert_eq!(
            devices,
            vec![
                AudioDevice { id: 59, name: "Built-in Audio Analog Stereo".to_string(), active: false },
                AudioDevice { id: 70, name: "WH-1000XM4".to_string(), active: true },
            ]
        );
    }

    #[test]
    fn device_list_falls_back_to_the_node_name_when_no_description_was_seen() {
        let names = tracked(&[(59, "alsa_output.analog", None)]);

        let devices = device_list(names.iter().map(|(&id, names)| (id, names)), None);

        assert_eq!(devices, vec![AudioDevice { id: 59, name: "alsa_output.analog".to_string(), active: true }]);
    }

    #[test]
    fn device_list_is_empty_with_nothing_tracked() {
        assert_eq!(device_list(std::iter::empty(), Some("anything")), Vec::new());
    }

    // ---- AudioState ----

    #[test]
    fn audio_state_serializes_as_the_flat_shape_docs_adr_0053_specifies() {
        // 0.5, not e.g. 0.3: an f32 not exactly representable in f64 serializes with long
        // trailing digits once serde_json widens it, making this check about float precision
        // instead of field shape.
        let stream = sample_stream(1);
        let state = AudioState {
            volume: 0.5,
            muted: false,
            sinks: vec![AudioDevice { id: 59, name: "Built-in Audio Analog Stereo".to_string(), active: true }],
            sources: vec![AudioDevice { id: 60, name: "Built-in Audio Analog Stereo".to_string(), active: true }],
            apps: vec![stream.clone()],
        };
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "volume": 0.5,
                "muted": false,
                "sinks": [{ "id": 59, "name": "Built-in Audio Analog Stereo", "active": true }],
                "sources": [{ "id": 60, "name": "Built-in Audio Analog Stereo", "active": true }],
                "apps": [{
                    "id": stream.id,
                    "pid": stream.pid,
                    "name": stream.name,
                    "process_name": stream.process_name,
                    "volume": stream.volume,
                    "muted": stream.muted,
                }],
            })
        );
    }

    // ---- MixerState::publish_audio (the wiring, exercised through the published snapshot) ----

    /// One device's raw `Props` at a given linear volume, so a test says the number it means
    /// rather than its cube.
    fn props_at(linear: f32, muted: bool) -> master::RawSinkProps {
        master::RawSinkProps { mute: muted, channel_volumes: master::cubed_channel_volumes(linear, 2).expect("two channels is not zero") }
    }

    /// Everything `publish_audio` reads, with every PipeWire proxy map left empty. A helper
    /// rather than repeated per test: only a handful of fields is ever the subject of a given
    /// test, and the rest is noise.
    fn mixer_state(updates: UnboundedSender<AudioState>, video_updates: UnboundedSender<Vec<VideoSourceApp>>) -> MixerState {
        MixerState {
            apps: AudioApps::new(),
            video_sources: VideoSourceApps::new(),
            nodes: HashMap::new(),
            updates,
            video_updates,
            sinks: HashMap::new(),
            sink_nodes: HashMap::new(),
            sources: HashMap::new(),
            devices: HashMap::new(),
            device_routes: HashMap::new(),
            app_props: HashMap::new(),
            default_sink_name: None,
            default_source_name: None,
            metadata: None,
            metadata_id: None,
        }
    }

    #[test]
    fn publish_audio_combines_master_volume_and_app_snapshot() {
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (video_updates, _video_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = mixer_state(updates, video_updates);
        state.sinks = HashMap::from([(59, sink_at("alsa_output.pci-...analog-stereo", None, Some(props_at(0.3, false))))]);
        state.default_sink_name = Some("alsa_output.pci-...analog-stereo".to_string());
        state.apps.upsert(sample_stream(1));

        state.publish_audio();

        let published = rx.try_recv().expect("publish_audio should have sent a snapshot");
        assert!((published.volume - 0.3).abs() < 1e-6, "expected ~0.3, got {}", published.volume);
        assert!(!published.muted);
        assert_eq!(published.apps, vec![sample_stream(1)]);
    }

    #[test]
    fn publish_audio_reports_the_master_volume_default_with_no_sink_tracked() {
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (video_updates, _video_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = mixer_state(updates, video_updates);

        state.publish_audio();

        let published = rx.try_recv().expect("publish_audio should have sent a snapshot");
        assert_eq!(published.volume, master::MasterVolume::default().volume);
        assert_eq!(published.muted, master::MasterVolume::default().muted);
        assert!(published.apps.is_empty());
        assert!(published.sinks.is_empty());
        assert!(published.sources.is_empty());
    }

    #[test]
    fn publish_audio_overlays_a_streams_own_props_reading_onto_its_entry() {
        // A stream's identity comes from an info event and its volume from a param event; this
        // is the one place the two are joined.
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (video_updates, _video_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = mixer_state(updates, video_updates);
        state.apps.upsert(sample_stream(1));
        state.app_props = HashMap::from([(1, props_at(0.42, true))]);

        state.publish_audio();

        let published = rx.try_recv().expect("publish_audio should have sent a snapshot");
        assert!((published.apps[0].volume - 0.42).abs() < 1e-6, "expected ~0.42, got {}", published.apps[0].volume);
        assert!(published.apps[0].muted);
    }

    #[test]
    fn publish_audio_leaves_a_stream_whose_props_have_not_arrived_at_pipewires_own_untouched_values() {
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (video_updates, _video_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = mixer_state(updates, video_updates);
        state.apps.upsert(sample_stream(1));
        state.app_props = HashMap::from([(99, props_at(0.42, true))]);

        state.publish_audio();

        let published = rx.try_recv().expect("publish_audio should have sent a snapshot");
        assert_eq!(published.apps[0].volume, 1.0, "another stream's reading must not leak onto this one");
        assert!(!published.apps[0].muted);
    }

    #[test]
    fn publish_audio_reports_sinks_and_sources_with_their_own_active_flags() {
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (video_updates, _video_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = mixer_state(updates, video_updates);
        state.sinks = HashMap::from([
            (59, sink_at("alsa_output.analog", Some("Built-in Audio Analog Stereo"), None)),
            (70, sink_at("bluez_output.headset", Some("WH-1000XM4"), None)),
        ]);
        state.default_sink_name = Some("bluez_output.headset".to_string());
        state.sources = tracked(&[(60, "alsa_input.analog", Some("Built-in Microphone"))]);
        state.default_source_name = Some("alsa_input.analog".to_string());

        state.publish_audio();

        let published = rx.try_recv().expect("publish_audio should have sent a snapshot");
        assert_eq!(published.sinks.iter().map(|sink| sink.active).collect::<Vec<_>>(), [false, true]);
        assert_eq!(published.sinks[1].name, "WH-1000XM4");
        assert_eq!(published.sources, vec![AudioDevice { id: 60, name: "Built-in Microphone".to_string(), active: true }]);
    }
}
