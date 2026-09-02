//! Decodes a file into a GPU texture, caches it, and fits it into a box (ADR-0054). PNG and JPEG
//! decode through the `image` crate ([`decode_raster`]); SVG decodes through `resvg`, needed
//! because Adwaita ships scalable SVG icons.
//!
//! The cache key is the path plus, for SVG only, the rasterized pixel size: a raster file decodes
//! once regardless of the box, but a vector rasterized for a 12px box would look blurry served to
//! a 24px box under a path-only key. It also carries the file's mtime and length (ADR-0031), so
//! the tray overwriting a path in place still gets a fresh texture. Failures are cached too, as
//! `None`, so an unreadable file or `.svgz` (see [`rasterize_svg`]) isn't retried every frame from
//! `layout::paint`'s draw loop; a *missing* file still retries since its key changes on appearing.

pub mod icons;

use crate::layout::node::Rgba;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use femtovg::renderer::OpenGl;
use femtovg::rgb::FromSlice;
use femtovg::{Canvas, ErrorKind, ImageFlags, ImageId, ImageSource};

use crate::text::snap::LogicalRect;

/// Entries, not bytes. ponytail: oldest-out FIFO, not the LRU
/// `oblisk-supervisor-services-dbus.md` § 9.2 asks for, so a wallpaper loaded once at startup is
/// evicted before a tray icon loaded forty times. Evicting by bytes (ADR-0043's budget: one 4K
/// wallpaper is 32 MB, 127 tray icons are not) matters more than LRU, and neither is worth
/// building before there is a cache to measure.
const CACHE_CAPACITY: usize = 128;

/// One cache slot. `raster_px` is the longest edge the SVG was rasterized for, or `0` for a file
/// decoded at its own native size (see this module's doc comment).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheKey {
    path: PathBuf,
    raster_px: u32,
    version: FileVersion,
    /// The `currentColor` value this texture was rasterized with, packed `0x00RRGGBB` (ADR-0072).
    /// `None` for a raster file and an untinted SVG. Part of the key because one theme file drawn
    /// white on the bar and dim in a popup is two textures, else the first tint wins for good.
    tint: Option<u32>,
}

/// What tells one revision of a file from the next, at a path that keeps its name (ADR-0031's
/// deferred item: the tray spools every icon update over `/dev/shm/oblisk-$UID/tray/{name}.png`,
/// no revision suffix). Mtime and length, not a content hash: tmpfs mtime is nanosecond-precise,
/// length is free, and hashing would mean reading the file to decide whether to read it. A file
/// that cannot be stat'd takes the default, so a *missing* file retries instead of staying cached
/// negative forever.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
struct FileVersion {
    mtime_secs: i64,
    mtime_nanos: i64,
    len: u64,
}

impl FileVersion {
    fn read(path: &Path) -> Self {
        let Ok(metadata) = std::fs::metadata(path) else {
            return FileVersion::default();
        };
        let modified = metadata.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok());
        FileVersion {
            mtime_secs: modified.map(|d| d.as_secs() as i64).unwrap_or(-1),
            mtime_nanos: modified.map(|d| i64::from(d.subsec_nanos())).unwrap_or(-1),
            len: metadata.len(),
        }
    }
}

/// How an image fills the box layout gave it (ADR-0055 decision 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Fit {
    /// Scales to cover the box and crops the overflow. The default because it is the only one of
    /// the three that cannot leave a wallpaper with bars down the side of a screen.
    #[default]
    Cover,
    /// Scales to fit inside the box, leaving the remainder unpainted.
    Contain,
    /// Ignores the aspect ratio.
    Stretch,
}

impl Fit {
    /// `"cover"`, `"contain"`, `"stretch"`, anything else `None`. Callers turn that into the
    /// `LayoutError` their own property name needs.
    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "cover" => Some(Fit::Cover),
            "contain" => Some(Fit::Contain),
            "stretch" => Some(Fit::Stretch),
            _ => None,
        }
    }
}

/// Path-and-size to uploaded texture, for one generation (`CONTEXT.md`, **Image cache**). Not
/// shared with the Supervisor and not persisted: the Renderer is swapped as an OS process on
/// every reload, so this is cold again after each config edit (ADR-0054).
pub struct ImageCache {
    entries: HashMap<CacheKey, Option<ImageId>>,
    order: VecDeque<CacheKey>,
    /// Evicted since the last [`ImageCache::release_evicted`], not yet freed.
    evicted: Vec<ImageId>,
}

impl Default for ImageCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageCache {
    pub fn new() -> Self {
        ImageCache { entries: HashMap::new(), order: VecDeque::new(), evicted: Vec::new() }
    }

    /// Frees textures evicted last frame; `layout::paint::paint_tree` calls this before walking
    /// anything, since femtovg resolves an `ImageId` to a texture at `flush`, not `fill_path`, so
    /// deleting mid-walk unbinds a texture a recorded command still names, drawing silently blank.
    pub fn release_evicted(&mut self, canvas: &mut Canvas<OpenGl>) {
        for id in self.evicted.drain(..) {
            canvas.delete_image(id);
        }
    }

    /// The uploaded texture for `path`, decoding and uploading on the first ask. `box_px`, the
    /// longest edge of the box in physical pixels, is what an SVG rasterizes against; a raster
    /// file ignores it. `None` for anything that did not decode, logged once, not once per frame.
    /// The canvas must be current on the calling thread, the only one that paints since ADR-0039.
    /// Stats the file on every call, including a hit, since the key carries its revision (see
    /// [`FileVersion`]): one `stat` per node per frame, six hundred a second for ten icons at 60Hz.
    ///
    /// ponytail: a miss reads the file, and for an SVG rasterizes it, inside the frame, which is
    /// also the Wayland dispatch thread and the config VM's thread (ADR-0039). Upgrade path: the
    /// shape `text::shaping` already has, a worker plus a dirty flag on upload (ADR-0044 decision
    /// 2), meeting `oblisk-supervisor-services-dbus.md` § 9.2's "off-thread".
    pub fn image(
        &mut self,
        canvas: &mut Canvas<OpenGl>,
        path: &Path,
        box_px: u32,
        tint: Option<Rgba>,
    ) -> Option<ImageId> {
        let vector = is_vector(path);
        let key = CacheKey {
            path: path.to_path_buf(),
            raster_px: if vector { box_px.max(1) } else { 0 },
            version: FileVersion::read(path),
            // Only a vector can carry `currentColor`; a PNG's tint is dropped rather than
            // splitting its cache slot per colour it will never use.
            tint: if vector { tint.map(packed_rgb) } else { None },
        };
        if let Some(cached) = self.entries.get(&key) {
            return *cached;
        }
        let loaded = match load(canvas, &key.path, key.raster_px, tint) {
            Ok(id) => Some(id),
            Err(err) => {
                eprintln!("[oblisk-renderer] image: {}: {err}", key.path.display());
                None
            }
        };
        self.insert(key, loaded);
        loaded
    }

    /// Evicts before inserting, so the map never exceeds [`CACHE_CAPACITY`]. Queues the evicted
    /// texture for [`ImageCache::release_evicted`] rather than deleting it here: femtovg frees
    /// nothing by `ImageId` until told to, so losing the id leaks the GPU allocation for good.
    fn insert(&mut self, key: CacheKey, value: Option<ImageId>) {
        while self.order.len() >= CACHE_CAPACITY {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(Some(id)) = self.entries.remove(&oldest) {
                self.evicted.push(id);
            }
        }
        self.order.push_back(key.clone());
        self.entries.insert(key, value);
    }
}

/// By extension, not by sniffing the file: the producers here all name their files
/// (`freedesktop-icons` returns `.svg` or `.png`, `shm_icons.rs` writes `.png`).
fn is_vector(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("svg"))
}

fn load(canvas: &mut Canvas<OpenGl>, path: &Path, raster_px: u32, tint: Option<Rgba>) -> Result<ImageId, String> {
    if raster_px == 0 {
        let (pixels, width, height) = decode_raster(path)?;
        // Straight alpha, what the `image` crate produces, so no flag: `PREMULTIPLIED` below is
        // the SVG path's answer to tiny-skia, not a house default.
        let source = ImageSource::from(femtovg::imgref::Img::new(pixels.as_rgba(), width as usize, height as usize));
        return canvas.create_image(source, ImageFlags::empty()).map_err(femtovg_error);
    }
    let (pixels, width, height) = rasterize_svg(path, raster_px, tint)?;
    // `PREMULTIPLIED` because tiny-skia's `Pixmap` is premultiplied RGBA8 and femtovg samples a
    // texture without this flag as straight alpha: getting it wrong shows as a dark halo around
    // every anti-aliased icon edge, not an outright failure.
    let source = ImageSource::from(femtovg::imgref::Img::new(pixels.as_rgba(), width as usize, height as usize));
    canvas.create_image(source, ImageFlags::PREMULTIPLIED).map_err(femtovg_error)
}

/// femtovg's `ErrorKind` writes the literal string `"canvas error"` for every one of its
/// sixteen variants, so `Debug` is the only thing that names which failure happened.
fn femtovg_error(err: ErrorKind) -> String {
    format!("{err:?}")
}

/// Decodes a PNG or JPEG to straight-alpha RGBA8, the reason `image` is a direct dependency
/// (`renderer/Cargo.toml`). femtovg's `Canvas::load_image_file` cannot decode anything: it
/// declares `image` with `default-features = false` and no format, so every PNG came back
/// `Unsupported(Exact(Png))`, taking the tray's and notification daemon's spooled pixmaps
/// (ADR-0031) with it. `into_rgba8` also covers the grayscale-plus-alpha and 16-bit variants
/// femtovg's own conversion refuses, at no cost since every theme icon already decodes to RGBA8.
fn decode_raster(path: &Path) -> Result<(Vec<u8>, u32, u32), String> {
    let decoded = ::image::open(path).map_err(|err| err.to_string())?;
    let rgba = decoded.into_rgba8();
    let (width, height) = rgba.dimensions();
    Ok((rgba.into_raw(), width, height))
}

/// Rasterizes at `box_px` on the longest edge, preserving the aspect ratio; [`fitted_rect`] does
/// the rest. ponytail: `resvg` has no default features, dropping SVG text rendering and gzipped
/// `.svgz`. A config passing an absolute `.svgz` path gets one log line and a blank box. The
/// upgrade is two feature flags (`resvg/svgz`, `resvg/text`), and `text` costs a second `fontdb`
/// that would disagree with the one `text::shaping` already loaded the declared font chain into.
fn rasterize_svg(path: &Path, box_px: u32, tint: Option<Rgba>) -> Result<(Vec<u8>, u32, u32), String> {
    let data = std::fs::read(path).map_err(|err| err.to_string())?;
    let data = match tint {
        Some(tint) => tinted_svg(&data, tint),
        None => data,
    };
    let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).map_err(|err| err.to_string())?;
    let size = tree.size();
    let longest = size.width().max(size.height());
    // `is_finite` as well as the sign: a bare `<= 0.0` lets NaN through to a NaN `scale` and a
    // zero-sized pixmap allocation, a worse error message further from its cause.
    if !longest.is_finite() || longest <= 0.0 {
        return Err(format!("svg declares a {}x{} viewport", size.width(), size.height()));
    }
    let scale = box_px as f32 / longest;
    let width = ((size.width() * scale).round() as u32).max(1);
    let height = ((size.height() * scale).round() as u32).max(1);
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(width, height).ok_or_else(|| format!("no pixmap for {width}x{height}"))?;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    Ok((pixmap.take(), width, height))
}

/// `0x00RRGGBB` from a parsed colour, for [`CacheKey`]. Alpha is dropped: a CSS `color` is
/// `#RRGGBB`, and an icon's transparency is the draw call's `alpha`, not the SVG's.
fn packed_rgb(color: Rgba) -> u32 {
    let channel = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    (channel(color.r) << 16) | (channel(color.g) << 8) | channel(color.b)
}

/// `#RRGGBB` for a parsed colour.
fn hex_rgb(color: Rgba) -> String {
    format!("#{:06x}", packed_rgb(color))
}

/// `data` with every `currentColor` resolved to `tint`, or untouched with none (ADR-0072). Two
/// rewrites, since symbolic icons come in two shapes a theme mixes freely. The stylesheet one,
/// what Breeze and Adwaita ship, is a `<style id="current-color-scheme">` block setting
/// `color:#232629`, Breeze *Light*'s text colour, on a class every path carries; Plasma rewrites
/// it at load time rather than reading it, and so does this (shipped as-is, the icon draws
/// near-black on a dark bar). The root-attribute one covers `fill="currentColor"` with no `color`
/// defined, where CSS's initial value is black; a presentation attribute on the root beats that,
/// so a file that does define `color` keeps it and is rewritten by the first pass. Byte-level, not
/// a parse: `usvg` resolves `currentColor` while building the tree with no hook before that.
/// `str::from_utf8`, not `from_utf8_lossy`, hands a non-UTF-8 file back verbatim for `usvg` to
/// reject with its own message.
fn tinted_svg(data: &[u8], tint: Rgba) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(data) else {
        return data.to_vec();
    };
    if !text.contains("currentColor") {
        return data.to_vec();
    }
    let hex = hex_rgb(tint);
    let rewritten = rewrite_color_declarations(text, &hex);
    match rewritten.find("<svg") {
        Some(at) => {
            let mut out = String::with_capacity(rewritten.len() + hex.len() + 10);
            out.push_str(&rewritten[..at + 4]);
            out.push_str(&format!(" color=\"{hex}\""));
            out.push_str(&rewritten[at + 4..]);
            out.into_bytes()
        }
        None => rewritten.into_bytes(),
    }
}

/// Every CSS `color:` declaration in `text` repointed at `hex`. Only a bare `color`, never
/// `stop-color`, `flood-color` or `lighting-color`: those name a specific paint rather than the
/// value `currentColor` reads, and rewriting them would flatten a gradient. The guard is the
/// character before the match, which must not continue an identifier. ponytail: the match is
/// textual, so a `color:` inside an XML comment or attribute value would be rewritten too.
/// Upgrade path is a real CSS pass over the `<style>` body: a parser this crate does not want.
fn rewrite_color_declarations(text: &str, hex: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("color:") {
        let after = at + "color:".len();
        let continues_an_identifier =
            rest[..at].chars().next_back().is_some_and(|c| c == '-' || c == '_' || c.is_alphanumeric());
        out.push_str(&rest[..after]);
        if continues_an_identifier {
            rest = &rest[after..];
            continue;
        }
        // The value, up to whatever ends it, replaced whole so `color: #232629` and `color:#232629`
        // behave the same.
        let value_len = rest[after..].find([';', '}', '"', '\'']).unwrap_or(rest.len() - after);
        out.push_str(hex);
        rest = &rest[after + value_len..];
    }
    out.push_str(rest);
    out
}

/// Where the image itself lands inside `box_rect`, given its own pixel dimensions and a [`Fit`].
/// Returns the rect the *image* occupies, which for `Cover` is deliberately larger than
/// `box_rect`: `layout::paint` already pushed a scissor for the node's box, so the overflow is
/// cropped by the clip, not by arithmetic here. femtovg clamps to the edge outside a paint's
/// extent unless `REPEAT_X`/`REPEAT_Y` are set, so a smaller extent would smear the image's edge
/// pixels rather than leave a gap; `Contain` returns the smaller rect instead.
pub fn fitted_rect(box_rect: LogicalRect, image_width: f32, image_height: f32, fit: Fit) -> LogicalRect {
    if fit == Fit::Stretch || image_width <= 0.0 || image_height <= 0.0 {
        return box_rect;
    }
    let horizontal = box_rect.width / image_width;
    let vertical = box_rect.height / image_height;
    let scale = match fit {
        Fit::Cover => horizontal.max(vertical),
        Fit::Contain => horizontal.min(vertical),
        Fit::Stretch => unreachable!("returned above"),
    };
    let width = image_width * scale;
    let height = image_height * scale;
    LogicalRect {
        x: box_rect.x + (box_rect.width - width) / 2.0,
        y: box_rect.y + (box_rect.height - height) / 2.0,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_rect() -> LogicalRect {
        LogicalRect { x: 10.0, y: 20.0, width: 100.0, height: 50.0 }
    }

    #[test]
    fn stretch_takes_the_box_exactly() {
        assert_eq!(fitted_rect(box_rect(), 640.0, 480.0, Fit::Stretch), box_rect());
    }

    #[test]
    fn contain_fits_inside_and_centres() {
        let fitted = fitted_rect(box_rect(), 64.0, 64.0, Fit::Contain);
        assert_eq!((fitted.width, fitted.height), (50.0, 50.0));
        assert_eq!((fitted.x, fitted.y), (35.0, 20.0));
    }

    #[test]
    fn cover_overflows_the_box_rather_than_leaving_a_gap() {
        // `layout::paint`'s scissor crops the overflow, which is why this may return a rect
        // taller than the box it was given.
        let fitted = fitted_rect(box_rect(), 64.0, 64.0, Fit::Cover);
        assert_eq!((fitted.width, fitted.height), (100.0, 100.0));
        assert_eq!((fitted.x, fitted.y), (10.0, -5.0));
    }

    #[test]
    fn a_zero_sized_image_falls_back_to_the_box_instead_of_dividing_by_zero() {
        // The alternative is an inf scale and a NaN rect reaching femtovg.
        assert_eq!(fitted_rect(box_rect(), 0.0, 64.0, Fit::Cover), box_rect());
        assert_eq!(fitted_rect(box_rect(), 64.0, 0.0, Fit::Contain), box_rect());
    }

    #[test]
    fn a_kde_symbolic_icon_is_recoloured_through_its_own_stylesheet() {
        // The Telegram case. Breeze bakes Breeze *Light*'s text colour into the file and expects the
        // toolkit to rewrite it; drawn as shipped it is near-black on a dark bar.
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 22 22">
  <defs><style id="current-color-scheme" type="text/css">
      .ColorScheme-Text { color:#232629; }
  </style></defs>
  <path class="ColorScheme-Text" style="fill:currentColor" d="M0 0h1v1h-1z"/>
</svg>"##;
        let out = String::from_utf8(tinted_svg(svg, tint())).unwrap();
        assert!(out.contains("color:#cdd6f4"), "the stylesheet's own declaration is repointed: {out}");
        assert!(!out.contains("#232629"), "and the shipped colour is gone: {out}");
    }

    #[test]
    fn an_icon_that_defines_no_colour_gets_one_on_the_root() {
        // hicolor and Adwaita ship this shape. CSS's initial value for `color` is black, so without
        // the root attribute `currentColor` is black whatever the caller asked for.
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg"><path fill="currentColor" d="M0 0h1v1h-1z"/></svg>"##;
        let out = String::from_utf8(tinted_svg(svg, tint())).unwrap();
        assert!(out.starts_with(r##"<svg color="#cdd6f4""##), "the root carries the colour: {out}");
    }

    #[test]
    fn a_full_colour_icon_is_handed_back_byte_for_byte() {
        // Every app icon. The tray passes a `foreground` for all of them and only the symbolic ones
        // may change, or a themed Slack logo would come out flat.
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg"><path fill="#2eb67d" d="M0 0h1v1h-1z"/></svg>"##;
        assert_eq!(tinted_svg(svg, tint()), svg.to_vec());
    }

    #[test]
    fn a_gradient_stop_is_not_a_colour_declaration() {
        // `stop-color:` contains `color:`. Rewriting it would flatten every gradient in the file to
        // one colour, which is a worse bug than the one being fixed.
        let out = rewrite_color_declarations("stop-color:#ff0000;color:#232629;flood-color:#00ff00", "#cdd6f4");
        assert_eq!(out, "stop-color:#ff0000;color:#cdd6f4;flood-color:#00ff00");
    }

    #[test]
    fn a_spaced_declaration_is_replaced_whole_rather_than_prefixed() {
        // The value runs to the terminator, so `color: #232629 ` goes in one piece and the
        // whitespace inside it goes with it. CSS does not care, and the alternative is trimming
        // rules that would.
        assert_eq!(rewrite_color_declarations("{ color: #232629 }", "#cdd6f4"), "{ color:#cdd6f4}");
    }

    #[test]
    fn one_file_tinted_two_ways_is_two_cache_slots() {
        // Without the tint in the key the first colour drawn wins for the life of the process, so an
        // icon on the bar and the same icon dimmed in a popup would be one texture.
        let a = CacheKey {
            path: PathBuf::from("/x.svg"),
            raster_px: 18,
            version: FileVersion::default(),
            tint: Some(0xffffff),
        };
        let b = CacheKey { tint: Some(0x808080), ..a.clone() };
        assert_ne!(a, b);
    }

    #[test]
    fn a_tint_packs_to_rgb_and_drops_alpha() {
        assert_eq!(packed_rgb(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 0.25 }), 0xff0000);
        assert_eq!(hex_rgb(Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 }), "#0000ff");
    }

    /// `#cdd6f4`, the dev config's `theme.FG`, so a test asserting on the hex asserts on a value it
    /// can read.
    fn tint() -> Rgba {
        Rgba { r: 0xcd as f32 / 255.0, g: 0xd6 as f32 / 255.0, b: 0xf4 as f32 / 255.0, a: 1.0 }
    }

    #[test]
    fn only_svg_is_rasterized_by_size() {
        assert!(is_vector(Path::new("/usr/share/icons/Adwaita/symbolic/x.svg")));
        assert!(is_vector(Path::new("/tmp/X.SVG")));
        assert!(!is_vector(Path::new("/dev/shm/oblisk-1000/tray/telegram.png")));
        assert!(!is_vector(Path::new("/tmp/no-extension")));
        // `.svgz` is not handled (see `rasterize_svg`'s ponytail): treating it as a vector would
        // hand gzip bytes to the XML parser, so it takes the raster path and fails there instead.
        assert!(!is_vector(Path::new("/tmp/gzipped.svgz")));
    }

    #[test]
    fn the_shipped_wallpaper_rasterizes_to_opaque_pixels_at_the_size_asked_for() {
        // Exercises resvg end to end against the file `dev-config` actually ships (ADR-0055):
        // a tree that parses to nothing renders a fully transparent pixmap rather than an error.
        let svg = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/oblisk/wallpaper.svg");
        let (pixels, width, height) = rasterize_svg(&svg, 128, None).expect("the shipped wallpaper should parse");
        // 1920x1080 viewBox, longest edge 128, so the aspect ratio survives the scale.
        assert_eq!((width, height), (128, 72));
        assert_eq!(pixels.len(), (width * height * 4) as usize);
        let opaque = pixels.as_chunks::<4>().0.iter().filter(|px| px[3] > 0).count();
        assert_eq!(opaque, (width * height) as usize, "the wallpaper covers its whole viewBox");
        // Not a single flat colour: the gradient and obelisk both have to survive.
        let distinct: std::collections::HashSet<[u8; 3]> =
            pixels.as_chunks::<4>().0.iter().map(|px| [px[0], px[1], px[2]]).collect();
        assert!(distinct.len() > 16, "expected a gradient, got {} colours", distinct.len());
    }

    /// A 2x2 RGBA PNG encoded by Pillow, byte for byte. An independent encoder is the point: a
    /// fixture this crate wrote itself could not tell a working decoder from a round trip through
    /// a broken one.
    const PIL_2X2_RGBA_PNG: [u8; 80] = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
        0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x72, 0xb6, 0x0d, 0x24, 0x00, 0x00, 0x00,
        0x17, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x05, 0xc1, 0x01, 0x01, 0x00, 0x00, 0x00, 0x82, 0x20, 0xa6, 0xf7,
        0xdc, 0x40, 0x24, 0x43, 0xc1, 0x01, 0x3a, 0xdc, 0x05, 0x7c, 0xf2, 0x4a, 0x44, 0x5b, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn a_png_decodes_to_the_pixels_it_was_written_with() {
        // The regression this exists for is not a wrong pixel, it is no decoder at all: femtovg
        // pulls `image` with every format feature off, so before `decode_raster` this file, every
        // themed PNG icon and every tray pixmap (ADR-0031) failed with `Unsupported(Png)`.
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("fixture.png");
        std::fs::write(&png, PIL_2X2_RGBA_PNG).unwrap();

        let (pixels, width, height) = decode_raster(&png).expect("a PNG decoder must be compiled in");
        assert_eq!((width, height), (2, 2));
        // Straight alpha, in the order Pillow was handed them: the half-transparent green stays
        // 0x00ff00 rather than arriving premultiplied to 0x008000.
        assert_eq!(pixels, vec![255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 255, 0, 0, 0, 0]);
    }

    #[test]
    fn a_file_that_is_not_an_image_reports_why_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("not-really.png");
        std::fs::write(&fake, b"<svg/>").unwrap();
        assert!(decode_raster(&fake).is_err());
    }

    #[test]
    fn the_cache_stays_at_its_capacity_and_keeps_the_keys_it_kept() {
        // Negative entries only: `ImageId` has no public constructor.
        let mut cache = ImageCache::new();
        for n in 0..(CACHE_CAPACITY * 2) {
            cache.insert(key(&format!("/tmp/{n}.png"), 0, FileVersion::default()), None);
        }
        assert_eq!(cache.entries.len(), CACHE_CAPACITY);
        assert_eq!(cache.order.len(), CACHE_CAPACITY);
        let newest = key(&format!("/tmp/{}.png", CACHE_CAPACITY * 2 - 1), 0, FileVersion::default());
        let oldest = key("/tmp/0.png", 0, FileVersion::default());
        assert!(cache.entries.contains_key(&newest));
        assert!(!cache.entries.contains_key(&oldest));
    }

    fn key(path: &str, px: u32, version: FileVersion) -> CacheKey {
        CacheKey {
            path: PathBuf::from(path),
            raster_px: if is_vector(Path::new(path)) { px } else { 0 },
            version,
            tint: None,
        }
    }

    #[test]
    fn a_vector_and_a_raster_of_the_same_name_do_not_share_a_slot() {
        let v = FileVersion::default();
        assert_eq!(key("/tmp/a.png", 12, v), key("/tmp/a.png", 24, v));
        assert_ne!(key("/tmp/a.svg", 12, v), key("/tmp/a.svg", 24, v));
    }

    #[test]
    fn one_path_rewritten_in_place_is_a_different_slot() {
        let first = FileVersion { mtime_secs: 1_700_000_000, mtime_nanos: 0, len: 512 };
        let same_time_new_size = FileVersion { len: 640, ..first };
        let same_size_new_time = FileVersion { mtime_nanos: 1, ..first };
        assert_ne!(key("/dev/shm/x.png", 16, first), key("/dev/shm/x.png", 16, same_time_new_size));
        assert_ne!(key("/dev/shm/x.png", 16, first), key("/dev/shm/x.png", 16, same_size_new_time));
        assert_eq!(key("/dev/shm/x.png", 16, first), key("/dev/shm/x.png", 16, first));
    }

    #[test]
    fn a_missing_file_and_a_real_one_read_different_versions() {
        assert_eq!(FileVersion::read(Path::new("/nonexistent/oblisk-x.png")), FileVersion::default());
        let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/oblisk/wallpaper.svg");
        let version = FileVersion::read(&shipped);
        assert_ne!(version, FileVersion::default());
        assert!(version.len > 0);
    }

    #[test]
    fn fit_parses_the_three_spelled_modes_and_nothing_else() {
        assert_eq!(Fit::from_str("cover"), Some(Fit::Cover));
        assert_eq!(Fit::from_str("contain"), Some(Fit::Contain));
        assert_eq!(Fit::from_str("stretch"), Some(Fit::Stretch));
        assert_eq!(Fit::from_str("Cover"), None);
        assert_eq!(Fit::from_str("fill"), None);
        assert_eq!(Fit::default(), Fit::Cover);
    }
}
