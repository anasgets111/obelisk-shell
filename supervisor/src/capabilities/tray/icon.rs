//! Icon pipeline: `IconName`-vs-`IconPixmap` resolution, bounds-checking, and ARGB32-to-PNG
//! encoding/spooling (ADR-0031: prefer IconName, decode IconPixmap only as fallback).
//! Split from `dbus::tray` -- see `dbus/tray/mod.rs` for the module-level doc.

use crate::capabilities::shm_icons::{self, PngEncodeError};

use super::MAX_PIXMAP_DIMENSION;

/// `/dev/shm/oblisk-$UID` subdirectory for tray pixmaps, shared by writes, per-item deletion, and
/// startup sweep.
pub(super) const SPOOL_SUBDIR: &str = "tray";

#[derive(Debug, Clone, PartialEq)]
pub(super) struct IconPixmap {
    pub(super) width: i32,
    pub(super) height: i32,
    pub(super) bytes: Vec<u8>,
}

/// Validates one raw `IconPixmap` (docs/oblisk-supervisor-services-dbus.md §2.1): non-empty square,
/// at most
/// [`MAX_PIXMAP_DIMENSION`], with exactly `width * height * 4` ARGB32 bytes.
fn pixmap_is_valid(width: i32, height: i32, byte_len: usize) -> bool {
    width > 0
        && width == height
        && width <= MAX_PIXMAP_DIMENSION
        && (width as usize) * (height as usize) * 4 == byte_len
}

/// Largest valid pixmap (ADR-0031: no target-size guess, cap at 128; downscaling beats upscaling).
pub(super) fn largest_valid_pixmap(pixmaps: &[IconPixmap]) -> Option<&IconPixmap> {
    pixmaps
        .iter()
        .filter(|pixmap| pixmap_is_valid(pixmap.width, pixmap.height, pixmap.bytes.len()))
        .max_by_key(|pixmap| pixmap.width)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum IconSource {
    /// `IconName` resolved inside the item's `IconThemePath` (ADR-0074), ahead of [`Self::Name`].
    ThemePathFile(String),
    /// Non-empty `IconName`, preferred over pixmaps (ADR-0031), so no decode or PNG spool occurs.
    Name(String),
    /// Empty `IconName` with at least one valid pixmap.
    Pixmap,
    /// Neither source is usable.
    None,
}

/// Finds `icon_name` in the item's `IconThemePath`, trying the bare name, `.png`, then `.svg`.
/// The spec says only "a directory of icons" and leaves layout to the application. Rejects `/`
/// and `\`; a name containing `/` or `..` could reach outside it.
pub(super) fn theme_path_file(theme_path: &str, icon_name: &str) -> Option<String> {
    if theme_path.is_empty() || icon_name.is_empty() || icon_name.contains('/') || icon_name.contains('\\') {
        return None;
    }
    let dir = std::path::Path::new(theme_path);
    ["", ".png", ".svg"]
        .iter()
        .map(|ext| dir.join(format!("{icon_name}{ext}")))
        .find(|candidate| candidate.is_file())
        .map(|path| path.to_string_lossy().into_owned())
}

/// Chooses the source and whether decoding is needed (ADR-0031). This stays a cheap, pure decision;
/// the caller re-derives the largest pixmap only for [`IconSource::Pixmap`].
pub(super) fn resolve_icon_source(icon_name: &str, pixmaps: &[IconPixmap], theme_path: &str) -> IconSource {
    if let Some(path) = theme_path_file(theme_path, icon_name) {
        IconSource::ThemePathFile(path)
    } else if !icon_name.is_empty() {
        IconSource::Name(icon_name.to_string())
    } else if largest_valid_pixmap(pixmaps).is_some() {
        IconSource::Pixmap
    } else {
        IconSource::None
    }
}

/// Encodes bounds-checked ARGB32 bytes (network order A, R, G, B) to PNG via `png` (ADR-0031:
/// pure Rust, encode-only, minimal dependency tree).
fn encode_argb32_to_png(width: u32, height: u32, argb: &[u8]) -> Result<Vec<u8>, PngEncodeError> {
    let mut rgba = Vec::with_capacity(argb.len());
    let (chunks, _remainder) = argb.as_chunks::<4>();
    for &[a, r, g, b] in chunks {
        rgba.push(r);
        rgba.push(g);
        rgba.push(b);
        rgba.push(a);
    }

    let mut buffer = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buffer, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(PngEncodeError::Png)?;
        writer.write_image_data(&rgba).map_err(PngEncodeError::Png)?;
    }
    Ok(buffer)
}

/// Writes a validated pixmap to `/dev/shm/oblisk-$UID/tray/{filename_stem}.png`
/// ([`shm_icons::write_png`]), creating the tree and overwriting the same path (no cache-busting,
/// ADR-0031). Base, attention, and overlay stems differ so their files do not collide (ADR-0074).
pub(super) fn write_icon_png(filename_stem: &str, pixmap: &IconPixmap) -> std::io::Result<String> {
    let png_bytes = encode_argb32_to_png(pixmap.width as u32, pixmap.height as u32, &pixmap.bytes)
        .map_err(std::io::Error::other)?;
    shm_icons::write_png(SPOOL_SUBDIR, &format!("{filename_stem}.png"), &png_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- theme_path_file / IconThemePath (ADR-0074) ----

    #[test]
    fn an_items_own_theme_path_beats_the_session_theme() {
        // Prefer artwork the session theme does not know.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("my-app.png"), b"not really a png").unwrap();
        let found = theme_path_file(dir.path().to_str().unwrap(), "my-app").expect("the shipped file");
        assert!(found.ends_with("my-app.png"));
        assert_eq!(
            resolve_icon_source("my-app", &[], dir.path().to_str().unwrap()),
            IconSource::ThemePathFile(found),
            "and it outranks the name"
        );
    }

    #[test]
    fn a_theme_path_that_does_not_hold_the_icon_falls_back_to_the_name() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(theme_path_file(dir.path().to_str().unwrap(), "my-app"), None);
        assert_eq!(
            resolve_icon_source("my-app", &[], dir.path().to_str().unwrap()),
            IconSource::Name("my-app".to_string())
        );
    }

    #[test]
    fn an_svg_is_found_and_so_is_a_name_that_carries_its_own_extension() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("vector.svg"), b"<svg/>").unwrap();
        std::fs::write(dir.path().join("literal.ico"), b"x").unwrap();
        assert!(theme_path_file(dir.path().to_str().unwrap(), "vector").unwrap().ends_with("vector.svg"));
        assert!(theme_path_file(dir.path().to_str().unwrap(), "literal.ico").unwrap().ends_with("literal.ico"));
    }

    #[test]
    fn an_icon_name_cannot_escape_the_directory_it_was_given() {
        // A themed name has no separator; refuse one rather than clean it up.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.png"), b"x").unwrap();
        assert_eq!(theme_path_file(dir.path().to_str().unwrap(), "../../etc/passwd"), None);
        assert_eq!(theme_path_file(dir.path().to_str().unwrap(), "sub/real"), None);
    }

    #[test]
    fn no_theme_path_and_no_name_still_reaches_the_pixmap() {
        // Empty theme paths must not shadow Chromium's pixmap branch.
        let pixmaps = vec![IconPixmap { width: 2, height: 2, bytes: vec![0; 16] }];
        assert_eq!(resolve_icon_source("", &pixmaps, ""), IconSource::Pixmap);
    }

    // ---- pixmap_is_valid ----

    #[test]
    fn pixmap_is_valid_accepts_a_well_formed_square_pixmap() {
        assert!(pixmap_is_valid(2, 2, 2 * 2 * 4));
    }

    #[test]
    fn pixmap_is_valid_rejects_a_non_square_pixmap() {
        assert!(!pixmap_is_valid(4, 2, 4 * 2 * 4));
    }

    #[test]
    fn pixmap_is_valid_rejects_oversized_pixmaps() {
        assert!(!pixmap_is_valid(129, 129, 129 * 129 * 4));
        assert!(pixmap_is_valid(128, 128, 128 * 128 * 4));
    }

    #[test]
    fn pixmap_is_valid_rejects_a_byte_length_mismatch() {
        assert!(!pixmap_is_valid(2, 2, 10));
    }

    #[test]
    fn pixmap_is_valid_rejects_non_positive_dimensions() {
        assert!(!pixmap_is_valid(0, 0, 0));
        assert!(!pixmap_is_valid(-1, -1, 4));
    }

    // ---- largest_valid_pixmap ----

    fn pixmap(size: i32) -> IconPixmap {
        IconPixmap { width: size, height: size, bytes: vec![0u8; (size * size * 4) as usize] }
    }

    #[test]
    fn largest_valid_pixmap_picks_the_biggest() {
        let pixmaps = vec![pixmap(16), pixmap(64), pixmap(32)];
        assert_eq!(largest_valid_pixmap(&pixmaps), Some(&pixmaps[1]));
    }

    #[test]
    fn largest_valid_pixmap_ignores_invalid_entries() {
        let oversized = IconPixmap { width: 200, height: 200, bytes: vec![0u8; 200 * 200 * 4] };
        let valid = pixmap(16);
        let pixmaps = vec![oversized, valid.clone()];
        assert_eq!(largest_valid_pixmap(&pixmaps), Some(&pixmaps[1]));
        assert_eq!(pixmaps[1], valid);
    }

    #[test]
    fn largest_valid_pixmap_is_none_when_every_entry_is_invalid() {
        let pixmaps = vec![IconPixmap { width: 4, height: 2, bytes: vec![0u8; 32] }];
        assert_eq!(largest_valid_pixmap(&pixmaps), None);
    }

    #[test]
    fn largest_valid_pixmap_is_none_for_an_empty_list() {
        assert_eq!(largest_valid_pixmap(&[]), None);
    }

    // ---- resolve_icon_source ----

    #[test]
    fn resolve_icon_source_prefers_icon_name_even_with_pixmaps_present() {
        let pixmaps = vec![pixmap(32)];
        assert_eq!(resolve_icon_source("battery-full", &pixmaps, ""), IconSource::Name("battery-full".to_string()));
    }

    #[test]
    fn resolve_icon_source_falls_back_to_pixmap_when_icon_name_is_empty() {
        let pixmaps = vec![pixmap(32)];
        assert_eq!(resolve_icon_source("", &pixmaps, ""), IconSource::Pixmap);
    }

    #[test]
    fn resolve_icon_source_is_none_when_neither_is_usable() {
        assert_eq!(resolve_icon_source("", &[], ""), IconSource::None);
        let invalid = vec![IconPixmap { width: 4, height: 2, bytes: vec![0u8; 32] }];
        assert_eq!(resolve_icon_source("", &invalid, ""), IconSource::None);
    }

    // ---- encode_argb32_to_png round trip through the real png crate ----

    #[test]
    fn encode_argb32_to_png_round_trips_a_known_pixel() {
        // One 1x1 SNI pixel: A=0x11, R=0x22, G=0x33, B=0x44.
        let argb = vec![0x11, 0x22, 0x33, 0x44];
        let png_bytes = encode_argb32_to_png(1, 1, &argb).expect("encoding must succeed");

        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes.as_slice()));
        let mut reader = decoder.read_info().expect("valid PNG header");
        let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).expect("valid PNG frame");
        let rgba = &buf[..info.buffer_size()];

        // PNG stores R, G, B, A, so the encoder must reorder ARGB.
        assert_eq!(rgba, &[0x22, 0x33, 0x44, 0x11]);
    }
}
