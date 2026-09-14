//! [`PrivacyController`] owns read-only `obelisk.privacy` telemetry (ADR-0034).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use inotify::{Inotify, WatchMask};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::capabilities::audio::mixer::{CaptureApp, PrivacySources, VideoSourceApp};

use super::video::{find_device_openers, read_comm};

/// An app using one watched resource. The `{app_name}` row shape from ADR-0034's
/// `privacy.camera_users` extends to microphone and screencast under ADR-0137; one type keeps the
/// three "who" lists identical.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct PrivacyUser {
    /// Process name from its PipeWire node, then `/proc/<pid>/comm`, then `"pid 1234"`; never
    /// empty.
    pub app_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct PrivacyState {
    /// Processes holding a camera open. Empty means no camera is in use; a config draws its
    /// indicator only when this is non-empty.
    pub camera_users: Vec<PrivacyUser>,
    /// Apps PipeWire reports reading a microphone now (ADR-0137). Open-but-idle streams are absent:
    /// this means "something is listening", not "something could".
    ///
    /// Distinct from `obelisk.audio.source_muted`, a device setting; a muted active capture appears
    /// in both.
    pub microphone_users: Vec<PrivacyUser>,
    /// Apps producing PipeWire screen-capture streams (ADR-0137). Names may be the portal's
    /// identity for portal-created nodes. wlr-screencopy recorders (`wf-recorder`, `grim`) bypass
    /// PipeWire and never appear.
    pub screencast_users: Vec<PrivacyUser>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivacySignal {
    Changed,
}

/// Deduplicated pids holding any `devices` open, from the expensive `/proc/*/fd/*` scan. One pass
/// on this machine is ~370 `opendir`s, ~11,000 `readlink`s, and ~6 MiB allocation, 58% of boot
/// allocations under DHAT. Only device open/close changes this set, so only inotify pays for it;
/// [`name_camera_users`] remains separate.
fn scan_camera_pids(proc_root: &Path, devices: &[PathBuf]) -> Vec<u32> {
    let mut pids: Vec<u32> =
        devices.iter().flat_map(|device| find_device_openers(proc_root, &device.to_string_lossy())).collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// Names scanned opener pids against the latest PipeWire `Video/Source` snapshot. A matching
/// PipeWire `app_name` wins, then `/proc/{pid}/comm`, then `pid {n}`; no opener is dropped. Pure
/// and unit-testable.
///
/// Takes `pids` rather than finding them: a new PipeWire `Video/Source` renames an existing opener,
/// not the `/dev/videoN` set. A fresh scan would spend the expensive scan for a string.
fn name_camera_users(proc_root: &Path, pids: &[u32], pipewire: &[VideoSourceApp]) -> Vec<PrivacyUser> {
    pids.iter()
        .copied()
        .map(|pid| {
            let app_name = pipewire
                .iter()
                .find(|source| source.pid == pid as i32)
                .and_then(|source| source.app_name.clone())
                .or_else(|| read_comm(proc_root, pid))
                .unwrap_or_else(|| format!("pid {pid}"));
            PrivacyUser { app_name }
        })
        .collect()
}

/// Names PipeWire captures by client `application.name`, then `/proc/{pid}/comm`, then `pid {n}`,
/// finally node id. Portal-created streams may have no pid, but remain captures.
///
/// Deduplicated by name, not node: browsers open one node per tab, but users need one Firefox row.
fn name_capture_users(proc_root: &Path, apps: &[CaptureApp]) -> Vec<PrivacyUser> {
    let mut users: Vec<PrivacyUser> = Vec::new();
    for app in apps {
        let app_name = app
            .app_name
            .clone()
            .or_else(|| app.pid.and_then(|pid| read_comm(proc_root, pid as u32)))
            .or_else(|| app.pid.map(|pid| format!("pid {pid}")))
            .unwrap_or_else(|| format!("node {}", app.node_id));
        if !users.iter().any(|user| user.app_name == app_name) {
            users.push(PrivacyUser { app_name });
        }
    }
    users
}

/// Test-only scan-plus-name convenience. Production calls the halves in sequence and retains pids.
#[cfg(test)]
fn resolve_camera_users(proc_root: &Path, devices: &[PathBuf], pipewire: &[VideoSourceApp]) -> Vec<PrivacyUser> {
    name_camera_users(proc_root, &scan_camera_pids(proc_root, devices), pipewire)
}

pub struct PrivacyController {
    state: Arc<Mutex<PrivacyState>>,
}

impl PrivacyController {
    /// `proc_root`/`video4linux_root` (defaults `/proc`/`/sys/class/video4linux`) are injected per
    /// the sysfs/procfs test convention. `sources` is the PipeWire connection shared with
    /// `obelisk.audio` (ADR-0034). Returns immediately.
    pub fn new(
        proc_root: PathBuf,
        video4linux_root: &Path,
        sources: UnboundedReceiver<PrivacySources>,
        events: UnboundedSender<PrivacySignal>,
    ) -> Self {
        let state = Arc::new(Mutex::new(PrivacyState::default()));
        let devices = super::video::enumerate_video_devices(video4linux_root);
        tokio::spawn(run_privacy_task(proc_root, devices, Arc::clone(&state), sources, events));
        Self { state }
    }

    pub fn snapshot(&self) -> PrivacyState {
        self.state.lock().unwrap().clone()
    }
}

/// Watches resolved video devices for live-reliable `OPEN`/`CLOSE` (see `privacy::video`) and
/// drains `sources`. Either triggers a full rebuild of all three lists, without debounce; only a
/// device event pays for the `/proc` scan.
///
/// The camera watch is optional, not the loop. Without a webcam, or after `Inotify`/stream failure,
/// `camera_users` stays empty while microphone and screencast continue. Before ADR-0137 those
/// failures ended the task, which was correct when camera was the only answer.
async fn run_privacy_task(
    proc_root: PathBuf,
    devices: Vec<PathBuf>,
    state: Arc<Mutex<PrivacyState>>,
    mut sources: UnboundedReceiver<PrivacySources>,
    events: UnboundedSender<PrivacySignal>,
) {
    let mut inotify_stream = watch_video_devices(&devices);
    let mut pipewire = PrivacySources::default();

    // Scan once: a camera may already be open at startup. The other lists await PipeWire.
    let mut opener_pids = scan_camera_pids(&proc_root, &devices);
    publish(&proc_root, &state, &opener_pids, &pipewire);
    if events.send(PrivacySignal::Changed).is_err() {
        return;
    }

    loop {
        tokio::select! {
            event = next_device_event(&mut inotify_stream) => {
                match event {
                    // Only a device open/close can change who holds it; this arm pays for the scan.
                    DeviceEvent::Opened => opener_pids = scan_camera_pids(&proc_root, &devices),
                    DeviceEvent::Failed(err) => {
                        eprintln!("privacy: inotify read failed: {err}");
                        continue;
                    }
                    // All watched device fds closed; drop the watch but keep the task.
                    DeviceEvent::Ended => {
                        inotify_stream = None;
                        continue;
                    }
                }
            }
            update = sources.recv() => {
                match update {
                    // Name camera users and rebuild the other lists from the same pid set.
                    Some(update) => pipewire = update,
                    None => break, // the mixer thread is gone -- no more updates coming.
                }
            }
        }
        publish(&proc_root, &state, &opener_pids, &pipewire);
        if events.send(PrivacySignal::Changed).is_err() {
            break;
        }
    }
}

/// Rebuilds all three lists into `state` every time. The PipeWire lists are cheap walks, and one
/// write prevents a config observing one list a push behind.
fn publish(proc_root: &Path, state: &Arc<Mutex<PrivacyState>>, opener_pids: &[u32], pipewire: &PrivacySources) {
    *state.lock().unwrap() = PrivacyState {
        camera_users: name_camera_users(proc_root, opener_pids, &pipewire.cameras),
        microphone_users: name_capture_users(proc_root, &pipewire.microphones),
        screencast_users: name_capture_users(proc_root, &pipewire.screencasts),
    };
}

/// Inotify stream for `/dev/videoN`, or `None` when no device exists or setup failed. Failure costs
/// only `camera_users` and is logged.
fn watch_video_devices(devices: &[PathBuf]) -> Option<inotify::EventStream<Vec<u8>>> {
    if devices.is_empty() {
        eprintln!("privacy: no /dev/videoN devices found; camera_users will stay empty");
        return None;
    }
    let inotify = match Inotify::init() {
        Ok(inotify) => inotify,
        Err(err) => {
            eprintln!("privacy: failed to initialize inotify; camera detection disabled for this run: {err}");
            return None;
        }
    };
    for device in devices {
        if let Err(err) = inotify.watches().add(device, WatchMask::OPEN | WatchMask::CLOSE) {
            eprintln!("privacy: failed to watch {}; camera opens on it won't be detected: {err}", device.display());
        }
    }
    match inotify.into_event_stream(vec![0u8; 4096]) {
        Ok(stream) => Some(stream),
        Err(err) => {
            eprintln!(
                "privacy: failed to start the inotify event stream; camera detection disabled for this run: {err}"
            );
            None
        }
    }
}

/// Camera watch event. The enum names cases and lets [`next_device_event`] flatten a missing watch.
enum DeviceEvent {
    Opened,
    Failed(std::io::Error),
    Ended,
}

/// Next device open/close, or a never-completing future without a camera. `select!` still needs an
/// arm future in that case.
async fn next_device_event(stream: &mut Option<inotify::EventStream<Vec<u8>>>) -> DeviceEvent {
    let Some(stream) = stream.as_mut() else { return std::future::pending().await };
    match stream.next().await {
        Some(Ok(_)) => DeviceEvent::Opened,
        Some(Err(err)) => DeviceEvent::Failed(err),
        None => DeviceEvent::Ended,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video_source(pid: i32, app_name: &str) -> VideoSourceApp {
        VideoSourceApp { node_id: 1, pid, app_name: Some(app_name.to_string()) }
    }

    #[test]
    fn resolve_camera_users_is_empty_when_nobody_has_a_device_open() {
        let root = tempfile::tempdir().unwrap();
        assert!(resolve_camera_users(root.path(), &[PathBuf::from("/dev/video0")], &[]).is_empty());
    }

    /// Keeps scan and naming separate: a PipeWire snapshot can be answered without scanning. This
    /// root has no `/dev/video0` symlink, so an empty scan can still name the previous pid set.
    #[test]
    fn a_pipewire_snapshot_names_the_openers_already_found_rather_than_scanning_again() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("1234")).unwrap();
        std::fs::write(root.path().join("1234").join("comm"), "raw-binary-name\n").unwrap();

        assert_eq!(
            name_camera_users(root.path(), &[1234], &[video_source(1234, "Cheese")]),
            vec![PrivacyUser { app_name: "Cheese".to_string() }]
        );
        assert!(
            resolve_camera_users(root.path(), &[PathBuf::from("/dev/video0")], &[]).is_empty(),
            "a scan of the same root finds nobody, so the answer above came from the pid set"
        );
    }

    #[test]
    fn resolve_camera_users_prefers_the_pipewire_app_name_over_proc_comm() {
        let root = tempfile::tempdir().unwrap();
        let fd_dir = root.path().join("1234").join("fd");
        std::fs::create_dir_all(&fd_dir).unwrap();
        std::os::unix::fs::symlink("/dev/video0", fd_dir.join("5")).unwrap();
        std::fs::write(root.path().join("1234").join("comm"), "raw-binary-name\n").unwrap();

        let users =
            resolve_camera_users(root.path(), &[PathBuf::from("/dev/video0")], &[video_source(1234, "Firefox")]);

        assert_eq!(users, vec![PrivacyUser { app_name: "Firefox".to_string() }]);
    }

    #[test]
    fn resolve_camera_users_falls_back_to_proc_comm_when_pipewire_has_no_match() {
        let root = tempfile::tempdir().unwrap();
        let fd_dir = root.path().join("1234").join("fd");
        std::fs::create_dir_all(&fd_dir).unwrap();
        std::os::unix::fs::symlink("/dev/video0", fd_dir.join("5")).unwrap();
        std::fs::write(root.path().join("1234").join("comm"), "mpv\n").unwrap();

        let users = resolve_camera_users(root.path(), &[PathBuf::from("/dev/video0")], &[]);

        assert_eq!(users, vec![PrivacyUser { app_name: "mpv".to_string() }]);
    }

    #[test]
    fn resolve_camera_users_falls_back_to_a_pid_placeholder_when_neither_source_resolves() {
        let root = tempfile::tempdir().unwrap();
        let fd_dir = root.path().join("1234").join("fd");
        std::fs::create_dir_all(&fd_dir).unwrap();
        std::os::unix::fs::symlink("/dev/video0", fd_dir.join("5")).unwrap();
        // No comm file written, no matching pipewire source.

        let users = resolve_camera_users(root.path(), &[PathBuf::from("/dev/video0")], &[]);

        assert_eq!(users, vec![PrivacyUser { app_name: "pid 1234".to_string() }]);
    }

    #[test]
    fn resolve_camera_users_dedupes_a_pid_that_has_two_devices_open() {
        let root = tempfile::tempdir().unwrap();
        let fd_dir = root.path().join("1234").join("fd");
        std::fs::create_dir_all(&fd_dir).unwrap();
        std::os::unix::fs::symlink("/dev/video0", fd_dir.join("5")).unwrap();
        std::os::unix::fs::symlink("/dev/video1", fd_dir.join("6")).unwrap();
        std::fs::write(root.path().join("1234").join("comm"), "mpv\n").unwrap();

        let users =
            resolve_camera_users(root.path(), &[PathBuf::from("/dev/video0"), PathBuf::from("/dev/video1")], &[]);

        assert_eq!(users, vec![PrivacyUser { app_name: "mpv".to_string() }]);
    }

    #[test]
    fn privacy_state_default_reports_nothing_in_use() {
        assert_eq!(
            PrivacyState::default(),
            PrivacyState { camera_users: vec![], microphone_users: vec![], screencast_users: vec![] }
        );
    }

    fn capture(node_id: u32, pid: Option<i32>, app_name: Option<&str>) -> CaptureApp {
        CaptureApp { node_id, pid, app_name: app_name.map(str::to_string), running: true }
    }

    #[test]
    fn a_capture_stream_is_named_by_the_name_its_client_published() {
        let root = tempfile::tempdir().unwrap();
        let users = name_capture_users(root.path(), &[capture(7, Some(1234), Some("Firefox"))]);

        assert_eq!(users, vec![PrivacyUser { app_name: "Firefox".to_string() }]);
    }

    #[test]
    fn a_capture_stream_with_no_published_name_falls_back_to_the_processs_own() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("1234")).unwrap();
        std::fs::write(root.path().join("1234").join("comm"), "pw-cat\n").unwrap();

        let users = name_capture_users(root.path(), &[capture(7, Some(1234), None)]);

        assert_eq!(users, vec![PrivacyUser { app_name: "pw-cat".to_string() }]);
    }

    /// Portal-created streams may lack `application.process.id`; node id is the last resort, not a
    /// dropped capture.
    #[test]
    fn a_capture_stream_with_neither_a_name_nor_a_pid_is_named_by_its_node() {
        let root = tempfile::tempdir().unwrap();
        let users = name_capture_users(root.path(), &[capture(7, None, None)]);

        assert_eq!(users, vec![PrivacyUser { app_name: "node 7".to_string() }]);
    }

    /// One row per app, not per stream; a browser opens one node per tab.
    #[test]
    fn one_app_holding_several_streams_is_listed_once() {
        let root = tempfile::tempdir().unwrap();
        let users = name_capture_users(
            root.path(),
            &[capture(7, Some(1234), Some("Firefox")), capture(8, Some(1234), Some("Firefox"))],
        );

        assert_eq!(users, vec![PrivacyUser { app_name: "Firefox".to_string() }]);
    }

    #[tokio::test]
    async fn no_video_devices_still_sends_one_signal_so_the_empty_state_gets_announced() {
        let video4linux_root = tempfile::tempdir().unwrap(); // empty -- no videoN entries.
        let (_privacy_tx, sources) = tokio::sync::mpsc::unbounded_channel();
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

        let controller = PrivacyController::new(PathBuf::from("/proc"), video4linux_root.path(), sources, events_tx);

        let signal = tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv()).await;
        assert_eq!(
            signal,
            Ok(Some(PrivacySignal::Changed)),
            "must still announce an empty state when no camera hardware exists"
        );
        assert!(controller.snapshot().camera_users.is_empty());
    }

    /// ADR-0137 regression: no webcam used to end the task, taking microphone and screencast down.
    #[tokio::test]
    async fn a_machine_with_no_camera_still_reports_a_microphone() {
        let video4linux_root = tempfile::tempdir().unwrap(); // empty -- no videoN entries.
        let (privacy_tx, sources) = tokio::sync::mpsc::unbounded_channel();
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

        let controller = PrivacyController::new(PathBuf::from("/proc"), video4linux_root.path(), sources, events_tx);
        assert_eq!(events_rx.recv().await, Some(PrivacySignal::Changed), "the empty seed");

        privacy_tx
            .send(PrivacySources {
                microphones: vec![capture(7, Some(1234), Some("Firefox"))],
                ..PrivacySources::default()
            })
            .unwrap();

        let signal = tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv()).await;
        assert_eq!(signal, Ok(Some(PrivacySignal::Changed)));
        assert_eq!(
            controller.snapshot().microphone_users,
            vec![PrivacyUser { app_name: "Firefox".to_string() }],
            "a PipeWire capture must be reported on a machine that has no camera to watch"
        );
    }
}
