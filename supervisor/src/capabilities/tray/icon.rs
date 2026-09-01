//! Icon pipeline: `IconName`-vs-`IconPixmap` resolution, bounds-checking, and ARGB32-to-PNG
//! encoding/spooling (ADR-0031: prefer IconName, decode IconPixmap only as fallback).
//! Split from `dbus::tray` -- see `dbus/tray/mod.rs` for the module-level doc.

use crate::capabilities::shm_icons::{self, PngEncodeError};

use super::MAX_PIXMAP_DIMENSION;

/// The `/dev/shm/oblisk-$UID` subdirectory every tray pixmap is spooled into. Named once because
/// three call sites now agree on it: the write, the per-item delete, and the startup sweep.
pub(super) const SPOOL_SUBDIR: &str = "tray";

#[derive(Debug, Clone, PartialEq)]
pub(super) struct IconPixmap {
    pub(super) width: i32,
    pub(super) height: i32,
    pub(super) bytes: Vec<u8>,
}

/// Bounds-checks one raw `IconPixmap` (docs/oblisk-supervisor-services-dbus.md §2.1): square,
/// non-empty, capped at [`MAX_PIXMAP_DIMENSION`], and its byte length matches `width * height *
/// 4` (ARGB32, 4 bytes/pixel) exactly.
fn pixmap_is_valid(width: i32, height: i32, byte_len: usize) -> bool {
    width > 0
        && width == height
        && width <= MAX_PIXMAP_DIMENSION
        && (width as usize) * (height as usize) * 4 == byte_len
}

/// The single largest pixmap that passes [`pixmap_is_valid`] (ADR-0031: "no target-size guess,
/// largest capped at 128" -- downscaling a large source always beats upscaling a small one).
pub(super) fn largest_valid_pixmap(pixmaps: &[IconPixmap]) -> Option<&IconPixmap> {
    pixmaps
        .iter()
        .filter(|pixmap| pixmap_is_valid(pixmap.width, pixmap.height, pixmap.bytes.len()))
        .max_by_key(|pixmap| pixmap.width)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum IconSource {
    /// `IconName` named a file inside the item's own `IconThemePath` (ADR-0074). Wins over
    /// [`Self::Name`], because a name that resolved to a concrete file has no business being looked
    /// up again in a session theme that has never heard of it.
    ThemePathFile(String),
    /// `IconName` was non-empty -- preferred over a pixmap (ADR-0031), skips the whole
    /// decode/PNG-spool pipeline.
    Name(String),
    /// `IconName` was empty but at least one pixmap passed bounds-checking.
    Pixmap,
    /// Neither source is usable.
    None,
}

/// The file `icon_name` names inside the item's own `IconThemePath`, or `None` when the item
/// declares no such directory or holds no such file (ADR-0074).
///
/// Three spellings tried, because the spec says only "a directory of icons" and leaves the layout
/// to the application: the bare name for a name that already carries its extension, then `.png` and
/// `.svg`, which is what the ones that do this actually ship.
///
/// `IconThemePath` is an application-supplied path, so a name containing `/` or `..` would reach
/// outside it. Rejected rather than sanitized: a real `IconName` is a themed icon name and never
/// contains either, and the alternative is path canonicalization to decide what is inside a
/// directory the shell does not own.
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

/// Which icon source to use, and whether a pixmap decode is even necessary (ADR-0031's
/// IconName-preference decision) -- kept separate from the pixmap *value* so this stays a
/// cheap, pure decision; the caller re-derives the actual largest pixmap via
/// [`largest_valid_pixmap`] only when this returns [`IconSource::Pixmap`].
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

/// Encodes a bounds-checked ARGB32 (network byte order: A, R, G, B per pixel) buffer to a PNG
/// byte stream via the `png` crate (ADR-0031: pure Rust, encode-only, minimal dependency tree).
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

/// Writes `pixmap` (already bounds-checked) as a PNG to
/// `/dev/shm/oblisk-$UID/tray/{filename_stem}.png` ([`shm_icons::write_png`]), creating the
/// directory tree if missing. Same path overwritten in place on every call -- no cache-busting
/// (ADR-0031).
///
/// `filename_stem` is the item's sanitized unique name for its base icon and that plus a variant
/// suffix for the other two, so an item's attention and overlay pixmaps do not overwrite each other
/// or the icon they sit beside (ADR-0074).
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
        // The whole point: an application that ships artwork the session theme has never heard of
        // gets that artwork, instead of a name the renderer will look up and miss.
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
        // `IconThemePath` and `IconName` both come from the application. A themed icon name never
        // contains a separator, so a name that does is refused rather than cleaned up.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.png"), b"x").unwrap();
        assert_eq!(theme_path_file(dir.path().to_str().unwrap(), "../../etc/passwd"), None);
        assert_eq!(theme_path_file(dir.path().to_str().unwrap(), "sub/real"), None);
    }

    #[test]
    fn no_theme_path_and_no_name_still_reaches_the_pixmap() {
        // The regression this signature change could have caused: an empty theme path must not
        // shadow the pixmap branch every Chromium tray item depends on.
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

    // ---- encode_argb32_to_png (round trip through the real png crate, both encode and decode) ----

    #[test]
    fn encode_argb32_to_png_round_trips_a_known_pixel() {
        // One 1x1 pixel: A=0x11, R=0x22, G=0x33, B=0x44 (network byte order per the SNI spec).
        let argb = vec![0x11, 0x22, 0x33, 0x44];
        let png_bytes = encode_argb32_to_png(1, 1, &argb).expect("encoding must succeed");

        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes.as_slice()));
        let mut reader = decoder.read_info().expect("valid PNG header");
        let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).expect("valid PNG frame");
        let rgba = &buf[..info.buffer_size()];

        // R, G, B, A -- the encoder must reorder from the source's A, R, G, B.
        assert_eq!(rgba, &[0x22, 0x33, 0x44, 0x11]);
    }
}
