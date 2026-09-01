//! Shared SHM icon-spooling helpers for `dbus::tray` and `dbus::notifications`: both spool
//! bounds-checked PNG bytes to `/dev/shm/oblisk-$UID/{subdir}/...` and shared a byte-for-byte
//! identical `PngEncodeError` plus the "make the dir, write the file, hand back the path"
//! shape. Only the subdirectory name and the pixel-source-to-PNG-bytes step differ, so those
//! stay local to each controller; this module holds only the shared PNG-error type and the
//! directory/write mechanics.

use std::path::PathBuf;

/// PNG encoding failure -- wraps the `png` crate's own error type. Shared because both
/// controllers' encoders hit the same `png::Encoder`/`png::Writer` API.
#[derive(Debug)]
pub enum PngEncodeError {
    Png(png::EncodingError),
}

impl std::fmt::Display for PngEncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Png(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for PngEncodeError {}

/// `/dev/shm/oblisk-$UID/{subdir}` (ADR-0031/ADR-0033: the `$UID` fix both controllers share).
pub fn icon_dir(subdir: &str) -> PathBuf {
    PathBuf::from(format!("/dev/shm/oblisk-{}/{subdir}", nix::unistd::Uid::current()))
}

/// Deletes one spooled PNG, best-effort.
///
/// `path` must be one this module wrote, which is checked rather than trusted: this deletes a file
/// from a path that travelled through a `TrayItem` and back, and the check costs a prefix compare.
/// A failure is silent because every caller is already tearing something down and there is nothing
/// useful to do about a file that is already gone.
pub fn remove_png(subdir: &str, path: &str) {
    if !std::path::Path::new(path).starts_with(icon_dir(subdir)) {
        return;
    }
    let _ = std::fs::remove_file(path);
}

/// Empties `subdir` of the files a previous run left behind.
///
/// Safe only at startup, and only because a fresh Supervisor owns nothing in there yet: its registry
/// is empty and every item re-registers from scratch, spooling again. `/dev/shm` outlives the
/// process, so without this every icon a killed run spooled stays resident until the machine
/// reboots -- files from the previous day were still sitting there when this was written.
///
/// ponytail: two Supervisors at once and the second sweeps the first's live files, which shows as a
/// tray of blank icons until something makes each item re-spool. Two shells is a debugging accident
/// rather than a mode, and the alternative is a lockfile or an mtime cutoff to avoid a leak measured
/// in kilobytes.
pub fn sweep(subdir: &str) {
    let dir = icon_dir(subdir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.path().is_file() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Writes already-encoded `png_bytes` to `/dev/shm/oblisk-$UID/{subdir}/{filename}`, creating the
/// directory tree if missing. Same path overwritten in place on every call -- no cache-busting
/// (ADR-0031, carried forward for notifications by ADR-0033).
pub fn write_png(subdir: &str, filename: &str, png_bytes: &[u8]) -> std::io::Result<String> {
    let dir = icon_dir(subdir);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(filename);
    std::fs::write(&path, png_bytes)?;
    Ok(path.to_string_lossy().into_owned())
}
