//! Tracked `obelisk.audio` state and pure parsing helpers, testable against recorded `pw-dump`
//! properties without a live PipeWire proxy.

use std::collections::HashMap;
use std::path::Path;

use pipewire as pw;
use pw::keys;
use pw::spa::utils::dict::DictRef;
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::capabilities::audio::master;

/// `media.class` for playback streams, verified in `pw-dump`.
const STREAM_OUTPUT_AUDIO: &str = "Stream/Output/Audio";

/// `media.class` for camera capture (ADR-0034). PipeWire sees only portal-routed cameras, so this
/// supplements `obelisk.privacy`'s kernel detection with a name-enrichment source.
const VIDEO_SOURCE: &str = "Video/Source";

/// `media.class` for app audio capture (ADR-0137), distinct from `source_volume`, a device
/// setting that says nothing about whether anything is listening.
const STREAM_INPUT_AUDIO: &str = "Stream/Input/Audio";

/// `media.class` for screen-capture producers (ADR-0137). A camera is a `Video/Source` device;
/// video streams identify screen capture without portal or compositor names.
///
/// ponytail: `wf-recorder`, `grim`, and other wlr-screencopy clients never reach PipeWire. Catching
/// them needs compositor-reported screencopy clients, which niri-ipc does not provide.
const STREAM_OUTPUT_VIDEO: &str = "Stream/Output/Video";

/// PipeWire's marker for a capture stream reading a sink monitor (`PW_KEY_STREAM_CAPTURE_SINK`).
/// cava and every other visualiser sets it; filtering the property avoids lighting a microphone
/// indicator for a spectrum analyser (the mirror only excludes cava by name).
const STREAM_CAPTURE_SINK: &str = "stream.capture.sink";

/// A `Stream/Output/Audio` node resolved to its owning process. `main.rs` publishes it unchanged;
/// § 2.4 names `id`/`name` (ADR-0053 decision 3), while ADR-0016's `pid`/`process_name` remain.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct AppStream {
    /// PipeWire registry id, the [`AudioApps`] key.
    pub id: u32,
    /// `application.process.id` recorded for the owning process.
    pub pid: i32,
    /// `application.name`, if the client set one.
    pub name: Option<String>,
    /// `/proc/{pid}/comm`, if the process still existed when observed.
    pub process_name: Option<String>,
    /// § 2.4 per-app volume, range `[0.0, 1.0]`, cube-rooted from `SPA_PARAM_Props` like a master
    /// sink (`pw-cli enum-params <id> Props` confirms cubed `channelVolumes`). `1.0` before it.
    pub volume: f32,
    /// § 2.4 per-app mute, from the same `Props` as `volume`.
    pub muted: bool,
}

/// String lookup shared by live PipeWire dicts and recorded `pw-dump` maps in tests.
pub(super) trait PropsLookup {
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

/// Parsed stream `media.class`/pid/name, before process-name resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedStream {
    pid: i32,
    app_name: Option<String>,
}

/// Node kind chosen at `global` time and carried into `info`; state-only `info` props can be empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NodeKind {
    Audio,
    Video,
    Microphone,
    Screencast,
}

/// Classifies a `global` event by `media.class`; `None` covers sinks, sources, and other nodes.
pub(super) fn classify(props: &impl PropsLookup) -> Option<NodeKind> {
    match props.get_prop(*keys::MEDIA_CLASS) {
        Some(STREAM_OUTPUT_AUDIO) => Some(NodeKind::Audio),
        Some(VIDEO_SOURCE) => Some(NodeKind::Video),
        Some(STREAM_INPUT_AUDIO) => Some(NodeKind::Microphone),
        Some(STREAM_OUTPUT_VIDEO) => Some(NodeKind::Screencast),
        _ => None,
    }
}

/// Parses a `Stream/Output/Audio` with a valid `application.process.id`; `None` also covers a
/// node PipeWire has not finished populating (see [`apply_info_event`]).
fn parse_stream_props(props: &impl PropsLookup) -> Option<ParsedStream> {
    if classify(props) != Some(NodeKind::Audio) {
        return None;
    }
    let pid = props.get_prop(*keys::APP_PROCESS_ID)?.parse().ok()?;
    let app_name = props.get_prop(*keys::APP_NAME).map(str::to_string);
    Some(ParsedStream { pid, app_name })
}

/// Reads `{proc_root}/{pid}/comm`; `None` if the process exited or procfs is unreadable.
///
/// Takes the root rather than hard-coding `/proc`, which is the repository rule for every procfs
/// reader in `capabilities/` -- it is what lets the tests below point at a tempdir. Shares
/// `privacy::video::read_comm`, which was already built that way.
fn resolve_process_name(proc_root: &Path, pid: i32) -> Option<String> {
    crate::capabilities::privacy::video::read_comm(proc_root, pid.try_into().ok()?)
}

/// Parses `props` and resolves the owning process name for both registry and `info` handlers.
fn build_app_stream(proc_root: &Path, node_id: u32, props: &impl PropsLookup) -> Option<AppStream> {
    let parsed = parse_stream_props(props)?;
    let process_name = resolve_process_name(proc_root, parsed.pid);
    Some(AppStream {
        id: node_id,
        pid: parsed.pid,
        name: parsed.app_name,
        process_name,
        // PipeWire reports this for an untouched stream (live `pw-cli enum-params <id> Props`);
        // replace it when the node's Props arrives. Unlike an unresolved master, 0.0 is not the
        // correct startup value because silence is a real stream state.
        volume: 1.0,
        muted: false,
    })
}

/// Applies a bound node's `info`. Gate upsert/remove on `NodeChangeMask::PROPS`: state-only events
/// carry empty props and must not drop a live stream. `global_bind`'s first `info` has PROPS.
pub(super) fn apply_info_event(
    proc_root: &Path,
    apps: &mut AudioApps,
    node_id: u32,
    has_props_change: bool,
    props: Option<&impl PropsLookup>,
) {
    if !has_props_change {
        return;
    }
    match props.and_then(|props| build_app_stream(proc_root, node_id, props)) {
        Some(app) => apps.upsert(app),
        None => {
            apps.remove(node_id);
        }
    }
}

/// Live per-app streams keyed by PipeWire node id.
#[derive(Debug, Default)]
pub struct AudioApps {
    streams: HashMap<u32, AppStream>,
}

impl AudioApps {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces a stream entry.
    pub fn upsert(&mut self, stream: AppStream) {
        self.streams.insert(stream.id, stream);
    }

    /// Removes a registry node or an entry whose properties no longer parse.
    pub fn remove(&mut self, node_id: u32) {
        self.streams.remove(&node_id);
    }

    /// Current streams, sorted by node id for deterministic snapshots.
    pub fn snapshot(&self) -> Vec<AppStream> {
        let mut apps: Vec<AppStream> = self.streams.values().cloned().collect();
        apps.sort_by_key(|app| app.id);
        apps
    }
}

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

/// `Video/Source` data for `obelisk.privacy` name enrichment (ADR-0034): `pid` matches a
/// kernel-detected `/dev/videoN` opener and `app_name` supplies its nicer PipeWire name. No
/// `process_name`: privacy already falls back to `/proc/{pid}/comm`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct VideoSourceApp {
    pub node_id: u32,
    pub pid: i32,
    pub app_name: Option<String>,
}

/// Parses a `Video/Source` with valid `application.process.id` into [`VideoSourceApp`].
fn parse_video_source_props(node_id: u32, props: &impl PropsLookup) -> Option<VideoSourceApp> {
    if classify(props) != Some(NodeKind::Video) {
        return None;
    }
    let pid = props.get_prop(*keys::APP_PROCESS_ID)?.parse().ok()?;
    let app_name = props.get_prop(*keys::APP_NAME).map(str::to_string);
    Some(VideoSourceApp { node_id, pid, app_name })
}

/// Applies a bound `Video/Source` `info`, with the same PROPS gating as [`apply_info_event`].
pub(super) fn apply_video_info_event(
    sources: &mut VideoSourceApps,
    node_id: u32,
    has_props_change: bool,
    props: Option<&impl PropsLookup>,
) {
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

/// Live `Video/Source` nodes keyed by PipeWire id. Kept separate from [`AudioApps`] deliberately.
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

    /// Removes an entry and reports whether it was present, letting `global_remove` choose the
    /// update channel without tracking node kind separately.
    pub fn remove(&mut self, node_id: u32) -> bool {
        self.sources.remove(&node_id).is_some()
    }

    pub fn snapshot(&self) -> Vec<VideoSourceApp> {
        let mut sources: Vec<VideoSourceApp> = self.sources.values().cloned().collect();
        sources.sort_by_key(|source| source.node_id);
        sources
    }
}

/// One microphone or screen-capture stream (ADR-0137); [`CaptureApps`] supplies the kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct CaptureApp {
    /// PipeWire registry id, the [`CaptureApps`] key.
    pub node_id: u32,
    /// `application.process.id`, which portal-created streams may omit because the owner is the
    /// portal. A naming hint, never an identity.
    pub pid: Option<i32>,
    /// `application.name`, if the client set one.
    pub app_name: Option<String>,
    /// Whether PipeWire reports `Running`. `Idle`/`Suspended` means the app holds the device open
    /// without reading, as a browser tab does between calls, and is not capture.
    pub running: bool,
}

/// Parses `props` for `kind`. Unlike [`parse_video_source_props`], `pid` is optional: camera users
/// cross-reference `/dev/videoN` openers by pid, but these lists have only PipeWire, so dropping a
/// pid-less node would drop the capture.
fn parse_capture_props(node_id: u32, kind: NodeKind, props: &impl PropsLookup) -> Option<CaptureApp> {
    if classify(props) != Some(kind) {
        return None;
    }
    if kind == NodeKind::Microphone && props.get_prop(STREAM_CAPTURE_SINK) == Some("true") {
        return None;
    }
    Some(CaptureApp {
        node_id,
        pid: props.get_prop(*keys::APP_PROCESS_ID).and_then(|pid| pid.parse().ok()),
        app_name: props.get_prop(*keys::APP_NAME).map(str::to_string),
        // Set by the caller from this `info` event.
        running: false,
    })
}

/// Applies one capture `info`. Props are gated on PROPS because other events carry a non-null
/// empty dict; `pw_node_info::state` is complete on every event, so `running` is not gated. The
/// STATE bit only says whether it moved since the last one.
pub(super) fn apply_capture_info_event(
    apps: &mut CaptureApps,
    node_id: u32,
    kind: NodeKind,
    has_props_change: bool,
    props: Option<&impl PropsLookup>,
    running: bool,
) {
    if has_props_change {
        match props.and_then(|props| parse_capture_props(node_id, kind, props)) {
            Some(app) => apps.upsert(app),
            None => {
                apps.remove(node_id);
            }
        }
    }
    apps.set_running(node_id, running);
}

/// Live capture streams for one kind, keyed by PipeWire id; kept separate from
/// [`VideoSourceApps`] deliberately.
#[derive(Debug, Default)]
pub struct CaptureApps {
    apps: HashMap<u32, CaptureApp>,
}

impl CaptureApps {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert(&mut self, app: CaptureApp) {
        self.apps.insert(app.node_id, app);
    }

    /// Ignores unknown nodes; a state event must not resurrect a rejected monitor capture.
    pub fn set_running(&mut self, node_id: u32, running: bool) {
        if let Some(app) = self.apps.get_mut(&node_id) {
            app.running = running;
        }
    }

    /// Removes an entry and reports whether it was present, like [`VideoSourceApps::remove`].
    pub fn remove(&mut self, node_id: u32) -> bool {
        self.apps.remove(&node_id).is_some()
    }

    /// Running entries only. Idle streams stay tracked so a later `Running` needs no re-parse.
    pub fn snapshot(&self) -> Vec<CaptureApp> {
        let mut apps: Vec<CaptureApp> = self.apps.values().filter(|app| app.running).cloned().collect();
        apps.sort_by_key(|app| app.node_id);
        apps
    }
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
    pub(super) apps: AudioApps,
    pub(super) video_sources: VideoSourceApps,
    /// Running `Stream/Input/Audio` nodes (ADR-0137).
    pub(super) microphones: CaptureApps,
    /// Running `Stream/Output/Video` nodes (ADR-0137).
    pub(super) screencasts: CaptureApps,
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
    pub(super) devices: HashMap<u32, (pw::device::Device, pw::device::DeviceListener)>,
    /// `(device global id, card.profile.device)` -> active `Route` index from the device.
    pub(super) device_routes: HashMap<(u32, i32), i32>,
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
            cameras: self.video_sources.snapshot(),
            microphones: self.microphones.snapshot(),
            screencasts: self.screencasts.snapshot(),
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

    fn camera_stream_props() -> HashMap<String, String> {
        HashMap::from([
            ("media.class".to_string(), "Video/Source".to_string()),
            ("application.name".to_string(), "Firefox".to_string()),
            ("application.process.id".to_string(), "42".to_string()),
            ("node.name".to_string(), "Firefox".to_string()),
        ])
    }

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

    fn capture_props(media_class: &str) -> HashMap<String, String> {
        HashMap::from([
            ("media.class".to_string(), media_class.to_string()),
            ("application.name".to_string(), "Firefox".to_string()),
            ("application.process.id".to_string(), "42".to_string()),
        ])
    }

    #[test]
    fn classify_tells_the_four_node_kinds_apart_by_media_class() {
        for (media_class, expected) in [
            ("Stream/Output/Audio", NodeKind::Audio),
            ("Video/Source", NodeKind::Video),
            ("Stream/Input/Audio", NodeKind::Microphone),
            ("Stream/Output/Video", NodeKind::Screencast),
        ] {
            let props = HashMap::from([("media.class".to_string(), media_class.to_string())]);
            assert_eq!(classify(&props), Some(expected), "{media_class}");
        }
        let props = HashMap::from([("media.class".to_string(), "Audio/Sink".to_string())]);
        assert_eq!(classify(&props), None, "a sink is bound by the device path, not this one");
    }

    /// A browser tab holds capture open between calls; publishing idle would light privacy forever.
    #[test]
    fn an_idle_capture_stream_is_tracked_but_not_published() {
        let mut apps = CaptureApps::new();
        apply_capture_info_event(
            &mut apps,
            1,
            NodeKind::Microphone,
            true,
            Some(&capture_props("Stream/Input/Audio")),
            false,
        );
        assert!(apps.snapshot().is_empty(), "an idle stream is not capture");

        let empty: HashMap<String, String> = HashMap::new();
        apply_capture_info_event(&mut apps, 1, NodeKind::Microphone, false, Some(&empty), true);
        assert_eq!(apps.snapshot().len(), 1, "the same node going Running must publish without a re-parse");
    }

    /// The following state-only event has empty props; re-parsing would drop the entry.
    #[test]
    fn a_running_capture_stream_survives_a_state_only_event() {
        let mut apps = CaptureApps::new();
        apply_capture_info_event(
            &mut apps,
            1,
            NodeKind::Microphone,
            true,
            Some(&capture_props("Stream/Input/Audio")),
            true,
        );
        let empty: HashMap<String, String> = HashMap::new();
        apply_capture_info_event(&mut apps, 1, NodeKind::Microphone, false, Some(&empty), true);

        assert_eq!(apps.snapshot().len(), 1);
    }

    /// cava reads the sink monitor; PipeWire's `stream.capture.sink` property beats a name list.
    #[test]
    fn a_monitor_capture_is_not_a_microphone_user() {
        let mut props = capture_props("Stream/Input/Audio");
        props.insert("stream.capture.sink".to_string(), "true".to_string());

        let mut apps = CaptureApps::new();
        apply_capture_info_event(&mut apps, 1, NodeKind::Microphone, true, Some(&props), true);

        assert!(apps.snapshot().is_empty(), "a visualiser reading the monitor must not light a microphone indicator");
    }

    /// A later state event cannot resurrect a node whose props were rejected.
    #[test]
    fn a_state_event_cannot_resurrect_a_node_whose_props_were_rejected() {
        let mut props = capture_props("Stream/Input/Audio");
        props.insert("stream.capture.sink".to_string(), "true".to_string());

        let mut apps = CaptureApps::new();
        apply_capture_info_event(&mut apps, 1, NodeKind::Microphone, true, Some(&props), true);
        let empty: HashMap<String, String> = HashMap::new();
        apply_capture_info_event(&mut apps, 1, NodeKind::Microphone, false, Some(&empty), true);

        assert!(apps.snapshot().is_empty());
    }

    /// Portal streams may omit pid; dropping one would drop the screencast, unlike the pid-matched
    /// camera path.
    #[test]
    fn a_capture_stream_without_a_pid_is_still_tracked() {
        let props = HashMap::from([("media.class".to_string(), "Stream/Output/Video".to_string())]);

        let mut apps = CaptureApps::new();
        apply_capture_info_event(&mut apps, 1, NodeKind::Screencast, true, Some(&props), true);

        assert_eq!(apps.snapshot(), vec![CaptureApp { node_id: 1, pid: None, app_name: None, running: true }]);
    }

    #[test]
    fn a_node_of_the_wrong_class_for_its_kind_is_dropped() {
        let mut apps = CaptureApps::new();
        apply_capture_info_event(
            &mut apps,
            1,
            NodeKind::Screencast,
            true,
            Some(&capture_props("Stream/Input/Audio")),
            true,
        );

        assert!(apps.snapshot().is_empty());
    }

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
        // Use this process's pid: a spawned child raced with parallel test threads in this binary.
        let pid = std::process::id() as i32;
        let name =
            resolve_process_name(Path::new("/proc"), pid).expect("this process's own /proc entry must be readable");
        assert!(!name.is_empty());
        assert!(!name.ends_with('\n'), "trim_end should have stripped comm's trailing newline");
    }

    #[test]
    fn resolve_process_name_returns_none_for_a_pid_that_does_not_exist() {
        assert_eq!(resolve_process_name(Path::new("/proc"), i32::MAX), None);
    }

    #[test]
    fn build_app_stream_combines_parsing_and_pid_resolution() {
        let pid = std::process::id();
        let expected_process_name = resolve_process_name(Path::new("/proc"), pid as i32);

        let props = HashMap::from([
            ("media.class".to_string(), "Stream/Output/Audio".to_string()),
            ("application.process.id".to_string(), pid.to_string()),
            ("application.name".to_string(), "Test App".to_string()),
        ]);

        let app = build_app_stream(Path::new("/proc"), 42, &props).expect("should build an AppStream");
        assert_eq!(app.id, 42);
        assert_eq!(app.pid, pid as i32);
        assert_eq!(app.name, Some("Test App".to_string()));
        assert_eq!(app.process_name, expected_process_name);
        // Identity comes from properties; volume lives on a param and joins in publish_audio.
        assert_eq!(app.volume, 1.0);
        assert!(!app.muted);
    }

    #[test]
    fn build_app_stream_rejects_a_non_audio_node() {
        let props = HashMap::from([("media.class".to_string(), "Video/Source".to_string())]);
        assert!(build_app_stream(Path::new("/proc"), 1, &props).is_none());
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
        // Bind-time global_bind guarantees the first info event carries PROPS and full props.
        apply_info_event(Path::new("/proc"), &mut apps, 1, true, Some(&zen_browser_stream_props()));
        assert_eq!(apps.snapshot().len(), 1, "the initial props-bearing info event should track the stream");

        // A state-only transition (e.g. RUNNING -> IDLE) sends empty props, not stream removal.
        let state_only_props: HashMap<String, String> = HashMap::new();
        apply_info_event(Path::new("/proc"), &mut apps, 1, false, Some(&state_only_props));

        assert_eq!(
            apps.snapshot().len(),
            1,
            "a state-only info event (no PROPS change) must not drop an already-tracked stream"
        );
    }

    #[test]
    fn apply_info_event_upserts_on_a_props_bearing_event() {
        let mut apps = AudioApps::new();
        apply_info_event(Path::new("/proc"), &mut apps, 1, true, Some(&zen_browser_stream_props()));
        assert_eq!(
            apps.snapshot(),
            vec![build_app_stream(Path::new("/proc"), 1, &zen_browser_stream_props()).unwrap()]
        );
    }

    #[test]
    fn apply_info_event_removes_when_a_props_bearing_event_no_longer_parses() {
        let mut apps = AudioApps::new();
        apply_info_event(Path::new("/proc"), &mut apps, 1, true, Some(&zen_browser_stream_props()));
        assert_eq!(apps.snapshot().len(), 1);

        let non_stream_props = HashMap::from([("media.class".to_string(), "Audio/Sink".to_string())]);
        apply_info_event(Path::new("/proc"), &mut apps, 1, true, Some(&non_stream_props));

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
        // ADR-0053 decision 3: node_id -> id, app_name -> name; keep pid/process_name (ADR-0016).
        let stream =
            AppStream { id: 7, pid: 999, name: Some("Zen".to_string()), process_name: None, volume: 1.0, muted: false };
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
        MixerState {
            hydrated: true,
            apps: AudioApps::new(),
            video_sources: VideoSourceApps::new(),
            microphones: CaptureApps::new(),
            screencasts: CaptureApps::new(),
            nodes: HashMap::new(),
            updates,
            privacy_updates,
            sinks: HashMap::new(),
            sink_nodes: HashMap::new(),
            sources: HashMap::new(),
            source_nodes: HashMap::new(),
            devices: HashMap::new(),
            device_routes: HashMap::new(),
            app_props: HashMap::new(),
            default_sink_name: None,
            default_source_name: None,
            metadata: None,
            metadata_id: None,
        }
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
    fn publish_audio_overlays_a_streams_own_props_reading_onto_its_entry() {
        // Identity arrives in `info`, volume in `param`; this is their join point.
        let (updates, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (privacy_updates, _privacy_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = mixer_state(updates, privacy_updates);
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
        let (privacy_updates, _privacy_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = mixer_state(updates, privacy_updates);
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
