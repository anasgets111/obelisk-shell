//! Decodes a file into a GPU texture, caches it, and fits it into a box (ADR-0054). PNG, JPEG and
//! WebP decode through the `image` crate ([`decode_raster`]); SVG decodes through `resvg`, needed
//! because Adwaita ships scalable SVG icons.
//!
//! The cache key is the path plus the box in physical pixels: a vector rasterized for a 12px box
//! would look blurry served to a 24px box under a path-only key, and a raster is downscaled to
//! cover its box (ADR-0122), so a 4K wallpaper drawn as a 230px thumbnail is a 230px texture
//! rather than a 32MB one. It also carries the file's mtime and length (ADR-0031), so the tray
//! overwriting a path in place still gets a fresh texture. Failures are cached too, as `Failed`,
//! so an unreadable file or `.svgz` (see [`rasterize_svg`]) isn't retried every frame from
//! `layout::paint`'s draw loop; a *missing* file still retries since its key changes on appearing.
//!
//! Decoding runs inline in the frame by default and on a worker pool when the node asked for
//! `async = true` (ADR-0122): the slot is `Pending` from the first draw until [`ImageCache::poll`]
//! finds the pixels landed and [`ImageCache::upload_landed`] turns them into a texture at the
//! start of the next paint. The pool decodes; only this thread, the one the canvas is current on
//! (ADR-0039), ever uploads. A pool decode also goes through the freedesktop thumbnail cache
//! ([`thumbnails`]): a current thumbnail is read instead of the file, and a file decoded in full
//! leaves one behind for the next open and for every other program that keeps that cache.
//!
//! Eviction is by two bounds (ADR-0123): [`CACHE_CAPACITY`] entries, oldest insert first, and
//! [`TEXTURE_BUDGET`] bytes of textures no mapped surface is showing, least recently asked-for
//! first, which `wayland::App` triggers after each paint with the pins its surfaces' last lists
//! name. A texture is freed at the start of the next paint, never mid-frame (see
//! [`ImageCache::release_evicted`]).

pub mod icons;
pub mod thumbnails;

use crate::layout::node::Rgba;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

use femtovg::renderer::OpenGl;
use femtovg::rgb::FromSlice;
use femtovg::{Canvas, ErrorKind, ImageFlags, ImageId, ImageSource};

use crate::text::snap::LogicalRect;

/// Entries, the ceiling on how many slots the map holds, `Failed` and `Pending` included. The
/// bytes are bounded separately by [`TEXTURE_BUDGET`]; this is what keeps a config cycling through
/// a thousand distinct failing paths from growing the map without bound.
const CACHE_CAPACITY: usize = 128;

/// Bytes of texture kept beyond what any mapped surface is showing (ADR-0123). Textures a surface
/// last painted are never evicted, whatever the total, so a working set larger than this is simply
/// over budget rather than thrashing through inline decodes; this bounds the *idle* part: the
/// wallpapers a user has moved on from (12 MB each on a 1920x1200 output, and before this every
/// one of them stayed for the next 128 inserts), the picker's tiles after it closed. 16 MB keeps
/// a closed picker's tiles (54 files, 6.5 MB) and the wallpaper just left on a 1920x1200 output,
/// and returns the rest to the GPU; on a 4K output a wallpaper left behind is 33 MB and goes at
/// once.
const TEXTURE_BUDGET: usize = 16 << 20;

/// How many decode workers the pool runs: the machine's parallelism, capped so a folder of forty
/// wallpapers landing at once does not take every core from the compositor drawing them.
const MAX_DECODE_WORKERS: usize = 4;

/// One cache slot. `box_px` is the box the texture was made for, in physical pixels: an SVG
/// rasterizes to its longest edge, a raster downscales to cover it (see this module's doc
/// comment).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheKey {
    path: PathBuf,
    box_px: (u32, u32),
    version: FileVersion,
    /// The `currentColor` value this texture was rasterized with, packed `0x00RRGGBB` (ADR-0072).
    /// `None` for a raster file and an untinted SVG. Part of the key because one theme file drawn
    /// white on the bar and dim in a popup is two textures, else the first tint wins for good.
    tint: Option<u32>,
}

/// What tells one revision of a file from the next, at a path that keeps its name (ADR-0031's
/// deferred item: the tray spools every icon update over `$XDG_RUNTIME_DIR/oblisk/tray/{name}.png`,
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

/// Whether a draw may wait for its pixels (ADR-0122). `Inline` is the default and what every
/// icon and the wallpaper use: the file decodes in the frame that first asks, so the first paint
/// is complete, which is what the candidate's presentation evidence promises (ADR-0003).
/// `Background` hands the decode to the pool and draws nothing until it lands, for a grid of
/// thumbnails where forty inline decodes would be a second of frozen shell.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Load {
    #[default]
    Inline,
    Background,
}

/// Pixels ready to upload, from either decoder, on the way to a texture.
struct Decoded {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    /// tiny-skia's `Pixmap` is premultiplied RGBA8 and the `image` crate's is straight, and
    /// femtovg samples a texture wrongly under the other flag: a dark halo around every
    /// anti-aliased icon edge, not an outright failure.
    premultiplied: bool,
}

/// One slot's state. `Pending` is a background decode in flight, which draws nothing and enqueues
/// nothing more: the first draw sent the job, and every draw until it lands finds this. `Ready`
/// carries the texture's size in bytes (RGBA8, so width times height times four), what
/// [`TEXTURE_BUDGET`] counts.
enum Slot {
    Pending,
    Ready(ImageId, usize),
    Failed,
}

/// A slot and when it was last asked for, on the [`ImageCache::tick`] clock, which is what
/// [`ImageCache::trim`] orders its victims by.
struct Entry {
    slot: Slot,
    last_hit: u64,
}

/// One decode the pool owes: the key it was asked under, so the result lands in the right slot
/// even if the box or file changed meanwhile, and the tint the key packed away.
struct Job {
    key: CacheKey,
    tint: Option<Rgba>,
}

/// The decode pool: a shared job queue and a result channel, `MAX_DECODE_WORKERS` threads at
/// most. Spawned with the cache, before any Lua has read anything, so a thread that fails to
/// start is a startup failure rather than a blank tile later.
struct Pool {
    jobs: Sender<Job>,
    results: Receiver<(CacheKey, Result<Decoded, String>)>,
}

impl Pool {
    fn spawn(waker: Option<crate::wake::Waker>) -> Self {
        let (jobs, job_rx) = std::sync::mpsc::channel::<Job>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let (result_tx, results) = std::sync::mpsc::channel();
        let workers = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, MAX_DECODE_WORKERS);
        let cache_root = thumbnails::cache_dir();
        for index in 0..workers {
            let job_rx = Arc::clone(&job_rx);
            let result_tx = result_tx.clone();
            let cache_root = cache_root.clone();
            let waker = waker.clone();
            std::thread::Builder::new()
                .name(format!("oblisk-image-decode-{index}"))
                .spawn(move || {
                    loop {
                        // The lock is held only to take a job, never through the decode, so the
                        // other workers keep draining while this one works.
                        let job = match job_rx.lock() {
                            Ok(rx) => rx.recv(),
                            Err(_) => return,
                        };
                        let Ok(job) = job else { return };
                        let result = decode(&job.key.path, job.key.box_px, job.tint, cache_root.as_deref());
                        if result_tx.send((job.key, result)).is_err() {
                            return;
                        }
                        // After the send, so the loop it wakes finds the result in `poll`.
                        if let Some(waker) = &waker {
                            waker.wake();
                        }
                    }
                })
                .expect("failed to spawn an oblisk-image-decode thread");
        }
        Pool { jobs, results }
    }
}

/// Path-and-size to uploaded texture, for one generation (`CONTEXT.md`, **Image cache**). Not
/// shared with the Supervisor and not persisted: the Renderer is swapped as an OS process on
/// every reload, so this is cold again after each config edit (ADR-0054).
pub struct ImageCache {
    entries: HashMap<CacheKey, Entry>,
    /// Insertion order, what [`CACHE_CAPACITY`] evicts by.
    order: VecDeque<CacheKey>,
    /// Evicted since the last [`ImageCache::release_evicted`], not yet freed.
    evicted: Vec<ImageId>,
    /// Bytes across every `Ready` slot, kept in step by [`ImageCache::insert`] and
    /// [`ImageCache::evict`].
    resident_bytes: usize,
    /// One more per [`ImageCache::image`] call: a Lamport clock, so "least recently asked for"
    /// needs no `Instant` and no frame notion this module does not have.
    tick: u64,
    pool: Pool,
    /// Decodes [`ImageCache::poll`] took off the pool and [`ImageCache::upload_landed`] has not
    /// yet turned into textures: `poll` runs where there is no canvas, in the main loop's turn.
    landed: Vec<(CacheKey, Result<Decoded, String>)>,
}

impl Default for ImageCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageCache {
    /// A cache whose pool wakes nobody: the tests', which poll for a landing themselves.
    pub fn new() -> Self {
        Self::build(None)
    }

    /// The Renderer's: a landing wakes the Wayland thread's poll (ADR-0124).
    pub fn with_waker(waker: crate::wake::Waker) -> Self {
        Self::build(Some(waker))
    }

    fn build(waker: Option<crate::wake::Waker>) -> Self {
        ImageCache {
            entries: HashMap::new(),
            order: VecDeque::new(),
            evicted: Vec::new(),
            resident_bytes: 0,
            tick: 0,
            pool: Pool::spawn(waker),
            landed: Vec::new(),
        }
    }

    /// Frees textures evicted last frame; `layout::paint::paint_tree` calls this before walking
    /// anything, since femtovg resolves an `ImageId` to a texture at `flush`, not `fill_path`, so
    /// deleting mid-walk unbinds a texture a recorded command still names, drawing silently blank.
    pub fn release_evicted(&mut self, canvas: &mut Canvas<OpenGl>) {
        for id in self.evicted.drain(..) {
            canvas.delete_image(id);
        }
    }

    /// Takes every finished background decode off the pool and names the files that landed,
    /// which is the main loop's cue to repaint whatever draws them. No canvas here: the loop's
    /// turn has none current, so the pixels wait in `landed` for the paint that follows.
    pub fn poll(&mut self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        loop {
            match self.pool.results.try_recv() {
                Ok(result) => {
                    files.push(result.0.path.clone());
                    self.landed.push(result);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    eprintln!("[oblisk-renderer] image: every decode worker is gone; background images will not load");
                    break;
                }
            }
        }
        files
    }

    /// Uploads what [`ImageCache::poll`] collected, at the start of a paint alongside
    /// [`ImageCache::release_evicted`]. A result whose slot was evicted while it decoded is
    /// dropped: the next draw of that file finds no slot and sends the job again.
    pub fn upload_landed(&mut self, canvas: &mut Canvas<OpenGl>) {
        for (key, result) in std::mem::take(&mut self.landed) {
            if !matches!(self.entries.get(&key).map(|entry| &entry.slot), Some(Slot::Pending)) {
                continue;
            }
            let slot = upload_or_log(canvas, &key.path, result);
            if let Slot::Ready(_, bytes) = slot {
                self.resident_bytes += bytes;
            }
            if let Some(entry) = self.entries.get_mut(&key) {
                entry.slot = slot;
            }
        }
    }

    /// Evicts idle textures until the total is back under [`TEXTURE_BUDGET`], oldest ask first
    /// (ADR-0123). `pinned` names what may not go: every `(path, box)` some mapped surface's last
    /// display list draws, which `wayland::App` collects from its surfaces after each paint. Asked
    /// for lazily, since walking every list is only worth it when there is something to evict.
    /// Icons are not pinned: a list carries a theme name where the cache has a path, an icon is a
    /// few kilobytes, and one evicted is one inline re-raster on its next paint.
    pub fn trim(&mut self, pinned: impl FnOnce() -> Vec<(PathBuf, (u32, u32))>) {
        if self.resident_bytes <= TEXTURE_BUDGET {
            return;
        }
        let pinned = pinned();
        let candidates = self.entries.iter().filter_map(|(key, entry)| match entry.slot {
            Slot::Ready(_, bytes) => Some((key.clone(), bytes, entry.last_hit)),
            Slot::Pending | Slot::Failed => None,
        });
        for key in victims(candidates, self.resident_bytes, TEXTURE_BUDGET, &pinned) {
            self.evict(&key);
        }
    }

    /// The uploaded texture for `path`, or `None` for anything that did not decode (logged once,
    /// not once per frame) or has not yet (`Load::Background` and still pending). `box_px` is the
    /// box in physical pixels: an SVG rasterizes to its longest edge, a raster downscales to
    /// cover it and never scales up. The canvas must be current on the calling thread, the only
    /// one that paints since ADR-0039. Stats the file on every call, including a hit, since the
    /// key carries its revision (see [`FileVersion`]): one `stat` per node per frame, six
    /// hundred a second for ten icons at 60Hz.
    ///
    /// `Load::Inline` reads the file, and for an SVG rasterizes it, inside the frame, which is
    /// also the Wayland dispatch thread and the config VM's thread (ADR-0039). That is the point
    /// for a wallpaper, whose first frame must be whole; a grid of files asks for
    /// `Load::Background` instead (ADR-0122).
    pub fn image(
        &mut self,
        canvas: &mut Canvas<OpenGl>,
        path: &Path,
        box_px: (u32, u32),
        tint: Option<Rgba>,
        load: Load,
    ) -> Option<ImageId> {
        let vector = is_vector(path);
        let box_px = (box_px.0.max(1), box_px.1.max(1));
        let key = CacheKey {
            path: path.to_path_buf(),
            // A vector's texture is its longest edge either way, so the key says so and a 24x30
            // box shares the 30x30 one's slot.
            box_px: if vector { (box_px.0.max(box_px.1), box_px.0.max(box_px.1)) } else { box_px },
            version: FileVersion::read(path),
            // Only a vector can carry `currentColor`; a PNG's tint is dropped rather than
            // splitting its cache slot per colour it will never use.
            tint: if vector { tint.map(packed_rgb) } else { None },
        };
        self.tick += 1;
        if let Some(cached) = self.entries.get_mut(&key) {
            cached.last_hit = self.tick;
            return match cached.slot {
                Slot::Ready(id, _) => Some(id),
                Slot::Pending | Slot::Failed => None,
            };
        }
        match load {
            Load::Inline => {
                let slot = upload_or_log(canvas, &key.path, decode(&key.path, key.box_px, tint, None));
                let id = match slot {
                    Slot::Ready(id, _) => Some(id),
                    _ => None,
                };
                self.insert(key, slot);
                id
            }
            Load::Background => {
                if self.pool.jobs.send(Job { key: key.clone(), tint }).is_err() {
                    eprintln!("[oblisk-renderer] image: {}: no decode worker left to take it", key.path.display());
                    self.insert(key, Slot::Failed);
                    return None;
                }
                self.insert(key, Slot::Pending);
                None
            }
        }
    }

    /// Evicts before inserting, so the map never exceeds [`CACHE_CAPACITY`]. Queues the evicted
    /// texture for [`ImageCache::release_evicted`] rather than deleting it here: femtovg frees
    /// nothing by `ImageId` until told to, so losing the id leaks the GPU allocation for good.
    fn insert(&mut self, key: CacheKey, slot: Slot) {
        while self.order.len() >= CACHE_CAPACITY {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.evict(&oldest);
        }
        if let Slot::Ready(_, bytes) = slot {
            self.resident_bytes += bytes;
        }
        self.order.push_back(key.clone());
        self.entries.insert(key, Entry { slot, last_hit: self.tick });
    }

    /// Drops one entry, queueing its texture for [`ImageCache::release_evicted`] and taking its
    /// bytes off the total. `order` is scanned, which is the [`CACHE_CAPACITY`]-bounded cost of
    /// an eviction and nothing a frame pays.
    fn evict(&mut self, key: &CacheKey) {
        if let Some(entry) = self.entries.remove(key) {
            if let Slot::Ready(id, bytes) = entry.slot {
                self.evicted.push(id);
                self.resident_bytes -= bytes;
            }
            if let Some(at) = self.order.iter().position(|k| k == key) {
                self.order.remove(at);
            }
        }
    }
}

/// Which of `candidates` (`(key, bytes, last_hit)`) to evict to bring `resident` under `budget`:
/// least recently asked for first, skipping anything `pinned` names by path and box, stopping
/// once under budget or out of unpinned candidates. Pure, so the policy is testable without an
/// `ImageId`, which femtovg gives no way to make outside a canvas.
fn victims(
    candidates: impl Iterator<Item = (CacheKey, usize, u64)>,
    resident: usize,
    budget: usize,
    pinned: &[(PathBuf, (u32, u32))],
) -> Vec<CacheKey> {
    let mut idle: Vec<(CacheKey, usize, u64)> = candidates
        .filter(|(key, _, _)| !pinned.iter().any(|(path, box_px)| *path == key.path && *box_px == key.box_px))
        .collect();
    idle.sort_by_key(|(_, _, last_hit)| *last_hit);
    let mut resident = resident;
    let mut out = Vec::new();
    for (key, bytes, _) in idle {
        if resident <= budget {
            break;
        }
        resident -= bytes;
        out.push(key);
    }
    out
}

/// Uploads a decode, or logs why there is nothing to upload. Each failure is logged once here,
/// at the moment its slot is filled, and the `Failed` slot is what stops the next frame asking
/// again.
fn upload_or_log(canvas: &mut Canvas<OpenGl>, path: &Path, decoded: Result<Decoded, String>) -> Slot {
    let result = decoded.and_then(|decoded| {
        let bytes = decoded.pixels.len();
        upload(canvas, decoded).map(|id| (id, bytes))
    });
    match result {
        Ok((id, bytes)) => Slot::Ready(id, bytes),
        Err(err) => {
            eprintln!("[oblisk-renderer] image: {}: {err}", path.display());
            Slot::Failed
        }
    }
}

/// By extension, not by sniffing the file: the producers here all name their files
/// (`freedesktop-icons` returns `.svg` or `.png`, `shm_icons.rs` writes `.png`).
fn is_vector(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("svg"))
}

/// The half of a load that needs no canvas, so it can run on a pool thread: a raster decoded and
/// downscaled to cover `box_px`, through the thumbnail cache under `thumbnails` when there is
/// one, or an SVG rasterized at `box_px`'s longest edge.
fn decode(path: &Path, box_px: (u32, u32), tint: Option<Rgba>, thumbnails: Option<&Path>) -> Result<Decoded, String> {
    if is_vector(path) {
        let (pixels, width, height) = rasterize_svg(path, box_px.0.max(box_px.1), tint)?;
        return Ok(Decoded { pixels, width, height, premultiplied: true });
    }
    let (pixels, width, height) = decode_raster(path, box_px, thumbnails)?;
    Ok(Decoded { pixels, width, height, premultiplied: false })
}

/// The half of a load that needs the canvas: one texture from one decode.
fn upload(canvas: &mut Canvas<OpenGl>, decoded: Decoded) -> Result<ImageId, String> {
    let Decoded { pixels, width, height, premultiplied } = decoded;
    let source = ImageSource::from(femtovg::imgref::Img::new(pixels.as_rgba(), width as usize, height as usize));
    let flags = if premultiplied { ImageFlags::PREMULTIPLIED } else { ImageFlags::empty() };
    canvas.create_image(source, flags).map_err(femtovg_error)
}

/// femtovg's `ErrorKind` writes the literal string `"canvas error"` for every one of its
/// sixteen variants, so `Debug` is the only thing that names which failure happened.
fn femtovg_error(err: ErrorKind) -> String {
    format!("{err:?}")
}

/// The size a `width`x`height` raster is stored at for a `box_px` box (ADR-0122): scaled down by
/// the larger of the two ratios, so it still covers the box the way `Fit::Cover` will crop it,
/// and never scaled up, since a small file has no more pixels to give. The same for every `Fit`:
/// `Contain` could go smaller, but one rule keeps one slot per box.
fn stored_size(width: u32, height: u32, box_px: (u32, u32)) -> (u32, u32) {
    if width == 0 || height == 0 || (width <= box_px.0 && height <= box_px.1) {
        return (width, height);
    }
    let scale = (box_px.0 as f64 / width as f64).max(box_px.1 as f64 / height as f64);
    if scale >= 1.0 {
        return (width, height);
    }
    (((width as f64 * scale).ceil() as u32).max(1), ((height as f64 * scale).ceil() as u32).max(1))
}

/// Decodes a PNG, JPEG or WebP to straight-alpha RGBA8 at [`stored_size`], the reason `image`
/// is a direct dependency (`renderer/Cargo.toml`). femtovg's `Canvas::load_image_file` cannot
/// decode anything: it declares `image` with `default-features = false` and no format, so every
/// PNG came back `Unsupported(Exact(Png))`, taking the tray's and notification daemon's spooled
/// pixmaps (ADR-0031) with it. `into_rgba8` also covers the grayscale-plus-alpha and 16-bit
/// variants femtovg's own conversion refuses, at no cost since every theme icon already decodes
/// to RGBA8. `thumbnail` is the `image` crate's fast triangle-filter downscale, which for a 4K
/// file to a tile is the difference between a decode and a decode plus a Lanczos pass.
///
/// With a `thumbnails` root and a box a thumbnail covers, a current thumbnail is decoded instead
/// of the file, and a file decoded in full leaves a thumbnail behind when it was larger than one
/// (a 32px tray icon is never thumbnailed). A thumbnail that cannot be written is one log line
/// and the texture it would have saved is made anyway.
fn decode_raster(path: &Path, box_px: (u32, u32), thumbnails: Option<&Path>) -> Result<(Vec<u8>, u32, u32), String> {
    let slot = thumbnails.and_then(|root| thumbnails::Slot::for_file(root, path, box_px));
    if let Some(slot) = &slot
        && let Some((pixels, width, height)) = slot.read_valid()
    {
        let (stored_width, stored_height) = stored_size(width, height, box_px);
        if (stored_width, stored_height) == (width, height) {
            return Ok((pixels, width, height));
        }
        let image = ::image::RgbaImage::from_raw(width, height, pixels).ok_or("thumbnail pixel count is off")?;
        let scaled = ::image::DynamicImage::ImageRgba8(image).thumbnail(stored_width, stored_height).into_rgba8();
        let (width, height) = scaled.dimensions();
        return Ok((scaled.into_raw(), width, height));
    }
    let decoded = ::image::open(path).map_err(|err| err.to_string())?;
    let (width, height) = (decoded.width(), decoded.height());
    if let Some(slot) = &slot
        && width.max(height) > slot.px
    {
        let thumb = decoded.thumbnail(slot.px, slot.px).into_rgba8();
        let (thumb_width, thumb_height) = thumb.dimensions();
        if let Err(err) = slot.write(thumb.as_raw(), thumb_width, thumb_height) {
            eprintln!("[oblisk-renderer] image: {}: thumbnail not written: {err}", path.display());
        }
    }
    let (stored_width, stored_height) = stored_size(width, height, box_px);
    let decoded = if (stored_width, stored_height) == (width, height) {
        decoded
    } else {
        decoded.thumbnail(stored_width, stored_height)
    };
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
            box_px: (18, 18),
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

        let (pixels, width, height) = decode_raster(&png, (2, 2), None).expect("a PNG decoder must be compiled in");
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
        assert!(decode_raster(&fake, (8, 8), None).is_err());
    }

    #[test]
    fn the_cache_stays_at_its_capacity_and_keeps_the_keys_it_kept() {
        // Negative entries only: `ImageId` has no public constructor.
        let mut cache = ImageCache::new();
        for n in 0..(CACHE_CAPACITY * 2) {
            cache.insert(key(&format!("/tmp/{n}.png"), 0, FileVersion::default()), Slot::Failed);
        }
        assert_eq!(cache.entries.len(), CACHE_CAPACITY);
        assert_eq!(cache.order.len(), CACHE_CAPACITY);
        let newest = key(&format!("/tmp/{}.png", CACHE_CAPACITY * 2 - 1), 0, FileVersion::default());
        let oldest = key("/tmp/0.png", 0, FileVersion::default());
        assert!(cache.entries.contains_key(&newest));
        assert!(!cache.entries.contains_key(&oldest));
    }

    fn key(path: &str, px: u32, version: FileVersion) -> CacheKey {
        CacheKey { path: PathBuf::from(path), box_px: (px, px), version, tint: None }
    }

    #[test]
    fn the_budget_evicts_the_least_recently_asked_for_idle_texture_and_never_a_pinned_one() {
        // Three wallpapers of 12 MB and a screen of tiles: the one on screen is pinned, the tiles
        // were asked for after the older wallpaper, so the older wallpaper goes first and the
        // budget is met without touching the tiles.
        let mb = 1 << 20;
        let v = FileVersion::default();
        let old = key("/w/old.jpg", 1920, v);
        let older = key("/w/older.jpg", 1920, v);
        let shown = key("/w/shown.jpg", 1920, v);
        let tiles = key("/w/tiles.png", 232, v);
        let candidates = vec![
            (older.clone(), 12 * mb, 1),
            (old.clone(), 12 * mb, 2),
            (tiles.clone(), 7 * mb, 3),
            (shown.clone(), 12 * mb, 4),
        ];
        let pinned = vec![(PathBuf::from("/w/shown.jpg"), (1920, 1920))];
        let out = victims(candidates.clone().into_iter(), 43 * mb, 31 * mb, &pinned);
        assert_eq!(out, vec![older.clone()]);
        // A tighter budget takes the next-oldest, then stops at the pinned one even though it is
        // still over: a working set larger than the budget is over budget, not thrashing.
        let out = victims(candidates.clone().into_iter(), 43 * mb, 10 * mb, &pinned);
        assert_eq!(out, vec![older, old, tiles]);
        // Under budget, nothing moves.
        assert!(victims(candidates.into_iter(), 43 * mb, 43 * mb, &pinned).is_empty());
    }

    #[test]
    fn a_pin_is_by_path_and_box_so_a_tile_of_the_shown_file_is_still_idle() {
        let v = FileVersion::default();
        let full = key("/w/a.jpg", 1920, v);
        let tile = key("/w/a.jpg", 232, v);
        let pinned = vec![(PathBuf::from("/w/a.jpg"), (1920, 1920))];
        let out = victims(vec![(full.clone(), 10, 1), (tile.clone(), 10, 2)].into_iter(), 20, 15, &pinned);
        assert_eq!(out, vec![tile]);
    }

    #[test]
    fn trim_under_budget_never_asks_for_the_pins() {
        let mut cache = ImageCache::new();
        cache.trim(|| unreachable!("nothing resident, nothing to walk"));
        assert_eq!(cache.resident_bytes, 0);
    }

    #[test]
    fn a_vector_and_a_raster_alike_take_one_slot_per_box() {
        let v = FileVersion::default();
        assert_ne!(key("/tmp/a.png", 12, v), key("/tmp/a.png", 24, v));
        assert_ne!(key("/tmp/a.svg", 12, v), key("/tmp/a.svg", 24, v));
        assert_ne!(key("/tmp/a.svg", 12, v), key("/tmp/a.png", 12, v));
    }

    #[test]
    fn a_raster_is_stored_scaled_down_to_cover_its_box_and_never_up() {
        // 4K into a 16:9 tile: both ratios agree.
        assert_eq!(stored_size(3840, 2160, (230, 130)), (232, 130));
        // Portrait into a landscape tile: the width ratio is the larger, so the width covers.
        assert_eq!(stored_size(1080, 1920, (230, 130)), (230, 409));
        // Already smaller than the box on both edges: untouched.
        assert_eq!(stored_size(16, 16, (24, 24)), (16, 16));
        // Larger on one edge only: still scaled by the larger ratio, which is under one.
        assert_eq!(stored_size(300, 10, (100, 100)), (300, 10));
        assert_eq!(stored_size(0, 0, (100, 100)), (0, 0));
    }

    #[test]
    fn a_large_png_decodes_to_its_box_and_a_small_one_to_itself() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.png");
        ::image::RgbaImage::from_pixel(400, 200, ::image::Rgba([10, 20, 30, 255])).save(&big).unwrap();
        let (_, width, height) = decode_raster(&big, (100, 100), None).unwrap();
        assert_eq!((width, height), (200, 100));
        let (_, width, height) = decode_raster(&big, (1000, 1000), None).unwrap();
        assert_eq!((width, height), (400, 200));
    }

    #[test]
    fn a_pool_decode_leaves_a_thumbnail_behind_and_the_next_one_reads_it_instead_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.png");
        ::image::RgbaImage::from_pixel(400, 200, ::image::Rgba([10, 20, 30, 255])).save(&big).unwrap();
        let cache = dir.path().join("cache");

        let (_, width, height) = decode_raster(&big, (100, 100), Some(&cache)).unwrap();
        assert_eq!((width, height), (200, 100), "the texture is the box's, whatever the thumbnail is");
        let slot = thumbnails::Slot::for_file(&cache, &big, (100, 100)).unwrap();
        let (_, thumb_width, thumb_height) = slot.read_valid().expect("a `normal` thumbnail was written");
        assert_eq!((thumb_width, thumb_height), (128, 64));

        // The source is gone: only the thumbnail can answer now, and it does.
        std::fs::remove_file(&big).unwrap();
        assert!(decode_raster(&big, (100, 100), Some(&cache)).is_err(), "no source, no mtime, no slot");
    }

    #[test]
    fn a_file_no_larger_than_a_thumbnail_is_not_thumbnailed() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("icon.png");
        ::image::RgbaImage::from_pixel(32, 32, ::image::Rgba([10, 20, 30, 255])).save(&small).unwrap();
        let cache = dir.path().join("cache");
        decode_raster(&small, (24, 24), Some(&cache)).unwrap();
        assert!(!cache.exists(), "a 32px file has nothing to gain from a 128px thumbnail");
    }

    #[test]
    fn the_pool_decodes_a_job_off_thread_and_poll_reports_it_landed() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("fixture.png");
        std::fs::write(&png, PIL_2X2_RGBA_PNG).unwrap();
        let mut cache = ImageCache::new();
        let key = CacheKey { path: png.clone(), box_px: (8, 8), version: FileVersion::read(&png), tint: None };
        cache.insert(key.clone(), Slot::Pending);
        cache.pool.jobs.send(Job { key: key.clone(), tint: None }).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut files = cache.poll();
        while files.is_empty() {
            assert!(std::time::Instant::now() < deadline, "the decode never landed");
            std::thread::sleep(std::time::Duration::from_millis(5));
            files = cache.poll();
        }
        assert_eq!(files, vec![png.clone()]);
        let (landed_key, result) = &cache.landed[0];
        assert_eq!(*landed_key, key);
        let decoded = result.as_ref().expect("a 2x2 PNG decodes");
        assert_eq!((decoded.width, decoded.height), (2, 2));
        assert!(!decoded.premultiplied);
        assert!(
            matches!(cache.entries.get(&key).map(|e| &e.slot), Some(Slot::Pending)),
            "no canvas, so nothing uploaded yet"
        );
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
