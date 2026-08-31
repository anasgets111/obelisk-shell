//! Icon pipeline: `IconName`-vs-`IconPixmap` resolution, bounds-checking, and ARGB32-to-PNG
//! encoding/spooling (ADR-0031: prefer IconName, decode IconPixmap only as fallback).
//! Split from `dbus::tray` -- see `dbus/tray/mod.rs` for the module-level doc.

use crate::dbus::shm_icons::{self, PngEncodeError};

use super::MAX_PIXMAP_DIMENSION;

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
    /// `IconName` was non-empty -- preferred (ADR-0031), skips the whole decode/PNG-spool
    /// pipeline.
    Name(String),
    /// `IconName` was empty but at least one pixmap passed bounds-checking.
    Pixmap,
    /// Neither source is usable.
    None,
}

/// Which icon source to use, and whether a pixmap decode is even necessary (ADR-0031's
/// IconName-preference decision) -- kept separate from the pixmap *value* so this stays a
/// cheap, pure decision; the caller re-derives the actual largest pixmap via
/// [`largest_valid_pixmap`] only when this returns [`IconSource::Pixmap`].
pub(super) fn resolve_icon_source(icon_name: &str, pixmaps: &[IconPixmap]) -> IconSource {
    if !icon_name.is_empty() {
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
/// `/dev/shm/oblisk-$UID/tray/{sanitized_unique_name}.png` ([`shm_icons::write_png`]), creating the
/// directory tree if missing. Same path overwritten in place on every call -- no cache-busting
/// (ADR-0031).
pub(super) fn write_icon_png(sanitized_unique_name: &str, pixmap: &IconPixmap) -> std::io::Result<String> {
    let png_bytes = encode_argb32_to_png(pixmap.width as u32, pixmap.height as u32, &pixmap.bytes)
        .map_err(std::io::Error::other)?;
    shm_icons::write_png("tray", &format!("{sanitized_unique_name}.png"), &png_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(resolve_icon_source("battery-full", &pixmaps), IconSource::Name("battery-full".to_string()));
    }

    #[test]
    fn resolve_icon_source_falls_back_to_pixmap_when_icon_name_is_empty() {
        let pixmaps = vec![pixmap(32)];
        assert_eq!(resolve_icon_source("", &pixmaps), IconSource::Pixmap);
    }

    #[test]
    fn resolve_icon_source_is_none_when_neither_is_usable() {
        assert_eq!(resolve_icon_source("", &[]), IconSource::None);
        let invalid = vec![IconPixmap { width: 4, height: 2, bytes: vec![0u8; 32] }];
        assert_eq!(resolve_icon_source("", &invalid), IconSource::None);
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
