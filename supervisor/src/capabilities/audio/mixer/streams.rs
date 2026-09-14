//! PipeWire stream nodes for `obelisk.audio` and `obelisk.privacy`: classifying a node by
//! `media.class`, and tracking playback streams, cameras, and microphone and screen captures from
//! their `info` events.

use std::collections::BTreeMap;
use std::path::Path;

use pipewire::keys;
use serde::Serialize;

use super::PropsLookup;

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
/// ADR-0053 decision 3 names `id`/`name` to match the spec; ADR-0016's `pid`/`process_name` remain.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct AppStream {
    /// PipeWire registry id, the `MixerState::apps` key.
    pub id: u32,
    /// `application.process.id` recorded for the owning process.
    pub pid: i32,
    /// `application.name`, if the client set one.
    pub name: Option<String>,
    /// `/proc/{pid}/comm`, if the process still existed when observed.
    pub process_name: Option<String>,
    /// Per-app volume, range `[0.0, 1.0]`, cube-rooted from `SPA_PARAM_Props` like a master
    /// sink (`pw-cli enum-params <id> Props` confirms cubed `channelVolumes`). `nil` until then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<f32>,
    /// Per-app mute, from the same `Props` as `volume`.
    pub muted: bool,
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
    Some(AppStream { id: node_id, pid: parsed.pid, name: parsed.app_name, process_name, volume: None, muted: false })
}

/// Applies a bound node's `info`. Gate upsert/remove on `NodeChangeMask::PROPS`: state-only events
/// carry empty props and must not drop a live stream. `global_bind`'s first `info` has PROPS.
pub(super) fn apply_info_event(
    proc_root: &Path,
    apps: &mut BTreeMap<u32, AppStream>,
    node_id: u32,
    has_props_change: bool,
    props: Option<&impl PropsLookup>,
) {
    if !has_props_change {
        return;
    }
    match props.and_then(|props| build_app_stream(proc_root, node_id, props)) {
        Some(app) => apps.insert(node_id, app),
        None => apps.remove(&node_id),
    };
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
    sources: &mut BTreeMap<u32, VideoSourceApp>,
    node_id: u32,
    has_props_change: bool,
    props: Option<&impl PropsLookup>,
) {
    if !has_props_change {
        return;
    }
    match props.and_then(|props| parse_video_source_props(node_id, props)) {
        Some(source) => sources.insert(node_id, source),
        None => sources.remove(&node_id),
    };
}

/// One microphone or screen-capture stream (ADR-0137); the list it is in supplies the kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct CaptureApp {
    /// PipeWire registry id, its list's key.
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
    apps: &mut BTreeMap<u32, CaptureApp>,
    node_id: u32,
    kind: NodeKind,
    has_props_change: bool,
    props: Option<&impl PropsLookup>,
    running: bool,
) {
    if has_props_change {
        match props.and_then(|props| parse_capture_props(node_id, kind, props)) {
            Some(app) => apps.insert(node_id, app),
            None => apps.remove(&node_id),
        };
    }
    // An unknown node stays unknown: a state event must not resurrect a rejected monitor capture.
    if let Some(app) = apps.get_mut(&node_id) {
        app.running = running;
    }
}

/// Running entries only. Idle streams stay tracked so a later `Running` needs no re-parse.
pub(super) fn running(apps: &BTreeMap<u32, CaptureApp>) -> Vec<CaptureApp> {
    apps.values().filter(|app| app.running).cloned().collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

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
        let mut apps = BTreeMap::new();
        apply_capture_info_event(
            &mut apps,
            1,
            NodeKind::Microphone,
            true,
            Some(&capture_props("Stream/Input/Audio")),
            false,
        );
        assert!(running(&apps).is_empty(), "an idle stream is not capture");

        let empty: HashMap<String, String> = HashMap::new();
        apply_capture_info_event(&mut apps, 1, NodeKind::Microphone, false, Some(&empty), true);
        assert_eq!(running(&apps).len(), 1, "the same node going Running must publish without a re-parse");
    }

    /// The following state-only event has empty props; re-parsing would drop the entry.
    #[test]
    fn a_running_capture_stream_survives_a_state_only_event() {
        let mut apps = BTreeMap::new();
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

        assert_eq!(running(&apps).len(), 1);
    }

    /// cava reads the sink monitor; PipeWire's `stream.capture.sink` property beats a name list.
    #[test]
    fn a_monitor_capture_is_not_a_microphone_user() {
        let mut props = capture_props("Stream/Input/Audio");
        props.insert("stream.capture.sink".to_string(), "true".to_string());

        let mut apps = BTreeMap::new();
        apply_capture_info_event(&mut apps, 1, NodeKind::Microphone, true, Some(&props), true);

        assert!(running(&apps).is_empty(), "a visualiser reading the monitor must not light a microphone indicator");
    }

    /// A later state event cannot resurrect a node whose props were rejected.
    #[test]
    fn a_state_event_cannot_resurrect_a_node_whose_props_were_rejected() {
        let mut props = capture_props("Stream/Input/Audio");
        props.insert("stream.capture.sink".to_string(), "true".to_string());

        let mut apps = BTreeMap::new();
        apply_capture_info_event(&mut apps, 1, NodeKind::Microphone, true, Some(&props), true);
        let empty: HashMap<String, String> = HashMap::new();
        apply_capture_info_event(&mut apps, 1, NodeKind::Microphone, false, Some(&empty), true);

        assert!(running(&apps).is_empty());
    }

    /// Portal streams may omit pid; dropping one would drop the screencast, unlike the pid-matched
    /// camera path.
    #[test]
    fn a_capture_stream_without_a_pid_is_still_tracked() {
        let props = HashMap::from([("media.class".to_string(), "Stream/Output/Video".to_string())]);

        let mut apps = BTreeMap::new();
        apply_capture_info_event(&mut apps, 1, NodeKind::Screencast, true, Some(&props), true);

        assert_eq!(running(&apps), vec![CaptureApp { node_id: 1, pid: None, app_name: None, running: true }]);
    }

    #[test]
    fn a_node_of_the_wrong_class_for_its_kind_is_dropped() {
        let mut apps = BTreeMap::new();
        apply_capture_info_event(
            &mut apps,
            1,
            NodeKind::Screencast,
            true,
            Some(&capture_props("Stream/Input/Audio")),
            true,
        );

        assert!(running(&apps).is_empty());
    }

    #[test]
    fn apply_video_info_event_ignores_a_state_only_event() {
        let mut sources = BTreeMap::new();
        apply_video_info_event(&mut sources, 1, true, Some(&camera_stream_props()));
        let empty: HashMap<String, String> = HashMap::new();
        apply_video_info_event(&mut sources, 1, false, Some(&empty));
        assert_eq!(sources.len(), 1, "a non-PROPS info event must not drop an already-tracked source");
    }

    #[test]
    fn apply_video_info_event_removes_when_a_props_bearing_event_no_longer_parses() {
        let mut sources = BTreeMap::new();
        apply_video_info_event(&mut sources, 1, true, Some(&camera_stream_props()));
        assert_eq!(sources.len(), 1);

        let non_video_props = HashMap::from([("media.class".to_string(), "Audio/Sink".to_string())]);
        apply_video_info_event(&mut sources, 1, true, Some(&non_video_props));

        assert!(sources.is_empty());
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
        assert_eq!(app.volume, None);
        assert!(!app.muted);
    }

    #[test]
    fn build_app_stream_rejects_a_non_audio_node() {
        let props = HashMap::from([("media.class".to_string(), "Video/Source".to_string())]);
        assert!(build_app_stream(Path::new("/proc"), 1, &props).is_none());
    }

    #[test]
    fn apply_info_event_keeps_a_tracked_stream_through_a_state_only_info_event() {
        let mut apps = BTreeMap::new();
        // Bind-time global_bind guarantees the first info event carries PROPS and full props.
        apply_info_event(Path::new("/proc"), &mut apps, 1, true, Some(&zen_browser_stream_props()));
        assert_eq!(apps.len(), 1, "the initial props-bearing info event should track the stream");

        // A state-only transition (e.g. RUNNING -> IDLE) sends empty props, not stream removal.
        let state_only_props: HashMap<String, String> = HashMap::new();
        apply_info_event(Path::new("/proc"), &mut apps, 1, false, Some(&state_only_props));

        assert_eq!(apps.len(), 1, "a state-only info event (no PROPS change) must not drop an already-tracked stream");
    }

    #[test]
    fn apply_info_event_upserts_on_a_props_bearing_event() {
        let mut apps = BTreeMap::new();
        apply_info_event(Path::new("/proc"), &mut apps, 1, true, Some(&zen_browser_stream_props()));
        assert_eq!(
            apps.values().cloned().collect::<Vec<_>>(),
            vec![build_app_stream(Path::new("/proc"), 1, &zen_browser_stream_props()).unwrap()]
        );
    }

    #[test]
    fn apply_info_event_removes_when_a_props_bearing_event_no_longer_parses() {
        let mut apps = BTreeMap::new();
        apply_info_event(Path::new("/proc"), &mut apps, 1, true, Some(&zen_browser_stream_props()));
        assert_eq!(apps.len(), 1);

        let non_stream_props = HashMap::from([("media.class".to_string(), "Audio/Sink".to_string())]);
        apply_info_event(Path::new("/proc"), &mut apps, 1, true, Some(&non_stream_props));

        assert!(apps.is_empty(), "a PROPS-bearing event that no longer parses as a stream should remove it");
    }

    #[test]
    fn app_stream_serializes_with_the_spec_field_spelling() {
        // ADR-0053 decision 3: node_id -> id, app_name -> name; keep pid/process_name (ADR-0016).
        let stream = AppStream {
            id: 7,
            pid: 999,
            name: Some("Zen".to_string()),
            process_name: None,
            volume: Some(1.0),
            muted: false,
        };
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
}
