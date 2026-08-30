//! Decodes a file into a GPU texture, caches it, and fits it into a box (docs/adr/0054,
//! build-steps.md Phase 29 items 1 and 2).
//!
//! PNG and JPEG decode through femtovg's `Canvas::load_image_file`. SVG decodes through `resvg`,
//! needed because Adwaita ships scalable SVG icons.
//!
//! The cache key is the path, and for SVG only, the rasterized pixel size: a raster file has one
//! decode regardless of the box it lands in, but a vector rasterized for a 12px box would be
//! served blurry to a 24px box under a path-only key. The key also carries the file's mtime and
//! length (docs/adr/0031), so a producer that overwrites a path in place -- the tray does -- gets
//! a fresh texture rather than the one it wrote last time.
//!
//! Failures are cached too, as `None`, so an unreadable file or `.svgz` (see [`rasterize_svg`])
//! isn't retried every frame from inside `layout::paint`'s draw loop. A *missing* file is the one
//! failure that still retries, because its key changes the moment it appears.

pub mod icons;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use femtovg::renderer::OpenGl;
use femtovg::rgb::FromSlice;
use femtovg::{Canvas, ImageFlags, ImageId, ImageSource};

use crate::text::snap::LogicalRect;

/// Entries, not bytes.
///
/// ponytail: oldest-out, which is FIFO rather than the LRU `oblisk-supervisor-services-dbus.md`
/// § 9.2 asks for, so a wallpaper loaded once at startup is evicted before a tray icon loaded
/// forty times. FIFO is a `VecDeque` and a counter; LRU needs a touch on every hit and either a
/// dependency or an intrusive list. The eviction that actually matters is by bytes rather than by
/// count (docs/adr/0043's budget: one 4K wallpaper is 32 MB and 127 tray icons are not), and
/// neither is worth building before there is a cache to measure.
const CACHE_CAPACITY: usize = 128;

/// One cache slot. `raster_px` is the longest edge the SVG was rasterized for, or `0` for a file
/// decoded at its own native size (see this module's doc comment).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheKey {
    path: PathBuf,
    raster_px: u32,
    version: FileVersion,
}

/// What tells one revision of a file from the next, at a path that keeps its name (docs/adr/0031's
/// deferred item: the tray spools every icon update over the same
/// `/dev/shm/oblisk-$UID/tray/{name}.png`, no revision suffix).
///
/// Mtime and length together, not a content hash: tmpfs mtime is nanosecond-precise, length is
/// free, and hashing would mean reading the file to decide whether to read the file.
///
/// A file that cannot be stat'd takes the default, which is what makes a *missing* file retry
/// instead of staying negatively cached forever: once it appears, its key changes.
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

/// How an image fills the box layout gave it (docs/adr/0055 decision 3).
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

/// Path-and-size to uploaded texture, for one generation (`CONTEXT.md`, **Image cache**).
///
/// Not shared with the Supervisor and not persisted: the Renderer is swapped as an OS process on
/// every reload, so this is cold again after each config edit (docs/adr/0054).
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
        ImageCache {
            entries: HashMap::new(),
            order: VecDeque::new(),
            evicted: Vec::new(),
        }
    }

    /// Frees the textures evicted during the previous frame. `layout::paint::paint_tree` calls this
    /// before it walks anything.
    ///
    /// Cannot happen where the eviction does: femtovg batches a frame's draw calls and resolves an
    /// `ImageId` to a texture at `flush`, not at `fill_path`, so deleting mid-walk unbinds a
    /// texture an already-recorded command still names -- and femtovg answers a missing id with
    /// default paint parameters rather than a failure, so the symptom is a silently blank image.
    pub fn release_evicted(&mut self, canvas: &mut Canvas<OpenGl>) {
        for id in self.evicted.drain(..) {
            canvas.delete_image(id);
        }
    }

    /// The uploaded texture for `path`, decoding and uploading on the first ask. `box_px` is the
    /// longest edge of the box this will be drawn into, in physical pixels, and is what an SVG
    /// rasterizes against; a raster file ignores it.
    ///
    /// `None` for anything that did not decode, logged once rather than once per frame. The canvas
    /// must be current on the calling thread, which since docs/adr/0039 is the only thread that
    /// paints.
    ///
    /// Stats the file on every call, including a hit, because the key carries the file's revision
    /// (see [`FileVersion`]). That is one `stat` per image node per frame -- six hundred a second
    /// for ten icons at 60Hz against tmpfs -- and is the cheapest correct answer: the alternative
    /// to asking whether the bytes changed is re-reading them to find out.
    ///
    /// ponytail: a miss reads the file, and for an SVG rasterizes it, inside the frame. That thread
    /// is also the Wayland dispatch thread and the one the config VM runs on (docs/adr/0039), so a
    /// cold `list` of thirty tray icons on a cold page cache is thirty `open`/`read` pairs plus
    /// thirty resvg renders before the first `swap_buffers`. Steady state after that is one hash
    /// lookup per node per frame, which is why this is a startup and reload cost rather than a
    /// per-frame one. The upgrade path is the shape `text::shaping` already has: hand the path and
    /// the size to a worker, return `None` for this frame, and mark the scene dirty when the upload
    /// is ready (docs/adr/0044 decision 2). That is also
    /// `oblisk-supervisor-services-dbus.md` § 9.2's "off-thread", met on this side of the process
    /// boundary. Not built now because nothing has measured a dropped frame from it.
    pub fn image(&mut self, canvas: &mut Canvas<OpenGl>, path: &Path, box_px: u32) -> Option<ImageId> {
        let key = CacheKey {
            path: path.to_path_buf(),
            raster_px: if is_vector(path) { box_px.max(1) } else { 0 },
            version: FileVersion::read(path),
        };
        if let Some(cached) = self.entries.get(&key) {
            return *cached;
        }
        let loaded = match load(canvas, &key.path, key.raster_px) {
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
    /// texture for [`ImageCache::release_evicted`] rather than deleting it here: femtovg keys its
    /// own image store by `ImageId` and frees nothing until told to, so losing the id leaks the GPU
    /// allocation for the life of the process.
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

fn load(canvas: &mut Canvas<OpenGl>, path: &Path, raster_px: u32) -> Result<ImageId, String> {
    if raster_px == 0 {
        return canvas.load_image_file(path, ImageFlags::empty()).map_err(|err| err.to_string());
    }
    let (pixels, width, height) = rasterize_svg(path, raster_px)?;
    // `PREMULTIPLIED` because tiny-skia's `Pixmap` is premultiplied RGBA8 and femtovg samples a
    // texture without this flag as straight alpha -- getting it wrong shows as a dark halo around
    // every anti-aliased icon edge rather than as an outright failure.
    let source = ImageSource::from(femtovg::imgref::Img::new(pixels.as_rgba(), width as usize, height as usize));
    canvas.create_image(source, ImageFlags::PREMULTIPLIED).map_err(|err| err.to_string())
}

/// Rasterizes at `box_px` on the longest edge, preserving the aspect ratio; [`fitted_rect`] does
/// the rest.
///
/// ponytail: `resvg` is built with no default features, which drops SVG text rendering and gzipped
/// `.svgz`. Neither has a caller: `freedesktop-icons` only ever returns `.svg` and `.png`, and an
/// icon with a `<text>` element in it is rare enough that no theme this was tested against has
/// one. A config passing an absolute `.svgz` path gets one log line and a blank box. The upgrade
/// is two feature flags (`resvg/svgz`, `resvg/text`), and `text` costs a second `fontdb` that
/// would then disagree with the one `text::shaping` already loaded the declared font chain into.
fn rasterize_svg(path: &Path, box_px: u32) -> Result<(Vec<u8>, u32, u32), String> {
    let data = std::fs::read(path).map_err(|err| err.to_string())?;
    let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).map_err(|err| err.to_string())?;
    let size = tree.size();
    let longest = size.width().max(size.height());
    // `is_finite` as well as the sign: a NaN here would sail through a bare `<= 0.0` and produce
    // a NaN `scale`, then a zero-sized pixmap allocation, which is a worse error message than this
    // one at a further remove from its cause.
    if !longest.is_finite() || longest <= 0.0 {
        return Err(format!("svg declares a {}x{} viewport", size.width(), size.height()));
    }
    let scale = box_px as f32 / longest;
    let width = ((size.width() * scale).round() as u32).max(1);
    let height = ((size.height() * scale).round() as u32).max(1);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height).ok_or_else(|| format!("no pixmap for {width}x{height}"))?;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    Ok((pixmap.take(), width, height))
}

/// Where the image itself lands inside `box_rect`, given its own pixel dimensions and a [`Fit`].
///
/// Returns the rect the *image* occupies, which for `Cover` is deliberately larger than
/// `box_rect`: `layout::paint` has already pushed a scissor for the node's box, so the overflow is
/// cropped by the clip rather than by arithmetic here. femtovg clamps to the edge outside a
/// paint's extent unless `REPEAT_X`/`REPEAT_Y` are set, so a smaller extent would smear the
/// image's edge pixels rather than leave a gap -- `Contain` returns the smaller rect instead.
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
        LogicalRect {
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 50.0,
        }
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
        // Exercises resvg end to end against the file `dev-config` actually ships (docs/adr/0055):
        // a tree that parses to nothing renders a fully transparent pixmap rather than an error.
        let svg = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/oblisk/wallpaper.svg");
        let (pixels, width, height) = rasterize_svg(&svg, 128).expect("the shipped wallpaper should parse");
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
