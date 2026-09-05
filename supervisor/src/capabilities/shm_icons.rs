//! Shared icon-spooling helpers for `dbus::tray` and `dbus::notifications`: both spool
//! bounds-checked PNG bytes to `$XDG_RUNTIME_DIR/oblisk/{subdir}/...` and shared a byte-for-byte
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

/// `$XDG_RUNTIME_DIR/oblisk/{subdir}` (ADR-0142; was `/dev/shm/oblisk-$UID` under ADR-0031).
///
/// Not `/dev/shm`: it is mode 1777, so any other local user can create `oblisk-$UID` before this
/// process does. A symlink there aims [`sweep`]'s startup delete at a directory of their choosing
/// and makes [`write_png`] follow the link. The runtime directory is 0700 and owned by us, which
/// is the property the `$UID` suffix was reaching for. Same tmpfs either way.
///
/// The fallback is the path systemd would have set, not `/dev/shm`: a spool that silently reverts
/// to a world-writable directory when one variable is missing is the hole, not the repair. If it
/// does not exist, [`write_png`] fails and the item draws without an icon.
pub fn icon_dir(subdir: &str) -> PathBuf {
    spool_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), subdir)
}

/// [`icon_dir`] with the environment passed in, so the fallback is testable without a test that
/// mutates process-wide state to prove it.
fn spool_dir(runtime: Option<&std::ffi::OsStr>, subdir: &str) -> PathBuf {
    let runtime =
        runtime.map_or_else(|| PathBuf::from(format!("/run/user/{}", nix::unistd::Uid::current())), PathBuf::from);
    runtime.join("oblisk").join(subdir)
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
/// is empty and every item re-registers from scratch, spooling again. The runtime directory
/// outlives the process, so without this every icon a killed run spooled stays resident until the
/// session ends -- files from the previous day were still sitting there when this was written.
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

/// Writes already-encoded `png_bytes` to `$XDG_RUNTIME_DIR/oblisk/{subdir}/{filename}`, creating the
/// directory tree if missing. Same path overwritten in place on every call -- no cache-busting
/// (ADR-0031, carried forward for notifications by ADR-0033).
pub fn write_png(subdir: &str, filename: &str, png_bytes: &[u8]) -> std::io::Result<String> {
    let dir = icon_dir(subdir);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(filename);
    std::fs::write(&path, png_bytes)?;
    Ok(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn the_spool_directory_is_the_private_runtime_one_and_never_a_shared_one() {
        assert_eq!(spool_dir(Some(OsStr::new("/run/user/4242")), "tray"), PathBuf::from("/run/user/4242/oblisk/tray"));

        let fallback = spool_dir(None, "notifications");
        assert!(
            fallback.starts_with("/run/user/"),
            "an unset variable must not drop the spool into a world-writable directory: {}",
            fallback.display()
        );
    }

    #[test]
    fn a_path_outside_the_spool_is_not_deleted() {
        remove_png("tray", "/etc/passwd");
        assert!(std::path::Path::new("/etc/passwd").exists(), "remove_png must refuse a path it did not write");
    }
}
