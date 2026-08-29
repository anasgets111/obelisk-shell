//! Decoding a file into a GPU texture, caching it, and fitting it into a box (docs/adr/0054,
//! build-steps.md Phase 29 items 1 and 2).
//!
//! Everything here exists because nothing in this crate could draw a pixel from a file. `icon` was
//! a node kind that parsed its `size`, reserved that much layout space and painted nothing, and
//! there was no `image` kind at all, while `dbus/shm_icons.rs` had been spooling decoded tray and
//! notification icons to `/dev/shm` and `mpris` had been reporting an `album_art_path` for phases.
//! Three producers, no consumer.
//!
//! Two decoders, picked by extension. PNG and JPEG go through `Canvas::load_image_file`, which is
//! femtovg's own default `image-loading` feature and costs nothing new. SVG goes through `resvg`,
//! which is not optional: Adwaita ships scalable SVG, so an SVG-blind cache draws nothing on a
//! stock GNOME install.
//!
//! The cache key is the path *and* the pixel size, and only for SVG. A vector rasterized for a
//! 12px box is a different texture from the same file rasterized for a 24px box, so keying on the
//! path alone would serve the first one blurry to the second forever. A raster file has one
//! decode regardless of the box it lands in, so its key pins the size at 0 and every box shares
//! the single upload.
//!
//! The key also carries the file's modification time and length, so a producer that overwrites a
//! path in place gets a new texture rather than the one it wrote last time. The tray does exactly
//! that, deliberately (docs/adr/0031), and this is the consumer-side half that ADR deferred until
//! an icon-loading path existed to need it.
//!
//! Failures are cached too, as `None`. An unreadable file or an `.svgz` (see [`rasterize_svg`])
//! would otherwise be retried on every frame, and `layout::paint` logs from inside the frame loop
//! -- one bad path would be an unbounded log stream on the Wayland dispatch thread, which is the
//! failure mode `paint::log_paint_error`'s own ponytail comment already describes for paint
//! properties. A *missing* file is the one failure that retries, because its key changes the moment
//! it appears.

pub mod icons;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use femtovg::renderer::OpenGl;
use femtovg::rgb::FromSlice;
use femtovg::{Canvas, ImageFlags, ImageId, ImageSource};

use crate::text::snap::LogicalRect;

/// Entries, not bytes. A bar's icon set is a handful of files and never approaches this; a
/// long-lived notification list with distinct album art is the thing that would.
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

/// What tells one revision of a file from the next, at a path that keeps its name.
///
/// This is docs/adr/0031's deferred item coming due. That ADR had the tray spool every icon update
/// to the same `/dev/shm/oblisk-$UID/tray/{name}.png` with "no revision suffix, no cleanup logic",
/// and deferred the consumer-side fix as Speculative Generality on the explicit grounds that "the
/// renderer has no scene graph or icon-loading path at all as of this ADR", with the upgrade path
/// named as "Renderer-side texture cache-busting, only once the renderer's actual icon-loading
/// mechanism exists and is shown to need it". It exists now, and a path-only key means an app that
/// changes its tray icon (a badge, a mute toggle, a connection state) keeps the pixels it had at
/// first paint for the life of the Renderer process.
///
/// Modification time and length together rather than either alone: tmpfs carries nanosecond
/// timestamps so mtime alone is enough in practice, and length is free and covers a filesystem that
/// rounds. Not a content hash, which would mean reading the file to decide whether to read the file.
///
/// A file that cannot be stat'd takes the default, which is what makes a *missing* file retry
/// rather than stay negatively cached forever: once it appears, its key changes and the lookup is
/// new.
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
/// every reload, so this is cold again after each config edit. That is the one real cost of
/// docs/adr/0054 deciding the way it did, and it is the whole of it.
pub struct ImageCache {
    entries: HashMap<CacheKey, Option<ImageId>>,
    order: VecDeque<CacheKey>,
    /// Textures evicted since the last [`ImageCache::release_evicted`], not yet freed. See that
    /// method for why the deletion cannot happen where the eviction does.
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
    /// The deletion cannot happen where the eviction does. femtovg batches a frame's draw calls and
    /// resolves an `ImageId` to a texture at `flush`, not at `fill_path`, so deleting mid-walk
    /// unbinds a texture an already-recorded command still names. femtovg answers a missing id by
    /// returning default paint parameters rather than by failing, so the symptom would be one
    /// silently blank image per eviction, in a frame that drew more than [`CACHE_CAPACITY`] distinct
    /// images. Deferring to the next frame's start puts the deletion after that flush.
    pub fn release_evicted(&mut self, canvas: &mut Canvas<OpenGl>) {
        for id in self.evicted.drain(..) {
            canvas.delete_image(id);
        }
    }

    /// The uploaded texture for `path`, decoding and uploading on the first ask. `box_px` is the
    /// longest edge of the box this will be drawn into, in physical pixels, and is what an SVG
    /// rasterizes against; a raster file ignores it.
    ///
    /// `None` for anything that did not decode, logged once rather than once per frame (see this
    /// module's doc comment). The canvas must be current on the calling thread, which since
    /// docs/adr/0039 is the only thread that paints.
    ///
    /// Stats the file on every call, including a hit, because the key carries the file's revision
    /// (see [`FileVersion`]). That is one `stat` per image node per frame, which on a bar with ten
    /// icons at 60Hz is six hundred a second against tmpfs, and is the cheapest correct answer:
    /// the alternative to asking whether the bytes changed is re-reading them to find out.
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

    /// Evicts before inserting, so the map never exceeds [`CACHE_CAPACITY`] rather than exceeding
    /// it by one and trimming afterwards. The evicted texture is queued for
    /// [`ImageCache::release_evicted`] rather than deleted here, and it has to be queued rather than
    /// dropped: femtovg keys its own image store by `ImageId` and frees nothing until told to, so
    /// losing the id leaks the GPU allocation for the life of the process.
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

/// By extension, not by sniffing the file. The producers here all name their files
/// (`freedesktop-icons` returns `.svg` or `.png` and nothing else, `shm_icons.rs` writes `.png`),
/// so reading the first bytes of every candidate to learn what a suffix already says would be
/// paying for a case that does not arise.
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

/// Rasterizes at `box_px` on the longest edge, preserving the aspect ratio, so a wide SVG in a
/// square box comes out wide rather than stretched and [`fitted_rect`] does the rest.
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
/// cropped by the clip rather than by arithmetic here. Filling the box rect with a paint extent
/// smaller than it would smear the image's edge pixels across the remainder, since femtovg clamps
/// to the edge outside a paint's extent unless `REPEAT_X`/`REPEAT_Y` are set, so `Contain` returns
/// the smaller rect and the caller fills exactly that.
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
        // A square image in a 100x50 box: the height binds, so 50x50 centred horizontally.
        let fitted = fitted_rect(box_rect(), 64.0, 64.0, Fit::Contain);
        assert_eq!((fitted.width, fitted.height), (50.0, 50.0));
        assert_eq!((fitted.x, fitted.y), (35.0, 20.0));
    }

    #[test]
    fn cover_overflows_the_box_rather_than_leaving_a_gap() {
        // Same square image, same box: now the width binds, so 100x100 overflowing 25px above and
        // below. `layout::paint`'s scissor is what crops that, which is why this is allowed to
        // return a rect taller than the box it was given.
        let fitted = fitted_rect(box_rect(), 64.0, 64.0, Fit::Cover);
        assert_eq!((fitted.width, fitted.height), (100.0, 100.0));
        assert_eq!((fitted.x, fitted.y), (10.0, -5.0));
    }

    #[test]
    fn a_zero_sized_image_falls_back_to_the_box_instead_of_dividing_by_zero() {
        // `image_size` should never report this, but the alternative here is an inf scale and a
        // NaN rect reaching femtovg, which is the same class of silent-NaN-geometry bug
        // `parse_spacing`'s 1e300 test exists for.
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
        // The one test that exercises resvg end to end, against the file `dev-config` actually
        // ships (docs/adr/0055). Everything above this line is arithmetic; this is the half that
        // would break silently if `resvg`'s feature set were trimmed further, because a tree that
        // parses to nothing renders a fully transparent pixmap rather than an error.
        let svg = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/oblisk/wallpaper.svg");
        let (pixels, width, height) = rasterize_svg(&svg, 128).expect("the shipped wallpaper should parse");
        // 1920x1080 viewBox, longest edge 128, so the aspect ratio survives the scale.
        assert_eq!((width, height), (128, 72));
        assert_eq!(pixels.len(), (width * height * 4) as usize);
        let opaque = pixels.as_chunks::<4>().0.iter().filter(|px| px[3] > 0).count();
        assert_eq!(opaque, (width * height) as usize, "the wallpaper covers its whole viewBox");
        // Not a single flat colour: the gradient and the obelisk both have to survive, which is
        // what tells a real render apart from a pixmap that only got its background rect.
        let distinct: std::collections::HashSet<[u8; 3]> =
            pixels.as_chunks::<4>().0.iter().map(|px| [px[0], px[1], px[2]]).collect();
        assert!(distinct.len() > 16, "expected a gradient, got {} colours", distinct.len());
    }

    #[test]
    fn the_cache_stays_at_its_capacity_and_keeps_the_keys_it_kept() {
        // Negative entries only, because `ImageId` has no public constructor: this pins the bound
        // and the ordering, and `release_evicted`'s own doc comment covers the half a unit test
        // cannot reach without a GL context.
        let mut cache = ImageCache::new();
        for n in 0..(CACHE_CAPACITY * 2) {
            cache.insert(key(&format!("/tmp/{n}.png"), 0, FileVersion::default()), None);
        }
        assert_eq!(cache.entries.len(), CACHE_CAPACITY);
        assert_eq!(cache.order.len(), CACHE_CAPACITY);
        // Oldest out: the first half is gone and the last entry inserted is still there.
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
        // The key is the path and, for SVG only, the size. Two boxes asking for one PNG share one
        // upload; two boxes asking for one SVG at different sizes must not, or the smaller
        // rasterization is served to the larger box forever.
        let v = FileVersion::default();
        assert_eq!(key("/tmp/a.png", 12, v), key("/tmp/a.png", 24, v));
        assert_ne!(key("/tmp/a.svg", 12, v), key("/tmp/a.svg", 24, v));
    }

    #[test]
    fn one_path_rewritten_in_place_is_a_different_slot() {
        // docs/adr/0031's deferred item. The tray spools every icon update over the same
        // `/dev/shm/oblisk-$UID/tray/{name}.png`, so without the version in the key an app that
        // changes its icon keeps the pixels it had at first paint for the life of the process.
        let first = FileVersion { mtime_secs: 1_700_000_000, mtime_nanos: 0, len: 512 };
        let same_time_new_size = FileVersion { len: 640, ..first };
        let same_size_new_time = FileVersion { mtime_nanos: 1, ..first };
        assert_ne!(key("/dev/shm/x.png", 16, first), key("/dev/shm/x.png", 16, same_time_new_size));
        assert_ne!(key("/dev/shm/x.png", 16, first), key("/dev/shm/x.png", 16, same_size_new_time));
        assert_eq!(key("/dev/shm/x.png", 16, first), key("/dev/shm/x.png", 16, first));
    }

    #[test]
    fn a_missing_file_and_a_real_one_read_different_versions() {
        // The default is what a failed stat produces, and it is what makes a missing file retry
        // instead of staying negatively cached: once it exists, its key changes.
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
