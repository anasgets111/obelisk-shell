//! [`PrivacyController`]: the `oblisk.privacy` state owner. Read-only telemetry (ADR-0034) --
//! no write actions. Split from `privacy` -- see `privacy/mod.rs` for the module-level doc.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use inotify::{Inotify, WatchMask};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::capabilities::audio::mixer::VideoSourceApp;

use super::video::{find_device_openers, read_comm};

/// One active camera user (ADR-0034: `privacy.camera_users: table`, array of `{app_name}`,
/// empty = inactive).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct CameraUser {
    /// The holding process's name, from its PipeWire node when it has one, then `/proc/<pid>/comm`,
    /// falling back to `"pid 1234"`. Always something drawable, never empty.
    pub app_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct PrivacyState {
    /// Every process holding a camera open. Empty means no camera is in use, which is the whole
    /// signal: a config draws an indicator when this is non-empty.
    pub camera_users: Vec<CameraUser>,
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
fn name_camera_users(proc_root: &Path, pids: &[u32], pipewire: &[VideoSourceApp]) -> Vec<CameraUser> {
    pids.iter()
        .copied()
        .map(|pid| {
            let app_name = pipewire
                .iter()
                .find(|source| source.pid == pid as i32)
                .and_then(|source| source.app_name.clone())
                .or_else(|| read_comm(proc_root, pid))
                .unwrap_or_else(|| format!("pid {pid}"));
            CameraUser { app_name }
        })
        .collect()
}

/// A scan and a naming in one call. Test-only since the two halves went their separate ways: the
/// startup path calls them in sequence and keeps the pid set, which is the whole point.
#[cfg(test)]
fn resolve_camera_users(proc_root: &Path, devices: &[PathBuf], pipewire: &[VideoSourceApp]) -> Vec<CameraUser> {
    name_camera_users(proc_root, &scan_camera_pids(proc_root, devices), pipewire)
}

pub struct PrivacyController {
    state: Arc<Mutex<PrivacyState>>,
}

impl PrivacyController {
    /// `proc_root`/`video4linux_root` (real defaults `/proc`/`/sys/class/video4linux`) follow
    /// this codebase's sysfs/procfs root-injection convention. `video_sources` is one PipeWire
    /// connection shared with `oblisk.audio` (ADR-0034). Returns immediately.
    pub fn new(
        proc_root: PathBuf,
        video4linux_root: &Path,
        video_sources: UnboundedReceiver<Vec<VideoSourceApp>>,
        events: UnboundedSender<PrivacySignal>,
    ) -> Self {
        let state = Arc::new(Mutex::new(PrivacyState::default()));
        let devices = super::video::enumerate_video_devices(video4linux_root);
        tokio::spawn(run_camera_task(proc_root, devices, Arc::clone(&state), video_sources, events));
        Self { state }
    }

    pub fn snapshot(&self) -> PrivacyState {
        self.state.lock().unwrap().clone()
    }
}

/// Watches every resolved video device for `OPEN`/`CLOSE` (confirmed live-reliable, see
/// `privacy::video`'s module doc), and separately drains `video_sources` for PipeWire
/// enrichment updates. Either one triggers a full rescan/rebuild of `camera_users`, no debounce.
///
/// Logs and returns without ever updating `state` if inotify can't be initialized or no video
/// devices exist. Still sends one `PrivacySignal::Changed` before returning on each of these
/// paths: without it, a newly-connecting generation never gets even the empty state seeded.
async fn run_camera_task(
    proc_root: PathBuf,
    devices: Vec<PathBuf>,
    state: Arc<Mutex<PrivacyState>>,
    mut video_sources: UnboundedReceiver<Vec<VideoSourceApp>>,
    events: UnboundedSender<PrivacySignal>,
) {
    if devices.is_empty() {
        eprintln!("privacy: no /dev/videoN devices found; camera_users will stay empty");
        let _ = events.send(PrivacySignal::Changed);
        return;
    }

    let inotify = match Inotify::init() {
        Ok(inotify) => inotify,
        Err(err) => {
            eprintln!("privacy: failed to initialize inotify; camera detection disabled for this run: {err}");
            let _ = events.send(PrivacySignal::Changed);
            return;
        }
    };
    for device in &devices {
        if let Err(err) = inotify.watches().add(device, WatchMask::OPEN | WatchMask::CLOSE) {
            eprintln!("privacy: failed to watch {}; camera opens on it won't be detected: {err}", device.display());
        }
    }
    let mut inotify_stream = match inotify.into_event_stream(vec![0u8; 4096]) {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!(
                "privacy: failed to start the inotify event stream; camera detection disabled for this run: {err}"
            );
            let _ = events.send(PrivacySignal::Changed);
            return;
        }
    };

    let mut pipewire_sources: Vec<VideoSourceApp> = Vec::new();

    // Initial scan: a camera can already be open at Supervisor startup, not just from an event
    // seen afterward.
    let mut opener_pids = scan_camera_pids(&proc_root, &devices);
    state.lock().unwrap().camera_users = name_camera_users(&proc_root, &opener_pids, &pipewire_sources);
    if events.send(PrivacySignal::Changed).is_err() {
        return;
    }

    loop {
        tokio::select! {
            event = inotify_stream.next() => {
                match event {
                    // An open or a close on a watched device, the one thing that can change who
                    // holds it: the only arm that pays for a scan.
                    Some(Ok(_)) => opener_pids = scan_camera_pids(&proc_root, &devices),
                    Some(Err(err)) => {
                        eprintln!("privacy: inotify read failed: {err}");
                        continue;
                    }
                    None => break, // every watched device's fd closed -- nothing left to watch.
                }
            }
            sources = video_sources.recv() => {
                match sources {
                    // Names, not openers: the pid set stands, and `name_camera_users` says why.
                    Some(sources) => pipewire_sources = sources,
                    None => break, // the mixer thread is gone -- no more enrichment updates coming.
                }
            }
        }
        state.lock().unwrap().camera_users = name_camera_users(&proc_root, &opener_pids, &pipewire_sources);
        if events.send(PrivacySignal::Changed).is_err() {
            break;
        }
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
            vec![CameraUser { app_name: "Cheese".to_string() }]
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

        assert_eq!(users, vec![CameraUser { app_name: "Firefox".to_string() }]);
    }

    #[test]
    fn resolve_camera_users_falls_back_to_proc_comm_when_pipewire_has_no_match() {
        let root = tempfile::tempdir().unwrap();
        let fd_dir = root.path().join("1234").join("fd");
        std::fs::create_dir_all(&fd_dir).unwrap();
        std::os::unix::fs::symlink("/dev/video0", fd_dir.join("5")).unwrap();
        std::fs::write(root.path().join("1234").join("comm"), "mpv\n").unwrap();

        let users = resolve_camera_users(root.path(), &[PathBuf::from("/dev/video0")], &[]);

        assert_eq!(users, vec![CameraUser { app_name: "mpv".to_string() }]);
    }

    #[test]
    fn resolve_camera_users_falls_back_to_a_pid_placeholder_when_neither_source_resolves() {
        let root = tempfile::tempdir().unwrap();
        let fd_dir = root.path().join("1234").join("fd");
        std::fs::create_dir_all(&fd_dir).unwrap();
        std::os::unix::fs::symlink("/dev/video0", fd_dir.join("5")).unwrap();
        // No comm file written, no matching pipewire source.

        let users = resolve_camera_users(root.path(), &[PathBuf::from("/dev/video0")], &[]);

        assert_eq!(users, vec![CameraUser { app_name: "pid 1234".to_string() }]);
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

        assert_eq!(users, vec![CameraUser { app_name: "mpv".to_string() }]);
    }

    #[test]
    fn privacy_state_default_has_no_camera_users() {
        assert_eq!(PrivacyState::default(), PrivacyState { camera_users: vec![] });
    }

    #[tokio::test]
    async fn no_video_devices_still_sends_one_signal_so_the_empty_state_gets_announced() {
        let video4linux_root = tempfile::tempdir().unwrap(); // empty -- no videoN entries.
        let (_video_tx, video_sources) = tokio::sync::mpsc::unbounded_channel();
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

        let controller =
            PrivacyController::new(PathBuf::from("/proc"), video4linux_root.path(), video_sources, events_tx);

        let signal = tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv()).await;
        assert_eq!(
            signal,
            Ok(Some(PrivacySignal::Changed)),
            "must still announce an empty state when no camera hardware exists"
        );
        assert!(controller.snapshot().camera_users.is_empty());
    }
}
