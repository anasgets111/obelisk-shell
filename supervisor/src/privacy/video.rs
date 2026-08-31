//! Kernel-level camera detection half of `oblisk.privacy` (ADR-0034): which processes have a
//! `/dev/videoN` capture device open, found by a `fuser`-equivalent scan of `/proc/*/fd/*`
//! symlinks -- inotify `OPEN`/`CLOSE` events on the device node trigger a rescan (verified
//! live-reliable: opening then closing `/dev/video0` fired real `IN_OPEN`/`IN_CLOSE_NOWRITE`
//! events, a different, VFS-level mechanism from the sysfs-attribute-notify path that turned
//! out unreliable for keyboard lock LEDs -- see `hardware::keyboard::locks`'s module doc).
//!
//! The `/sys/class/video4linux/video<n>/streaming` fast-path flag ADR-0034 also proposed is
//! deliberately not implemented: this dev machine's real UVC webcam doesn't expose it despite
//! running past the "6.3+" threshold, so it can't be verified live, and the fd-scan below is
//! sufficient and already needed on its own regardless. Left for the ADR's upgrade path.

use std::path::{Path, PathBuf};

/// Every `/dev/videoN` device this machine's kernel currently advertises, resolved once --
/// cameras don't typically hotplug, so a USB webcam plugged in after boot won't be picked up.
pub fn enumerate_video_devices(video4linux_root: &Path) -> Vec<PathBuf> {
    let mut devices = Vec::new();
    let Ok(entries) = std::fs::read_dir(video4linux_root) else { return devices };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(index) = name.strip_prefix("video")
            && !index.is_empty()
            && index.chars().all(|c| c.is_ascii_digit())
        {
            devices.push(PathBuf::from(format!("/dev/{name}")));
        }
    }
    devices.sort();
    devices
}

/// A `fuser`-equivalent: every pid with an open file descriptor whose target resolves to
/// exactly `device_path`, found by scanning `<proc_root>/*/fd/*` symlinks. `proc_root` is
/// injected for testing. A pid with more than one fd open on the same device appears once.
pub fn find_device_openers(proc_root: &Path, device_path: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    let Ok(proc_entries) = std::fs::read_dir(proc_root) else { return pids };
    for proc_entry in proc_entries.flatten() {
        let Ok(pid) = proc_entry.file_name().to_string_lossy().parse::<u32>() else { continue };
        let Ok(fd_entries) = std::fs::read_dir(proc_entry.path().join("fd")) else { continue };
        let has_device_open = fd_entries
            .flatten()
            .any(|fd_entry| std::fs::read_link(fd_entry.path()).is_ok_and(|target| target == Path::new(device_path)));
        if has_device_open {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    pids
}

/// Reads `<proc_root>/<pid>/comm` -- the fallback app name for an opener PipeWire's
/// `Video/Source` enrichment doesn't have a matching node for (a raw V4L2 user, ADR-0034).
pub fn read_comm(proc_root: &Path, pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(proc_root.join(pid.to_string()).join("comm")).ok()?;
    Some(text.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn write_video_device_dir(video4linux_root: &Path, name: &str) {
        std::fs::create_dir(video4linux_root.join(name)).unwrap();
    }

    // ---- enumerate_video_devices ----

    #[test]
    fn enumerate_video_devices_finds_every_videon_directory_as_a_dev_path() {
        let root = tempfile::tempdir().unwrap();
        write_video_device_dir(root.path(), "video0");
        write_video_device_dir(root.path(), "video1");

        assert_eq!(
            enumerate_video_devices(root.path()),
            vec![PathBuf::from("/dev/video0"), PathBuf::from("/dev/video1")]
        );
    }

    #[test]
    fn enumerate_video_devices_ignores_non_video_entries() {
        let root = tempfile::tempdir().unwrap();
        write_video_device_dir(root.path(), "video0");
        write_video_device_dir(root.path(), "vbi0"); // a real video4linux sibling class, not a capture device.

        assert_eq!(enumerate_video_devices(root.path()), vec![PathBuf::from("/dev/video0")]);
    }

    #[test]
    fn enumerate_video_devices_is_empty_against_a_nonexistent_root() {
        let root = tempfile::tempdir().unwrap();
        assert!(enumerate_video_devices(&root.path().join("does-not-exist")).is_empty());
    }

    // ---- find_device_openers ----

    fn write_fd_symlink(proc_root: &Path, pid: u32, fd: u32, target: &str) {
        let fd_dir = proc_root.join(pid.to_string()).join("fd");
        std::fs::create_dir_all(&fd_dir).unwrap();
        symlink(target, fd_dir.join(fd.to_string())).unwrap();
    }

    #[test]
    fn find_device_openers_finds_a_pid_with_the_device_open() {
        let root = tempfile::tempdir().unwrap();
        write_fd_symlink(root.path(), 1234, 5, "/dev/video0");
        write_fd_symlink(root.path(), 1234, 6, "/dev/null");

        assert_eq!(find_device_openers(root.path(), "/dev/video0"), vec![1234]);
    }

    #[test]
    fn find_device_openers_lists_a_pid_once_even_with_multiple_fds_on_the_device() {
        let root = tempfile::tempdir().unwrap();
        write_fd_symlink(root.path(), 1234, 5, "/dev/video0");
        write_fd_symlink(root.path(), 1234, 6, "/dev/video0");

        assert_eq!(find_device_openers(root.path(), "/dev/video0"), vec![1234]);
    }

    #[test]
    fn find_device_openers_ignores_a_pid_with_no_matching_fd() {
        let root = tempfile::tempdir().unwrap();
        write_fd_symlink(root.path(), 1234, 5, "/dev/null");

        assert!(find_device_openers(root.path(), "/dev/video0").is_empty());
    }

    #[test]
    fn find_device_openers_is_empty_against_an_empty_proc_root() {
        let root = tempfile::tempdir().unwrap();
        assert!(find_device_openers(root.path(), "/dev/video0").is_empty());
    }

    #[test]
    fn find_device_openers_finds_multiple_distinct_pids_sorted() {
        let root = tempfile::tempdir().unwrap();
        write_fd_symlink(root.path(), 999, 3, "/dev/video0");
        write_fd_symlink(root.path(), 42, 3, "/dev/video0");

        assert_eq!(find_device_openers(root.path(), "/dev/video0"), vec![42, 999]);
    }

    // ---- read_comm ----

    #[test]
    fn read_comm_reads_and_trims_a_real_comm_file() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("1234")).unwrap();
        std::fs::write(root.path().join("1234").join("comm"), "mpv\n").unwrap();

        assert_eq!(read_comm(root.path(), 1234), Some("mpv".to_string()));
    }

    #[test]
    fn read_comm_is_none_for_a_pid_with_no_comm_file() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read_comm(root.path(), 9999), None);
    }
}
