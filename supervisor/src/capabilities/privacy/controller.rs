//! [`PrivacyController`]: the `oblisk.privacy` state owner. Read-only telemetry (ADR-0034) --
//! no write actions. Split from `privacy` -- see `privacy/mod.rs` for the module-level doc.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use inotify::{Inotify, WatchMask};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::capabilities::audio::mixer::{CaptureApp, PrivacySources, VideoSourceApp};

use super::video::{find_device_openers, read_comm};

/// One app using one of the three things this capability watches (ADR-0034's
/// `privacy.camera_users: table`, array of `{app_name}`; extended to microphone and screencast by
/// ADR-0137). One type for all three because all three answer the same question, "who", and a
/// second identical struct would only make the three lists look like they differ.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct PrivacyUser {
    /// The using process's name, from its PipeWire node when it has one, then `/proc/<pid>/comm`,
    /// falling back to `"pid 1234"`. Always something drawable, never empty.
    pub app_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct PrivacyState {
    /// Every process holding a camera open. Empty means no camera is in use, which is the whole
    /// signal: a config draws an indicator when this is non-empty.
    pub camera_users: Vec<PrivacyUser>,
    /// Every app PipeWire reports as reading a microphone right now (ADR-0137). A stream that is
    /// open but idle is not here, so this is "something is listening", not "something could".
    ///
    /// Not the same question as `oblisk.audio`'s `source_muted`, which is a device setting: a
    /// muted microphone with a running capture stream appears in both.
    pub microphone_users: Vec<PrivacyUser>,
    /// Every app producing a screen-capture stream into PipeWire (ADR-0137). The name is
    /// best-effort and may be the portal rather than the app that asked it, since a portal-created
    /// node carries the portal's identity. Screen recorders on wlr-screencopy (`wf-recorder`,
    /// `grim`) never reach PipeWire and never appear here.
    pub screencast_users: Vec<PrivacyUser>,
}

/// One shared signal, `Changed` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivacySignal {
    Changed,
}

/// Every pid holding one of `devices` open, deduped across devices: the `/proc/*/fd/*` scan, and
/// the expensive half of answering `camera_users`. On this machine one pass is ~370 `opendir`s and
/// ~11,000 `readlink`s, about 6 MiB of allocation -- 58% of everything the Supervisor allocates
/// during a boot, measured under DHAT. Which is fine for what it answers, and the reason
/// [`name_camera_users`] is a separate function: only a device open or close can change this set,
/// so only inotify's event runs it.
fn scan_camera_pids(proc_root: &Path, devices: &[PathBuf]) -> Vec<u32> {
    let mut pids: Vec<u32> =
        devices.iter().flat_map(|device| find_device_openers(proc_root, &device.to_string_lossy())).collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// Names an already-scanned set of opener pids against the latest PipeWire `Video/Source`
/// enrichment snapshot: PipeWire's `app_name` wins when a pid matches a tracked video-source node,
/// else `/proc/{pid}/comm`, else a `pid {n}` placeholder so a real opener is never silently
/// dropped. Pure and unit-testable.
///
/// Takes `pids` rather than finding them, because a PipeWire snapshot renames openers it never
/// discovers: a `Video/Source` node appearing says a camera app registered with PipeWire, not that
/// the set of processes holding `/dev/videoN` changed. Answering one with a fresh scan spent that
/// scan for a string.
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

/// Names a PipeWire capture list on the same terms [`name_camera_users`] names openers: the
/// client's own `application.name` first, then `/proc/{pid}/comm`, then a `pid {n}` placeholder,
/// and finally the node id, because a portal-created stream carries no pid at all and a capture
/// nobody can name is still a capture.
///
/// Deduplicated by name, not by node: a browser opens one node per tab, and a list that says
/// Firefox three times is noise in front of a user who wants to know whether anything is
/// listening.
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

/// A scan and a naming in one call. Test-only since the two halves went their separate ways: the
/// startup path calls them in sequence and keeps the pid set, which is the whole point.
#[cfg(test)]
fn resolve_camera_users(proc_root: &Path, devices: &[PathBuf], pipewire: &[VideoSourceApp]) -> Vec<PrivacyUser> {
    name_camera_users(proc_root, &scan_camera_pids(proc_root, devices), pipewire)
}

pub struct PrivacyController {
    state: Arc<Mutex<PrivacyState>>,
}

impl PrivacyController {
    /// `proc_root`/`video4linux_root` (real defaults `/proc`/`/sys/class/video4linux`) follow
    /// this codebase's sysfs/procfs root-injection convention. `sources` is one PipeWire
    /// connection shared with `oblisk.audio` (ADR-0034). Returns immediately.
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

/// Watches every resolved video device for `OPEN`/`CLOSE` (confirmed live-reliable, see
/// `privacy::video`'s module doc), and drains `sources` for the PipeWire side. Either one
/// triggers a full rebuild of all three lists, no debounce; only a device event pays for the
/// `/proc` scan.
///
/// The camera watch is optional and the loop is not. A desktop with no webcam has no
/// `/dev/videoN` to watch and still has a microphone, so a missing device, a failed `Inotify`,
/// and a stream that ends all leave `camera_users` permanently empty while the other two lists
/// keep being served. Before ADR-0137 each of those returned from the task outright, which was
/// right when a camera was all this capability answered.
async fn run_privacy_task(
    proc_root: PathBuf,
    devices: Vec<PathBuf>,
    state: Arc<Mutex<PrivacyState>>,
    mut sources: UnboundedReceiver<PrivacySources>,
    events: UnboundedSender<PrivacySignal>,
) {
    let mut inotify_stream = watch_video_devices(&devices);
    let mut pipewire = PrivacySources::default();

    // Initial scan: a camera can already be open at Supervisor startup, not just from an event
    // seen afterward. The other two lists start empty and stay so until PipeWire says otherwise.
    let mut opener_pids = scan_camera_pids(&proc_root, &devices);
    publish(&proc_root, &state, &opener_pids, &pipewire);
    if events.send(PrivacySignal::Changed).is_err() {
        return;
    }

    loop {
        tokio::select! {
            event = next_device_event(&mut inotify_stream) => {
                match event {
                    // An open or a close on a watched device, the one thing that can change who
                    // holds it: the only arm that pays for a scan.
                    DeviceEvent::Opened => opener_pids = scan_camera_pids(&proc_root, &devices),
                    DeviceEvent::Failed(err) => {
                        eprintln!("privacy: inotify read failed: {err}");
                        continue;
                    }
                    // Every watched device's fd closed. Drop the watch, keep the task.
                    DeviceEvent::Ended => {
                        inotify_stream = None;
                        continue;
                    }
                }
            }
            update = sources.recv() => {
                match update {
                    // Names for the camera list, and the whole answer for the other two: the
                    // opener pid set stands either way, and `name_camera_users` says why.
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

/// Rebuilds all three lists into `state`. All three every time: two of them are a walk over a
/// list PipeWire already handed over, and splitting the writes would let a config observe a
/// snapshot where one list is a push behind.
fn publish(proc_root: &Path, state: &Arc<Mutex<PrivacyState>>, opener_pids: &[u32], pipewire: &PrivacySources) {
    *state.lock().unwrap() = PrivacyState {
        camera_users: name_camera_users(proc_root, opener_pids, &pipewire.cameras),
        microphone_users: name_capture_users(proc_root, &pipewire.microphones),
        screencast_users: name_capture_users(proc_root, &pipewire.screencasts),
    };
}

/// The inotify stream watching every `/dev/videoN`, or `None` when there is nothing to watch or
/// inotify could not be set up. Every failure here is logged and costs only `camera_users`.
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

/// What one camera watch event was. An enum rather than the stream's own item type so the caller
/// reads as three named cases, and so [`next_device_event`] can flatten the missing-watch case
/// into the same match.
enum DeviceEvent {
    Opened,
    Failed(std::io::Error),
    Ended,
}

/// The next device open or close, or a future that never completes when there is no camera to
/// watch. `select!` needs a future in every arm, and this is the arm that has to be allowed to
/// have nothing behind it.
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

    /// The reason the scan and the naming are two functions: a PipeWire snapshot is answered
    /// without one. The root here holds no `/dev/video0` symlink at all, so a scan of it finds
    /// nobody -- and naming the pid set the last scan produced still answers correctly.
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

    /// A portal-created stream carries no `application.process.id` at all, and a capture nobody
    /// can name is still a capture: the node id is the last resort rather than a dropped entry.
    #[test]
    fn a_capture_stream_with_neither_a_name_nor_a_pid_is_named_by_its_node() {
        let root = tempfile::tempdir().unwrap();
        let users = name_capture_users(root.path(), &[capture(7, None, None)]);

        assert_eq!(users, vec![PrivacyUser { app_name: "node 7".to_string() }]);
    }

    /// One row per app, not per stream: a browser opens a node per tab.
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

    /// The regression ADR-0137 had to avoid: a desktop with no webcam used to end the whole task,
    /// which would now take microphone and screencast detection down with the camera.
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
