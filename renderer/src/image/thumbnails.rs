//! The freedesktop thumbnail cache, read and written (ADR-0122). `file:///a/b.jpg` maps to
//! `$XDG_CACHE_HOME/thumbnails/<size>/<md5 of URI>.png`, with longest edge `normal` (128), `large`
//! (256), `x-large` (512), or `xx-large` (1024), plus source URI and mtime in `tEXt`. Nautilus,
//! Thunar, GTK file choosers, and this picker share it, so either side can open an already-seen
//! folder without decoding again.
//!
//! Implemented: layout, URI hashing, `Thumb::URI`/`Thumb::MTime` write and mtime read check,
//! temp-file-and-rename with `0600` files and `0700` dirs. Not implemented: `fail/` (a decode miss
//! is `Slot::Failed` for this generation), `Thumb::Size`, or `/usr/share/thumbnails` lookup.

use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};

/// Spec sizes, smallest first, as (directory name, longest edge).
const SIZES: [(&str, u32); 4] = [("normal", 128), ("large", 256), ("x-large", 512), ("xx-large", 1024)];

/// Smallest spec size covering `box_px`'s longer edge, or `None` for a wallpaper-sized box.
pub fn size_for(box_px: (u32, u32)) -> Option<(&'static str, u32)> {
    let longest = box_px.0.max(box_px.1);
    SIZES.iter().copied().find(|(_, px)| *px >= longest)
}

/// `$XDG_CACHE_HOME`, then `$HOME/.cache`, else nothing. Without it every decode is full.
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(xdg));
    }
    std::env::var_os("HOME").filter(|value| !value.is_empty()).map(|home| PathBuf::from(home).join(".cache"))
}

/// `file://` plus GLib `g_filename_to_uri` escaping, matching existing thumbnail hashes:
/// alphanumerics, `-_.!~*'()` and `/:@&=+$,` pass; spaces, `#`, `%`, `?`, and bytes over 127 become
/// uppercase `%XX`. Escape bytes, not chars, so UTF-8 names hash by their bytes.
pub fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        let keep = byte.is_ascii_alphanumeric() || b"-_.!~*'()/:@&=+$,".contains(byte);
        if keep {
            uri.push(*byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// The `dir`-size thumbnail path for `uri` under `cache_root`.
pub fn thumbnail_path(cache_root: &Path, dir: &str, uri: &str) -> PathBuf {
    let digest = Md5::digest(uri.as_bytes());
    let mut name = String::with_capacity(36);
    for byte in digest {
        name.push_str(&format!("{byte:02x}"));
    }
    name.push_str(".png");
    cache_root.join("thumbnails").join(dir).join(name)
}

/// One source file's slot: path, freshness metadata, and longest edge.
pub struct Slot {
    path: PathBuf,
    uri: String,
    mtime_secs: i64,
    /// The longest edge the thumbnail is made at.
    pub px: u32,
}

impl Slot {
    /// The slot for `source` at the spec size covering `box_px`, or `None` when the box is past
    /// the largest size or the source cannot be stat'd (there is no mtime to validate against).
    pub fn for_file(cache_root: &Path, source: &Path, box_px: (u32, u32)) -> Option<Slot> {
        let (dir, px) = size_for(box_px)?;
        let modified = std::fs::metadata(source).ok()?.modified().ok()?;
        let mtime_secs = modified.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
        let uri = file_uri(source);
        Some(Slot { path: thumbnail_path(cache_root, dir, &uri), uri, mtime_secs, px })
    }

    /// Straight RGBA8 pixels if `Thumb::MTime` matches the source. Missing mtime is untrusted per
    /// spec. `png` reads text and `image` reads pixels, one open each; a 256px PNG is too small to
    /// justify a second decoder.
    pub fn read_valid(&self) -> Option<(Vec<u8>, u32, u32)> {
        let file = std::fs::File::open(&self.path).ok()?;
        let reader = png::Decoder::new(BufReader::new(file)).read_info().ok()?;
        let text = &reader.info().uncompressed_latin1_text;
        let mtime = text.iter().find(|chunk| chunk.keyword == "Thumb::MTime")?;
        if mtime.text.trim().parse::<i64>().ok()? != self.mtime_secs {
            return None;
        }
        // Another `Thumb::URI` means a hash collision or copied cache. Missing URI is allowed when
        // mtime matches.
        if text.iter().any(|chunk| chunk.keyword == "Thumb::URI" && chunk.text != self.uri) {
            return None;
        }
        drop(reader);
        // Under the same ceiling as any other decode. These files are ours, but they live in a
        // shared `$XDG_CACHE_HOME` any process of this user can write, so their headers are not
        // evidence of their size (`image::MAX_DECODE_EDGE`).
        let decoded = super::decode_within_limits(&self.path).ok()?.into_rgba8();
        let (width, height) = decoded.dimensions();
        Ok::<_, ()>((decoded.into_raw(), width, height)).ok()
    }

    /// Writes straight-alpha `rgba` as the spec requires: `0700` dir, `0600` temp beside the final
    /// file, then rename, so readers never see a partial PNG.
    pub fn write(&self, rgba: &[u8], width: u32, height: u32) -> io::Result<()> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let dir = self.path.parent().ok_or_else(|| io::Error::other("thumbnail path has no parent"))?;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let temp = dir.join(format!(".oblisk-{}-{}.png.tmp", std::process::id(), self.mtime_secs));
        let result = (|| -> io::Result<()> {
            let file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
            let mut encoder = png::Encoder::new(io::BufWriter::new(file), width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.add_text_chunk("Thumb::URI".to_string(), self.uri.clone()).map_err(io::Error::other)?;
            encoder
                .add_text_chunk("Thumb::MTime".to_string(), self.mtime_secs.to_string())
                .map_err(io::Error::other)?;
            encoder.add_text_chunk("Software".to_string(), "Oblisk".to_string()).map_err(io::Error::other)?;
            let mut writer = encoder.write_header().map_err(io::Error::other)?;
            writer.write_image_data(rgba).map_err(io::Error::other)?;
            writer.finish().map_err(io::Error::other)?;
            std::fs::rename(&temp, &self.path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_specs_own_example_hashes_to_the_name_it_gives() {
        // Section "Thumbnail Naming" of the spec: file:///home/jens/photos/me.png.
        let path = thumbnail_path(Path::new("/c"), "normal", "file:///home/jens/photos/me.png");
        assert_eq!(path, PathBuf::from("/c/thumbnails/normal/c6ee772d9e49320e97ec29a7eb5b1697.png"));
    }

    #[test]
    fn a_file_uri_escapes_what_glib_escapes_and_keeps_what_it_keeps() {
        assert_eq!(file_uri(Path::new("/a/b.png")), "file:///a/b.png");
        assert_eq!(file_uri(Path::new("/a/my wall #2 (v1)'s.png")), "file:///a/my%20wall%20%232%20(v1)'s.png");
        assert_eq!(file_uri(Path::new("/a/é.png")), "file:///a/%C3%A9.png");
        assert_eq!(file_uri(Path::new("/a/x,y:z@w=v+u$t.png")), "file:///a/x,y:z@w=v+u$t.png");
    }

    #[test]
    fn the_size_is_the_smallest_that_covers_the_box_and_a_wallpaper_gets_none() {
        assert_eq!(size_for((24, 24)), Some(("normal", 128)));
        assert_eq!(size_for((230, 130)), Some(("large", 256)));
        assert_eq!(size_for((256, 10)), Some(("large", 256)));
        assert_eq!(size_for((257, 10)), Some(("x-large", 512)));
        assert_eq!(size_for((1024, 1024)), Some(("xx-large", 1024)));
        assert_eq!(size_for((1920, 1200)), None);
    }

    #[test]
    fn a_written_thumbnail_reads_back_while_the_source_is_unchanged_and_not_after_it_moves_on() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("wall.png");
        ::image::RgbaImage::from_pixel(4, 2, ::image::Rgba([1, 2, 3, 255])).save(&source).unwrap();
        let cache = dir.path().join("cache");

        let slot = Slot::for_file(&cache, &source, (100, 100)).unwrap();
        assert_eq!(slot.px, 128);
        assert!(slot.read_valid().is_none(), "nothing written yet");

        let pixels = vec![9u8, 8, 7, 255, 6, 5, 4, 255];
        slot.write(&pixels, 2, 1).unwrap();
        assert!(slot.path().starts_with(cache.join("thumbnails/normal")));
        let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(slot.path()).unwrap().permissions());
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(slot.read_valid(), Some((pixels.clone(), 2, 1)));

        // A later source mtime makes the thumbnail stale.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        ::image::RgbaImage::from_pixel(4, 2, ::image::Rgba([1, 2, 3, 255])).save(&source).unwrap();
        let later = Slot::for_file(&cache, &source, (100, 100)).unwrap();
        assert!(later.read_valid().is_none());
    }

    #[test]
    fn a_thumbnail_naming_another_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("wall.png");
        let other = dir.path().join("other.png");
        ::image::RgbaImage::from_pixel(4, 2, ::image::Rgba([1, 2, 3, 255])).save(&source).unwrap();
        std::fs::copy(&source, &other).unwrap();
        let cache = dir.path().join("cache");
        let slot = Slot::for_file(&cache, &source, (100, 100)).unwrap();
        slot.write(&[9, 8, 7, 255], 1, 1).unwrap();
        // Same bytes and mtime, another name: copy it to the other's thumbnail path.
        let other_slot = Slot::for_file(&cache, &other, (100, 100)).unwrap();
        std::fs::create_dir_all(other_slot.path().parent().unwrap()).unwrap();
        std::fs::copy(slot.path(), other_slot.path()).unwrap();
        assert!(other_slot.read_valid().is_none(), "the URI inside names `wall.png`");
    }
}
