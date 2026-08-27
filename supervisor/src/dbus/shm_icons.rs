//! Shared SHM icon-spooling helpers for `dbus::tray` and `dbus::notifications` (code-review
//! finding 6: both controllers spooled bounds-checked PNG bytes to
//! `/dev/shm/oblisk-$UID/{subdir}/...` and each defined a byte-for-byte identical `PngEncodeError`
//! plus its own copy of the "make the dir, write the file, hand back the path" shape -- only the
//! subdirectory name and the upstream pixel-source-to-PNG-bytes step actually differ between them,
//! so those two stay local to each controller (`tray::encode_argb32_to_png` reorders ARGB->RGBA
//! from `IconPixmap`; `notifications::encode_image_data_to_png` encodes an already-RGB(A)
//! `image-data` hint) while this module holds only the truly shared PNG-error type and the
//! directory/write mechanics.

use std::path::PathBuf;

/// PNG encoding failure -- wraps the `png` crate's own error type. Shared because both
/// controllers' encoders hit the same `png::Encoder`/`png::Writer` API, just with different
/// pixel-source shapes upstream of it.
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
