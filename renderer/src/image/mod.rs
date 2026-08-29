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
//! Failures are cached too, as `None`. A missing file, an unreadable one, an `.svgz` (see
//! [`rasterize_svg`]) would otherwise be retried on every frame, and `layout::paint` logs from
//! inside the frame loop -- one bad path would be an unbounded log stream on the Wayland dispatch
//! thread, which is the failure mode `paint::log_paint_error`'s own ponytail comment already
//! describes for paint properties.

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
#[derive(Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    path: PathBuf,
    raster_px: u32,
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
        }
    }

    /// The uploaded texture for `path`, decoding and uploading on the first ask. `box_px` is the
    /// longest edge of the box this will be drawn into, in physical pixels, and is what an SVG
    /// rasterizes against; a raster file ignores it.
    ///
    /// `None` for anything that did not decode, logged once rather than once per frame (see this
    /// module's doc comment). The canvas must be current on the calling thread, which since
    /// docs/adr/0039 is the only thread that paints.
    pub fn image(&mut self, canvas: &mut Canvas<OpenGl>, path: &Path, box_px: u32) -> Option<ImageId> {
        let key = CacheKey {
            path: path.to_path_buf(),
            raster_px: if is_vector(path) { box_px.max(1) } else { 0 },
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
        self.insert(canvas, key, loaded);
        loaded
    }

    /// Evicts before inserting, so the map never exceeds [`CACHE_CAPACITY`] rather than exceeding
    /// it by one and trimming afterwards. Deletes the evicted texture from the canvas: dropping
    /// the `ImageId` alone leaks the GPU allocation, since femtovg keys its own image store by
    /// that id and frees nothing until told to.
    fn insert(&mut self, canvas: &mut Canvas<OpenGl>, key: CacheKey, value: Option<ImageId>) {
        while self.order.len() >= CACHE_CAPACITY {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(Some(id)) = self.entries.remove(&oldest) {
                canvas.delete_image(id);
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
    fn fit_parses_the_three_spelled_modes_and_nothing_else() {
        assert_eq!(Fit::from_str("cover"), Some(Fit::Cover));
        assert_eq!(Fit::from_str("contain"), Some(Fit::Contain));
        assert_eq!(Fit::from_str("stretch"), Some(Fit::Stretch));
        assert_eq!(Fit::from_str("Cover"), None);
        assert_eq!(Fit::from_str("fill"), None);
        assert_eq!(Fit::default(), Fit::Cover);
    }
}
