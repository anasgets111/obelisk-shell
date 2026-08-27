//! [`PrivacyController`]: the `oblisk.privacy` state owner. Read-only telemetry (ADR-0034) --
//! no write actions, unlike every capability with a `parse_*_args`/dispatch arm in `main.rs`.
//! Split from `privacy` -- see `privacy/mod.rs` for the module-level doc.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use inotify::{Inotify, WatchMask};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::audio::mixer::VideoSourceApp;

use super::video::{find_device_openers, read_comm};

/// One active camera user (ADR-0034: `privacy.camera_users: table`, array of `{app_name}`,
/// empty = inactive).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CameraUser {
    pub app_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct PrivacyState {
    pub camera_users: Vec<CameraUser>,
}

/// One shared signal, `Changed` only (mirrors `KeyboardSignal`/`SysinfoSignal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivacySignal {
    Changed,
}

/// Combines a set of kernel-detected `/dev/videoN` opener pids with the latest PipeWire
/// `Video/Source` enrichment snapshot into the `camera_users` list: PipeWire's `app_name` wins
/// when a pid matches a tracked video-source node (the portal-routed subset PipeWire can see,
/// ADR-0034), else `/proc/{pid}/comm`, else a `pid {n}` placeholder so a real opener is never
/// silently dropped just because neither name source resolved. Pure and unit-testable: the
/// pid-to-name resolution is exactly what a real rescan does, just with `proc_root`/`pipewire`
/// as parameters instead of live state.
fn resolve_camera_users(proc_root: &Path, devices: &[PathBuf], pipewire: &[VideoSourceApp]) -> Vec<CameraUser> {
    let mut pids: Vec<u32> = devices.iter().flat_map(|device| find_device_openers(proc_root, &device.to_string_lossy())).collect();
    pids.sort_unstable();
    pids.dedup();

    pids.into_iter()
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

pub struct PrivacyController {
    state: Arc<Mutex<PrivacyState>>,
}

impl PrivacyController {
    /// `proc_root`/`video4linux_root` (real defaults `/proc`/`/sys/class/video4linux`) follow
    /// this codebase's sysfs/procfs root-injection convention. `video_sources` is the receiving
    /// half of `audio::mixer::run`'s `video_updates` channel -- one PipeWire connection shared
    /// with `oblisk.audio`, not a second one (ADR-0034). Returns immediately; the real inotify
    /// watch and initial scan happen in a spawned task, matching every other event-driven
    /// controller's "construction never blocks on I/O" shape.
    pub fn new(proc_root: PathBuf, video4linux_root: &Path, video_sources: UnboundedReceiver<Vec<VideoSourceApp>>, events: UnboundedSender<PrivacySignal>) -> Self {
        let state = Arc::new(Mutex::new(PrivacyState::default()));
        let devices = super::video::enumerate_video_devices(video4linux_root);
        tokio::spawn(run_camera_task(proc_root, devices, Arc::clone(&state), video_sources, events));
        Self { state }
    }

    pub fn snapshot(&self) -> PrivacyState {
        self.state.lock().unwrap().clone()
    }
}

/// Watches every resolved video device for `OPEN`/`CLOSE` (confirmed live-reliable on this dev
/// machine -- see `privacy::video`'s module doc comment), and separately drains
/// `video_sources` for PipeWire enrichment updates. Either one triggers a full rescan/rebuild of
/// `camera_users` (no debounce, no incremental patching -- matches every other capability's
/// "full re-derive on any relevant event" discipline already established in this codebase).
/// Logs and returns without ever *updating* `state` if inotify itself can't be initialized or
/// no video devices exist -- `camera_users` stays at its empty default, the same "degrade
/// gracefully, don't fake activity" posture every other missing-capability path here already
/// takes. Still sends one `PrivacySignal::Changed` before returning on each of these paths
/// (Correctness review): without it, `main.rs` never pushes even the empty `StateSnapshot`, so
/// `last_snapshots["privacy"]` -- what a newly-connecting generation gets hydrated from -- never
/// gets seeded at all. On the very common "no camera hardware" case (most desktop/server
/// machines), every client for the process's whole lifetime would otherwise see `oblisk.privacy`
/// as never having existed, not as "reports empty" -- a real, client-visible difference.
async fn run_camera_task(proc_root: PathBuf, devices: Vec<PathBuf>, state: Arc<Mutex<PrivacyState>>, mut video_sources: UnboundedReceiver<Vec<VideoSourceApp>>, events: UnboundedSender<PrivacySignal>) {
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
            eprintln!("privacy: failed to start the inotify event stream; camera detection disabled for this run: {err}");
            let _ = events.send(PrivacySignal::Changed);
            return;
        }
    };

    let mut pipewire_sources: Vec<VideoSourceApp> = Vec::new();

    // Initial scan: a camera can already be open at Supervisor startup, not just from an event
    // seen afterward.
    state.lock().unwrap().camera_users = resolve_camera_users(&proc_root, &devices, &pipewire_sources);
    if events.send(PrivacySignal::Changed).is_err() {
        return;
    }

    loop {
        tokio::select! {
            event = inotify_stream.next() => {
                match event {
                    Some(Ok(_)) => {}
                    Some(Err(err)) => {
                        eprintln!("privacy: inotify read failed: {err}");
                        continue;
                    }
                    None => break, // every watched device's fd closed -- nothing left to watch.
                }
            }
            sources = video_sources.recv() => {
                match sources {
                    Some(sources) => pipewire_sources = sources,
                    None => break, // the mixer thread is gone -- no more enrichment updates coming.
                }
            }
        }
        state.lock().unwrap().camera_users = resolve_camera_users(&proc_root, &devices, &pipewire_sources);
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

    #[test]
    fn resolve_camera_users_prefers_the_pipewire_app_name_over_proc_comm() {
        let root = tempfile::tempdir().unwrap();
        let fd_dir = root.path().join("1234").join("fd");
        std::fs::create_dir_all(&fd_dir).unwrap();
        std::os::unix::fs::symlink("/dev/video0", fd_dir.join("5")).unwrap();
        std::fs::write(root.path().join("1234").join("comm"), "raw-binary-name\n").unwrap();

        let users = resolve_camera_users(root.path(), &[PathBuf::from("/dev/video0")], &[video_source(1234, "Firefox")]);

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

        let users = resolve_camera_users(root.path(), &[PathBuf::from("/dev/video0"), PathBuf::from("/dev/video1")], &[]);

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

        let controller = PrivacyController::new(PathBuf::from("/proc"), video4linux_root.path(), video_sources, events_tx);

        let signal = tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv()).await;
        assert_eq!(signal, Ok(Some(PrivacySignal::Changed)), "must still announce an empty state when no camera hardware exists");
        assert!(controller.snapshot().camera_users.is_empty());
    }
}
