//! Off-thread wrapper around `cosmic-text`'s font shaping, so measuring a dynamic string never
//! blocks the render thread. The worker builds its `FontSystem` from `text::fonts::resolve_chain`'s
//! declared chain, not `FontSystem::new()`, which takes up to ~1s per cosmic-text's own docs
//! (ADR-0043 decision 2). A plain `std::thread` plus `mpsc` runs it rather than tokio's
//! multi-thread runtime: this is one dedicated CPU-bound worker, and renderer's Cargo.toml only
//! carries tokio's `rt`/`net`/`macros` features.
//!
//! `shape()` asks for `Family::Name(primary_family)`, the resolved chain's own first hit, and paint
//! draws the faces it chose (ADR-0211).

use std::collections::{HashMap, HashSet};
use std::env;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread;

use cosmic_text::{Attrs, Buffer, Family, FontSystem, LineIter, Metrics, Shaping, Style, Weight};

use super::fonts::{self, ResolvedFonts};

/// A byte range of a request's text shaped in a bold and/or italic face rather than the chain's
/// regular one (ADR-0104). Only what changes a *measurement* is here: an underline and a colour
/// are paint's business and never reach the worker, which is why this is not `layout`'s
/// `StyleRun` but the half of it the shaper reads.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FontRun {
    pub range: Range<usize>,
    pub bold: bool,
    pub italic: bool,
}

/// A shaping request: the text to measure and the metrics to shape it at.
pub struct ShapeRequest {
    pub text: String,
    pub font_size: f32,
    pub line_height: f32,
    /// Logical-pixel width to wrap at. `None` measures the text unconstrained, on one line, which
    /// is what every caller uses today.
    pub max_width: Option<f32>,
    /// The parts of `text` in another face than the regular one, in order, non-overlapping, on
    /// character boundaries -- `layout` builds them that way. Empty for plain text, which is every
    /// request but a notification body's.
    pub runs: Vec<FontRun>,
    /// The family this node named, exactly as the config wrote it (ADR-0144). `None` is the
    /// declared chain, which is every node that says nothing. A measurement is only usable by the
    /// paint that draws the same family, which is why this is part of the cache key too.
    pub font: Option<Arc<str>>,
}

/// The measured result of shaping a request: its tight bounding box in logical pixels, plus the
/// lines the text was broken onto getting there.
///
/// Behind an `Arc` because [`ShapingHandle::shape`] hands a clone back on every memo hit, and a
/// hit is the common case: `Scene::apply` re-measures every text node it resolves.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapeResult {
    pub width: f32,
    pub height: f32,
    /// One entry per laid-out line, in order, each trimmed of the whitespace a word wrap leaves
    /// behind at the break. Never empty for non-empty text: a string that needs no break is one
    /// entry holding the whole string.
    pub lines: Arc<[String]>,
    /// Where each of `lines` came from: the byte range of the request's text it is a slice of,
    /// parallel to `lines` (`text[line_ranges[i]] == lines[i]`). What lets `layout` carry a
    /// styled run through a wrap: the runs are ranges over the source, and a line that knows its
    /// own range can say which runs it holds (ADR-0104).
    pub line_ranges: Arc<[Range<usize>]>,
    /// How each of `lines` is laid out, parallel to it (ADR-0211). Paint and link hit-testing
    /// read these glyphs rather than shaping the line a second time.
    pub shaped: Arc<[ShapedLine]>,
}

/// One laid-out line (ADR-0211): which way it reads, how far its baseline sits below the line's
/// top, and its glyphs.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapedLine {
    pub rtl: bool,
    pub width: f32,
    pub baseline: f32,
    pub glyphs: Box<[Glyph]>,
}

/// One placed glyph: the face, weight and glyph cosmic-text chose, its pen position from the line's
/// left edge and baseline, its advance, and the byte of the request's text it came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glyph {
    pub face: fontdb::ID,
    /// What a variable face was shaped at ([`FontFace::coords`]).
    pub weight: u16,
    pub id: u16,
    pub x: f32,
    pub y: f32,
    pub advance: f32,
    pub start: usize,
}

/// The multiplier every measurement and every paint derives a line height from.
///
/// One constant rather than the four call sites that each spelled `font_size * 1.2`: paint has to
/// advance a wrapped line by exactly the step the shaper measured with, and "exactly" is not
/// something to maintain by hand in four places.
pub const LINE_HEIGHT_RATIO: f32 = 1.2;

/// [`LINE_HEIGHT_RATIO`] applied, for the callers that have a font size and want the step.
pub fn line_height(font_size: f32) -> f32 {
    font_size * LINE_HEIGHT_RATIO
}

/// One font file's bytes, held once and shared by every reader. The inner `Arc` is `fontdb`'s:
/// `Database::make_shared_face_data` maps the file and rewrites every face from it to point at
/// the mapping, so cosmic-text (which owns the `Database`) and femtovg (handed this) read the
/// same `Shared_Clean`, page-cache-backed pages instead of each holding a copy. The newtype exists
/// because femtovg's `add_shared_font_with_index` takes `T: AsRef<[u8]> + 'static` by value, and
/// `Arc<dyn AsRef<[u8]>>` doesn't itself implement `AsRef<[u8]>`.
#[derive(Clone)]
pub struct FontData(std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>);

/// One face femtovg should load: its file's shared bytes, which face of the file, and the id
/// cosmic-text's glyphs name it by (ADR-0211).
#[derive(Clone)]
pub struct FontFace {
    pub data: FontData,
    pub index: u32,
    pub id: fontdb::ID,
}

impl FontFace {
    /// The normalized axis coordinates cosmic-text shapes this face at for `weight`, in `fvar`
    /// order, the way `cosmic_text::Font::new` derives them; empty for a face with no axes.
    pub fn coords(&self, weight: u16) -> Vec<i16> {
        use cosmic_text::skrifa::{FontRef, MetadataProvider, Tag};
        let Ok(font) = FontRef::from_index(self.data.as_ref(), self.index) else {
            return Vec::new();
        };
        let location = font.axes().location([(Tag::new(b"wght"), f32::from(weight))]);
        location.coords().iter().map(|coord| coord.to_bits()).collect()
    }
}

impl FontData {
    /// The address of the shared mapping behind this face, which identifies it across rebuilds of
    /// the chain list. `fontdb::Database::make_shared_face_data` hands back the *same* `Arc` for a
    /// face it has already shared (its `Source::SharedFile` arm is a `data.clone()`), so two
    /// `FontFace`s built from one file in two `font_chain_data` calls compare equal here.
    ///
    /// What `text::atlas` keys its femtovg registrations on: femtovg's `add_shared_font_with_index`
    /// mints a fresh `FontId` on every call and never dedups by bytes, so re-registering a face it
    /// already holds parses the file again and leaks the old entry (ADR-0144).
    pub fn addr(&self) -> usize {
        std::sync::Arc::as_ptr(&self.0) as *const () as usize
    }
}

impl AsRef<[u8]> for FontData {
    fn as_ref(&self) -> &[u8] {
        (*self.0).as_ref()
    }
}

enum Request {
    /// The `bool` keeps each line's glyphs ([`ShapingHandle::shape_glyphs`]).
    Shape(ShapeRequest, bool, mpsc::Sender<ShapeResult>),
    FontChainData(mpsc::Sender<Vec<FontFace>>),
    /// Replace the chain this worker measures against, once the config has said what it wants
    /// (ADR-0043 decision 2). Replies once the new `FontSystem` is live, so the next
    /// `font_chain_data` call answers with the new faces.
    SetChain(Vec<String>, mpsc::Sender<()>),
    /// Resolve a family without measuring anything (ADR-0144). A `text` node with an explicit
    /// width *and* height is never handed to taffy's measure callback (`compute_leaf_layout`
    /// returns early when both are known) and, without `elide` or `wrap`, never reaches
    /// `fit_text_to_box`'s shaping either -- so nothing would ever resolve its family and it would
    /// paint in the declared chain.
    EnsureFamily(Arc<str>, mpsc::Sender<()>),
    // Test-only (`#[cfg(test)]`, not `#[allow(dead_code)]`): nothing outside a test binary sends
    // this. `fonts::resolve_chain`'s own `eprintln!` is the production diagnostic.
    #[cfg(test)]
    ResolvedPrimaryFamily(mpsc::Sender<String>),
}

/// How many measured strings [`ShapingHandle`] remembers before it drops the lot. Sized against
/// the working set (a bar resolves about twenty text nodes, a long list a few hundred) with room
/// for churn: a clock shapes a string nobody asks for again, so an unbounded map would grow for the life of the session. Full, the map holds on
/// the order of 400KB (4096 short `String` entries plus four integers and two floats, plus
/// `HashMap` overhead), under one percent of ADR-0043's 50MB-per-monitor budget.
///
/// Measured 2026-09-06 against a live shell, idle: **3.2 to 4.3 new entries a minute**,
/// so the cap is reached in roughly **sixteen hours**, not the hour an earlier draft of this
/// comment claimed. That estimate assumed a clock ticking seconds; that shell's shows minutes, and
/// the churn that does exist comes from elsewhere. Nothing about the sizing changes -- a cache that
/// turns over daily rather than hourly holds fewer dead entries, not more -- but the number is
/// worth stating correctly, because it is the one that says whether 4096 is generous or tight.
///
/// Entries from [`ShapingHandle::shape_glyphs`] also hold a 32-byte `Glyph` per glyph (ADR-0211).
///
/// Note that `HashMap::clear` keeps the table it has grown, so after the first fill the bucket
/// array is a floor for the rest of the session rather than something the clear gives back.
const SHAPE_CACHE_CAPACITY: usize = 4096;

/// What a measurement is keyed by: every field of a [`ShapeRequest`]. Keying on a subset would be
/// a wrong-answer bug, not a slow one, which is why `line_height` is here even though every caller
/// today derives it as `font_size * 1.2`. Floats are held as bit patterns because `f32` is not
/// `Hash` or `Eq`, making equality here stricter than `==` in both harmless directions: `NaN`s
/// sharing a bit pattern share an entry `==` would call different, and `0.0`/`-0.0` get separate
/// entries `==` would call equal. Neither direction can return another request's answer.
#[derive(PartialEq, Eq, Hash)]
struct ShapeKey {
    text: String,
    font_size: u32,
    line_height: u32,
    max_width: Option<u32>,
    runs: Vec<FontRun>,
    font: Option<Arc<str>>,
    glyphs: bool,
}

/// A handle to a dedicated shaping worker thread and its warm font cache. `Clone` clones only the
/// request `Sender`, so every clone addresses the one worker and `FontSystem`, letting
/// `wayland::App` and the `RendererClient` it owns share a warm cache instead of each paying
/// `FontSystem::new()`'s ~1s startup (ADR-0023, closed by ADR-0039 decision 3). The measurement
/// cache is `Arc`-shared for the same reason, and lives on this side of the channel: a worker-side
/// cache would still pay an `mpsc` round trip and thread wake per node, a real fraction of the
/// 17us each costs on a 500-row list.
#[derive(Clone)]
pub struct ShapingHandle {
    requests: mpsc::Sender<Request>,
    cache: Arc<Mutex<HashMap<ShapeKey, ShapeResult>>>,
    /// See [`ShapingHandle::font_generation`].
    generation: Arc<AtomicU64>,
    /// Families [`ShapingHandle::ensure_family`] has already asked the worker about, so the common
    /// case costs a hash lookup instead of a channel round trip. Mirrors the worker's own memo
    /// rather than replacing it: the worker still has to answer a family this side never saw.
    ensured: Arc<Mutex<HashSet<Arc<str>>>>,
}

impl ShapingHandle {
    /// Spawns the worker thread and its own `FontSystem`, created inside the spawned closure
    /// rather than moved into it, so this doesn't require `FontSystem: Send`.
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel::<Request>();
        // Bumped by the worker whenever the loaded face set changes, read by the painter to know
        // its femtovg font registry is stale (ADR-0144). An atomic rather than another request:
        // paint asks once a frame and must not pay a channel round trip to hear "nothing changed".
        let generation = Arc::new(AtomicU64::new(0));
        let handle_generation = Arc::clone(&generation);
        thread::Builder::new()
            .name("obelisk-text-shaping".into())
            .spawn(move || {
                let mut fonts = WorkerFonts::new(fonts::DEFAULT_CHAIN);
                while let Ok(request) = rx.recv() {
                    match request {
                        Request::Shape(req, glyphs, reply) => {
                            let family = fonts.family_for(req.font.as_ref(), &generation);
                            // A dropped receiver just means the result is discarded.
                            let _ = reply.send(shape(&mut fonts.font_system, &family, &req, glyphs));
                        }
                        Request::EnsureFamily(asked, reply) => {
                            fonts.family_for(Some(&asked), &generation);
                            let _ = reply.send(());
                        }
                        Request::FontChainData(reply) => {
                            let _ = reply.send(fonts.chain_data.clone());
                        }
                        Request::SetChain(chain, reply) => {
                            let borrowed: Vec<&str> = chain.iter().map(String::as_str).collect();
                            fonts = WorkerFonts::new(&borrowed);
                            generation.fetch_add(1, Ordering::Release);
                            let _ = reply.send(());
                        }
                        #[cfg(test)]
                        Request::ResolvedPrimaryFamily(reply) => {
                            let _ = reply.send(fonts.primary_family.clone());
                        }
                    }
                }
            })
            .expect("failed to spawn obelisk-text-shaping thread");
        Self {
            requests: tx,
            cache: Arc::new(Mutex::new(HashMap::new())),
            generation: handle_generation,
            ensured: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Measures `request`, from [`SHAPE_CACHE_CAPACITY`]'s memo when asked before and from the
    /// worker thread otherwise, blocking only on a miss. Nothing invalidates an entry while the
    /// chain stands; [`ShapingHandle::set_chain`] is the one thing that can, and it clears the
    /// whole map rather than deciding which measurements the new faces would have changed.
    ///
    /// `Scene::apply` re-measures every text node it resolves, and ADR-0044 decision 2's dirty
    /// flag turned that from once per config edit into once per push. On a 500-row list, this memo
    /// takes 6.64ms off a 10.61ms pass, and the same 6.64ms off the 12.78ms variant that also
    /// draws an icon: the saving is measurement, not something else moving. Panics on a miss if
    /// the worker thread has died: a bug, not a recoverable state. A hit never reaches the worker,
    /// so it keeps answering after death, correct rather than lucky, but the next new string, not
    /// the next call, is the first symptom.
    pub fn shape(&self, request: ShapeRequest) -> ShapeResult {
        self.shape_keyed(request, false)
    }

    /// [`shape`](Self::shape), keeping each line's glyphs for paint and link hit-testing (ADR-0211).
    pub fn shape_glyphs(&self, request: ShapeRequest) -> ShapeResult {
        self.shape_keyed(request, true)
    }

    /// Each line of `text` shaped alone with its glyphs and `runs` re-based onto it, paired with the
    /// byte it starts at (ADR-0211). Lines end where cosmic-text ends them, so paint draws the rows
    /// measurement counted.
    ///
    /// ponytail: paint asks every frame -- a memo hit that still allocates each line's key, and blocks on
    /// the worker after the memo clears. Upgrade path: carry the glyphs in the display list.
    pub fn shape_lines(
        &self,
        text: &str,
        runs: &[FontRun],
        font_size: f32,
        font: Option<&Arc<str>>,
    ) -> Vec<(usize, ShapeResult)> {
        // A trailing line ending opens one more, empty line, as `Buffer::set_text` does.
        let trailing = text.is_empty() || text.ends_with(['\n', '\r']);
        LineIter::new(text)
            .map(|(range, _)| range)
            .chain(trailing.then_some(text.len()..text.len()))
            .map(|range| {
                let runs = runs
                    .iter()
                    .filter_map(|run| {
                        let (from, to) = (run.range.start.max(range.start), run.range.end.min(range.end));
                        (from < to).then(|| FontRun {
                            range: from - range.start..to - range.start,
                            bold: run.bold,
                            italic: run.italic,
                        })
                    })
                    .collect();
                let shaped = self.shape_glyphs(ShapeRequest {
                    text: text[range.clone()].to_string(),
                    font_size,
                    line_height: line_height(font_size),
                    max_width: None,
                    runs,
                    font: font.cloned(),
                });
                (range.start, shaped)
            })
            .collect()
    }

    fn shape_keyed(&self, request: ShapeRequest, glyphs: bool) -> ShapeResult {
        // The text moves into the key rather than being cloned into it, so a hit allocates
        // nothing and only a miss pays for the copy the worker needs.
        let key = ShapeKey {
            text: request.text,
            font_size: request.font_size.to_bits(),
            line_height: request.line_height.to_bits(),
            max_width: request.max_width.map(f32::to_bits),
            runs: request.runs,
            font: request.font,
            glyphs,
        };
        // A poisoned lock is recovered rather than propagated: this map is a pure memo, so no
        // invariant can break, and an unrelated thread's death shouldn't kill text measurement.
        if let Some(hit) = self.cache.lock().unwrap_or_else(PoisonError::into_inner).get(&key) {
            return hit.clone();
        }

        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests
            .send(Request::Shape(
                ShapeRequest {
                    text: key.text.clone(),
                    font_size: f32::from_bits(key.font_size),
                    line_height: f32::from_bits(key.line_height),
                    max_width: key.max_width.map(f32::from_bits),
                    runs: key.runs.clone(),
                    font: key.font.clone(),
                },
                key.glyphs,
                reply_tx,
            ))
            .expect("obelisk-text-shaping worker thread died");
        let result = reply_rx.recv().expect("obelisk-text-shaping worker thread died before replying");

        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        // Cleared wholesale rather than evicted one at a time: an LRU needs a recency order
        // maintained on every hit, work on the path this exists to make cheap.
        //
        // ponytail: a working set genuinely larger than the cap would re-shape everything every
        // pass. The largest list measured is 500 rows; the fix if that changes is an LRU.
        if cache.len() >= SHAPE_CACHE_CAPACITY {
            cache.clear();
        }
        cache.insert(key, result.clone());
        result
    }

    /// Replaces the font chain every reader measures and paints against, and clears the
    /// measurement cache, since every entry was measured against the faces this call replaces.
    /// Called once, after startup evaluation, with whatever chain the config declared. Works by
    /// ordering, not respawn: `ShapingHandle::spawn` runs before any Lua has been read
    /// (`wayland/mod.rs`), and `TextPainter` is built lazily on a surface's first paint, after, so
    /// femtovg picks up the new faces once the caller drops any painter it already built.
    ///
    /// A no-op for an empty chain: a config declaring no fonts keeps [`fonts::DEFAULT_CHAIN`],
    /// since a chain no font on the system can honour would otherwise leave `TextPainter::new`
    /// with nothing to load and the shell with no text.
    pub fn set_chain(&self, chain: &[String]) {
        if chain.is_empty() {
            return;
        }
        self.cache.lock().unwrap_or_else(PoisonError::into_inner).clear();
        // The worker drops its own family memo with the database those families resolved against,
        // so this side must forget them too or `ensure_family` would skip re-resolving one.
        self.ensured.lock().unwrap_or_else(PoisonError::into_inner).clear();
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests
            .send(Request::SetChain(chain.to_vec(), reply_tx))
            .expect("obelisk-text-shaping worker thread died");
        let _ = reply_rx.recv();
    }

    /// How many measurements are memoized. Test-only: exposing this in production would invite a
    /// caller to reason about cache state instead of treating [`shape`](Self::shape) as pure.
    #[cfg(test)]
    fn cached_len(&self) -> usize {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    /// Entry count and a byte floor for `wayland::memory_profile`, which reports rather than
    /// decides: no branch may read this, for `cached_len`'s reason. Sums the heap each entry owns
    /// -- the key's text and font runs, the result's lines and ranges -- and not the `HashMap`'s
    /// own table, so it under-reports and is labelled `approx` in the report. `Arc` contents count
    /// once per entry even when two entries share one, which cannot happen here: `shape` builds a
    /// fresh `ShapeResult` per miss.
    pub fn census(&self) -> (usize, usize) {
        let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        let bytes: usize = cache
            .iter()
            .map(|(key, result)| {
                let key_bytes = key.text.len() + key.runs.len() * std::mem::size_of::<FontRun>();
                let line_bytes: usize = result.lines.iter().map(String::len).sum();
                let range_bytes = result.line_ranges.len() * std::mem::size_of::<Range<usize>>();
                let glyph_bytes: usize = result
                    .shaped
                    .iter()
                    .map(|line| std::mem::size_of::<ShapedLine>() + std::mem::size_of_val(&*line.glyphs))
                    .sum();
                std::mem::size_of::<ShapeKey>()
                    + std::mem::size_of::<ShapeResult>()
                    + key_bytes
                    + line_bytes
                    + range_bytes
                    + glyph_bytes
            })
            .sum();
        (cache.len(), bytes)
    }

    /// Every loaded face's shared bytes, for `text::atlas::TextPainter` to register (ADR-0211).
    /// Cloning `FontData` clones an `Arc`, so repeated calls are cheap and copy no font file.
    pub fn font_chain_data(&self) -> Vec<FontFace> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests.send(Request::FontChainData(reply_tx)).expect("obelisk-text-shaping worker thread died");
        reply_rx.recv().expect("obelisk-text-shaping worker thread died before replying")
    }

    /// Resolves `family` without measuring anything, so a node that names one is drawn in it even
    /// when nothing ever measures that node (ADR-0144).
    ///
    /// Needed because measurement is not guaranteed. A `text` with an explicit width *and* height
    /// never reaches taffy's measure callback, and without `elide` or `wrap` never reaches
    /// `layout::scene::fit_text_to_box`'s shaping either -- so its family would never be loaded and
    /// paint would fall back to the declared chain. `layout::scene` calls this for every named
    /// family it prepares.
    ///
    /// One channel round trip per distinct family for the life of a chain, and a hash lookup after
    /// that; `set_chain` clears the memo along with the worker's.
    pub fn ensure_family(&self, family: &Arc<str>) {
        if self.ensured.lock().unwrap_or_else(PoisonError::into_inner).contains(family) {
            return;
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests
            .send(Request::EnsureFamily(Arc::clone(family), reply_tx))
            .expect("obelisk-text-shaping worker thread died");
        let _ = reply_rx.recv();
        self.ensured.lock().unwrap_or_else(PoisonError::into_inner).insert(Arc::clone(family));
    }

    /// How many times the loaded face set has changed (ADR-0144). The painter records this
    /// alongside the faces it registered with femtovg and re-syncs when the two differ, which is
    /// what makes a family first named at runtime reach paint as well as measurement.
    ///
    /// Read once a frame, so it is an atomic rather than a request: hearing "nothing changed"
    /// must not cost a channel round trip and a worker wake-up.
    pub fn font_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Returns the font chain's resolved primary family: the same name `shape()` asks
    /// cosmic-text for via `Family::Name`. Test-only. See `Request::ResolvedPrimaryFamily`.
    #[cfg(test)]
    pub fn resolved_primary_family(&self) -> String {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests.send(Request::ResolvedPrimaryFamily(reply_tx)).expect("obelisk-text-shaping worker thread died");
        reply_rx.recv().expect("obelisk-text-shaping worker thread died before replying")
    }
}

/// Everything the worker thread knows about fonts: the declared chain's database and primary, the
/// families nodes have named so far, and the face list femtovg is handed.
///
/// One value rather than five locals in the loop, because replacing the chain has to replace all of
/// them together: a `families` map kept across a `set_chain` would point at faces that went down
/// with the old database.
struct WorkerFonts {
    font_system: FontSystem,
    primary_family: String,
    /// What each family a node named resolved to, or `None` for one nothing answered. Both
    /// outcomes are remembered, so a font that is simply not installed costs one `fc-match` rather
    /// than one per measurement.
    families: HashMap<Arc<str>, Option<String>>,
    /// Every file already loaded, so `load_family` can tell a new one from a repeat.
    loaded_paths: HashSet<PathBuf>,
    /// What `font_chain_data` last produced: the faces `TextPainter` registers with femtovg.
    chain_data: Vec<FontFace>,
}

impl WorkerFonts {
    fn new(chain: &[&str]) -> Self {
        let ResolvedFonts { mut db, primary_family, loaded_paths } = fonts::resolve_chain(chain);
        // Mapped once, before the `Database` reaches cosmic-text, so the mappings stay *in* the
        // database instead of being mapped twice.
        let families = HashMap::new();
        let chain_data = font_chain_data(&mut db);
        let font_system = FontSystem::new_with_locale_and_db(detect_locale(), db);
        Self { font_system, primary_family, families, loaded_paths, chain_data }
    }

    /// The family name to shape `asked` under, resolving it on first sight (ADR-0144).
    ///
    /// A new family is loaded into the database the declared chain already filled, so that chain's
    /// CJK and emoji faces stay available as fallback behind it, and `generation` is bumped so the
    /// painter knows to register the new faces before drawing with them.
    ///
    /// A family nothing on the system answers resolves to the declared chain's own primary. That is
    /// what makes a typo draw the text in the wrong face rather than not at all.
    fn family_for(&mut self, asked: Option<&Arc<str>>, generation: &AtomicU64) -> String {
        let Some(asked) = asked else {
            return self.primary_family.clone();
        };
        if !self.families.contains_key(asked) {
            let hit = fonts::load_family(self.font_system.db_mut(), asked, &mut self.loaded_paths);
            self.families.insert(Arc::clone(asked), hit);
            self.chain_data = font_chain_data(self.font_system.db_mut());
            generation.fetch_add(1, Ordering::Release);
        }
        self.families[asked].clone().unwrap_or_else(|| self.primary_family.clone())
    }
}

fn shape(font_system: &mut FontSystem, primary_family: &str, request: &ShapeRequest, glyphs: bool) -> ShapeResult {
    let metrics = Metrics::new(request.font_size, request.line_height);
    let mut buffer = Buffer::new(font_system, metrics);
    buffer.set_size(request.max_width, None);
    let attrs = Attrs::new().family(Family::Name(primary_family));
    if request.runs.is_empty() {
        buffer.set_text(&request.text, &attrs, Shaping::Advanced, None);
    } else {
        buffer.set_rich_text(rich_spans(&request.text, &request.runs, &attrs), &attrs, Shaping::Advanced, None);
    }
    buffer.shape_until_scroll(font_system, false);

    // Where each paragraph starts in the source. cosmic-text splits the text into one
    // `BufferLine` per paragraph and a layout run's glyph offsets count from *its* paragraph, so
    // turning them into offsets into the whole string means adding the paragraph's own start --
    // its predecessors' text plus whichever line ending each of them was split on.
    let mut paragraph_starts = Vec::with_capacity(buffer.lines.len());
    let mut cursor = 0usize;
    for line in &buffer.lines {
        paragraph_starts.push(cursor);
        cursor += line.text().len() + line.ending().as_str().len();
    }

    let mut width = 0.0f32;
    let mut lines: Vec<String> = Vec::new();
    let mut line_ranges: Vec<Range<usize>> = Vec::new();
    let mut shaped: Vec<ShapedLine> = Vec::new();
    for run in buffer.layout_runs() {
        width = width.max(run.line_w);
        // `run.text` is cosmic-text's "original text line" -- the whole source paragraph, handed
        // back again for every visual line the wrap broke it into. The glyphs are what say which
        // slice of it this run is. Read as min/max over the cluster indices rather than as the
        // first and last glyph's, because a bidi run's glyphs come in visual order and its byte
        // range is not theirs to be sorted by.
        let (start, slice) = match (run.glyphs.iter().map(|g| g.start).min(), run.glyphs.iter().map(|g| g.end).max()) {
            (Some(start), Some(end)) => (start, &run.text[start..end]),
            // A blank line carries no glyphs and still takes up its height.
            _ => (0, ""),
        };
        // Trailing whitespace only: a word wrap leaves the break's space on the line it broke, and
        // a trailing space shifts a centred or right-aligned line by its own advance. Leading
        // space is the author's own indentation and stays.
        let trimmed = slice.trim_end();
        let paragraph_start = paragraph_starts.get(run.line_i).copied().unwrap_or(0);
        line_ranges.push(paragraph_start + start..paragraph_start + start + trimmed.len());
        lines.push(trimmed.to_string());

        // Positions from the line's own left edge: paint places the line by its alignment.
        let left = run.glyphs.iter().map(|glyph| glyph.x).fold(f32::INFINITY, f32::min);
        let placed = match glyphs {
            true => run
                .glyphs
                .iter()
                .map(|glyph| Glyph {
                    face: glyph.font_id,
                    weight: glyph.font_weight.0,
                    id: glyph.glyph_id,
                    x: glyph.x + glyph.x_offset * glyph.font_size - left,
                    y: glyph.y - glyph.y_offset * glyph.font_size,
                    advance: glyph.w,
                    start: paragraph_start + glyph.start,
                })
                .collect(),
            false => Box::default(),
        };
        shaped.push(ShapedLine {
            rtl: run.rtl,
            width: run.line_w,
            baseline: run.line_y - run.line_top,
            glyphs: placed,
        });
    }

    ShapeResult {
        width,
        height: lines.len() as f32 * metrics.line_height,
        lines: lines.into(),
        line_ranges: line_ranges.into(),
        shaped: shaped.into(),
    }
}

/// `text` as the `(slice, attrs)` spans `Buffer::set_rich_text` takes: each run in a face of its
/// own weight and style, and the text between runs in `base`. A run reaching past the end of the
/// text, or one that would start before the previous ended, is clamped rather than refused: the
/// ranges are built by `layout` from the same string, so neither happens, and a shaper that panics
/// on a range is a worse outcome than one that measures a character in the wrong weight.
fn rich_spans<'t, 'a>(text: &'t str, runs: &[FontRun], base: &Attrs<'a>) -> Vec<(&'t str, Attrs<'a>)> {
    let mut spans = Vec::with_capacity(runs.len() * 2 + 1);
    let mut cursor = 0usize;
    for run in runs {
        let start = run.range.start.clamp(cursor, text.len());
        let end = run.range.end.clamp(start, text.len());
        if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            continue;
        }
        if start > cursor {
            spans.push((&text[cursor..start], base.clone()));
        }
        let mut attrs = base.clone();
        attrs.weight = if run.bold { Weight::BOLD } else { Weight::NORMAL };
        attrs.style = if run.italic { Style::Italic } else { Style::Normal };
        spans.push((&text[start..end], attrs));
        cursor = end;
    }
    if cursor < text.len() {
        spans.push((&text[cursor..], base.clone()));
    }
    spans
}

/// Every face in the database, for femtovg to load (ADR-0211). Paint draws the faces cosmic-text
/// chose, and cosmic-text may choose any face the database holds -- a collection's second face, a
/// bold, a named family's -- so the painter is handed all of them. The database holds only the
/// chain's files and the families nodes named, so this maps no file nothing asked for.
///
/// Maps rather than reads, the entire memory story here: `Noto Color Emoji` is an 11MB CBDT bitmap
/// font, and the `data.to_vec()` this replaces held it three times over (worker `Vec<Vec<u8>>`,
/// femtovg's `add_font_mem` copy, cosmic-text's own mapping); dropping it on an idle eleven-surface
/// session cut the Renderer's private-dirty memory from 49.7MB to 22.7MB. `make_shared_face_data`
/// rewrites every face sharing the path to `Source::SharedFile`, so this is also cosmic-text's map,
/// and asking it for two faces of one file maps the file once.
///
/// SAFETY: `make_shared_face_data` is `unsafe` because a font file rewritten on disk changes
/// under the mapping, which can fault or produce nonsense glyphs. That is the same bargain
/// cosmic-text already makes internally for every font it renders, and the alternative is paying
/// a private copy per font per process to defend against someone editing a system font in place.
///
/// A face whose mapping cannot be established is skipped rather than fatal, matching
/// `resolve_chain`'s own treatment of an entry it can't honor: losing the emoji font is a missing
/// glyph, not a dead shell.
fn font_chain_data(db: &mut fontdb::Database) -> Vec<FontFace> {
    // Collected first: `make_shared_face_data` needs `&mut db`, so nothing may be borrowing it.
    let ids: Vec<fontdb::ID> = db.faces().map(|face| face.id).collect();
    let mut data = Vec::with_capacity(ids.len());
    for id in ids {
        // SAFETY: mapping a font file the process does not own, as the doc comment above spells
        // out. A rewrite in place changes the bytes under the mapping. Same bargain cosmic-text
        // already makes for every font it renders.
        match unsafe { db.make_shared_face_data(id) } {
            Some((bytes, index)) => data.push(FontFace { data: FontData(bytes), index, id }),
            None => eprintln!("font chain: face {id:?} could not be mapped, skipped"),
        }
    }
    data
}

/// The process locale, read the way glibc's own env-var chain does: `LC_ALL`, then `LC_CTYPE`,
/// then `LANG`, defaulting to `"en-US"` when none are set (or all name `"C"`/`"POSIX"`). Replaces
/// the `sys_locale` lookup `FontSystem::new()` does internally, now that construction bypasses it.
fn detect_locale() -> String {
    let raw = env::var("LC_ALL").or_else(|_| env::var("LC_CTYPE")).or_else(|_| env::var("LANG")).unwrap_or_default();

    // `en_US.UTF-8@euro` -> `en-US`: cosmic-text's fallback tables key off the language and
    // region subtags, not the encoding or modifier, so both are dropped rather than parsed.
    let without_modifier = raw.split('@').next().unwrap_or("");
    let without_encoding = without_modifier.split('.').next().unwrap_or("");
    let normalized = without_encoding.replace('_', "-");

    if normalized.is_empty() || normalized.eq_ignore_ascii_case("C") || normalized.eq_ignore_ascii_case("POSIX") {
        "en-US".to_string()
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(text: &str, font_size: f32) -> ShapeRequest {
        ShapeRequest {
            text: text.into(),
            font_size,
            line_height: line_height(font_size),
            max_width: None,
            runs: Vec::new(),
            font: None,
        }
    }

    fn req_in(text: &str, font_size: f32, family: Option<&str>) -> ShapeRequest {
        ShapeRequest { font: family.map(Arc::from), ..req(text, font_size) }
    }

    /// Two families both installed on this machine, so a test can tell one from the other. `None`
    /// when the machine has only one, which is a skip rather than a failure.
    fn two_families() -> Option<(&'static str, &'static str)> {
        if !fonts::fc_match_available() {
            return None;
        }
        let (declared, named) = ("Noto Sans", "Noto Sans Mono");
        (fonts::family_installed(declared) && fonts::family_installed(named)).then_some((declared, named))
    }

    /// A family a node names joins the faces femtovg is handed, so glyphs shaped in it can be
    /// drawn (ADR-0211).
    #[test]
    fn a_family_named_by_a_node_joins_the_faces_the_painter_is_handed() {
        let Some((declared, named)) = two_families() else {
            eprintln!("skip: need two installed families to tell apart");
            return;
        };
        let handle = ShapingHandle::spawn();
        handle.set_chain(&[declared.to_string()]);
        assert!(named_face_is_handed(&handle, named));
    }

    /// The hole `ensure_family` exists for: a node whose box is fully specified is never measured,
    /// so nothing would resolve its family and paint would fall back to the declared chain.
    #[test]
    fn a_family_can_be_resolved_without_measuring_anything_in_it() {
        let Some((declared, named)) = two_families() else {
            eprintln!("skip: need two installed families to tell apart");
            return;
        };
        let handle = ShapingHandle::spawn();
        handle.set_chain(&[declared.to_string()]);
        let before = handle.font_generation();

        handle.ensure_family(&Arc::from(named));

        assert!(handle.font_generation() > before, "the painter has to hear about the face");
        assert_eq!(handle.cached_len(), 0, "resolving a family measures nothing");
        assert!(named_face_is_handed(&handle, named), "and the face has to be in the list femtovg is handed");
    }

    /// Whether `named` shapes in a face of its own, and that face is one femtovg is handed.
    fn named_face_is_handed(handle: &ShapingHandle, named: &str) -> bool {
        let face = |family| handle.shape_glyphs(req_in("hello", 20.0, family)).shaped[0].glyphs[0].face;
        let named_face = face(Some(named));
        named_face != face(None) && handle.font_chain_data().iter().any(|candidate| candidate.id == named_face)
    }

    /// The client-side memo must not outlive the database its families resolved against, or a
    /// `set_chain` would leave `ensure_family` skipping a family the worker has forgotten.
    #[test]
    fn replacing_the_chain_makes_ensure_family_resolve_again() {
        let Some((declared, named)) = two_families() else {
            eprintln!("skip: need two installed families to tell apart");
            return;
        };
        let handle = ShapingHandle::spawn();
        handle.set_chain(&[declared.to_string()]);
        handle.ensure_family(&Arc::from(named));

        handle.set_chain(&[declared.to_string()]);
        let after_reset = handle.font_generation();
        handle.ensure_family(&Arc::from(named));

        assert!(handle.font_generation() > after_reset, "the family must be loaded into the new database");
        assert!(
            named_face_is_handed(&handle, named),
            "or a node naming it would paint in the declared chain after a chain change"
        );
    }

    /// Loading a family the painter has not registered yet is only safe because the painter is
    /// told to catch up; the generation is how it finds out.
    #[test]
    fn resolving_a_new_family_bumps_the_generation_and_resolving_it_again_does_not() {
        let Some((declared, named)) = two_families() else {
            eprintln!("skip: need two installed families to tell apart");
            return;
        };
        let handle = ShapingHandle::spawn();
        handle.set_chain(&[declared.to_string()]);
        let before = handle.font_generation();

        handle.shape(req_in("hello", 20.0, Some(named)));
        let after_first = handle.font_generation();
        assert!(after_first > before, "the painter has to hear about a face it does not hold");

        handle.shape(req_in("different text entirely", 20.0, Some(named)));
        assert_eq!(handle.font_generation(), after_first, "a family already resolved is not reloaded");
    }

    /// Measuring the same string in two families has to reach different faces, or the box a node
    /// reserves is the declared family's width and the glyphs it draws will not fit it.
    #[test]
    fn a_named_family_measures_differently_from_the_declared_one() {
        let Some((declared, named)) = two_families() else {
            eprintln!("skip: need two installed families to tell apart");
            return;
        };
        let handle = ShapingHandle::spawn();
        handle.set_chain(&[declared.to_string()]);
        // Proportional against monospaced: "iiii" is much narrower in a sans than in a mono, which
        // no amount of cache-key confusion could produce on its own.
        let plain = handle.shape(req_in("iiii", 32.0, None)).width;
        let mono = handle.shape(req_in("iiii", 32.0, Some(named))).width;
        assert!(mono > plain, "monospaced {named:?} at {mono} should be wider than {declared:?} at {plain}");
    }

    /// The family is part of the cache key, so the first measurement cannot answer for another
    /// family. Without it whichever node measured first would poison every other family's box.
    #[test]
    fn a_measurement_in_one_family_is_not_served_to_another() {
        let Some((declared, named)) = two_families() else {
            eprintln!("skip: need two installed families to tell apart");
            return;
        };
        let handle = ShapingHandle::spawn();
        handle.set_chain(&[declared.to_string()]);
        let plain = handle.shape(req_in("iiii", 32.0, None));
        let mono = handle.shape(req_in("iiii", 32.0, Some(named)));
        assert_eq!(handle.cached_len(), 2, "the two families are separate entries, not one");
        assert_ne!(plain.width, mono.width);
    }

    /// A family nothing on the system answers draws in the declared chain rather than as nothing,
    /// and is resolved once -- an `fc-match` subprocess per frame for a missing font would sit on
    /// the measurement path.
    #[test]
    fn a_family_nothing_answers_measures_in_the_declared_chain_and_is_only_looked_up_once() {
        let handle = ShapingHandle::spawn();
        let missing = Some("ZZ No Such Family 9184");
        assert_eq!(handle.shape(req_in("Obelisk", 20.0, missing)).width, handle.shape(req("Obelisk", 20.0)).width);
        let generation = handle.font_generation();
        handle.shape(req_in("Obelisk Shell", 20.0, missing));
        assert_eq!(handle.font_generation(), generation, "a miss is remembered, not retried");
    }

    /// Replacing the chain invalidates the families resolved against the old database, or a node
    /// keeps pointing at faces that went down with it.
    #[test]
    fn replacing_the_chain_forgets_the_families_resolved_against_the_old_one() {
        let Some((declared, named)) = two_families() else {
            eprintln!("skip: need two installed families to tell apart");
            return;
        };
        let handle = ShapingHandle::spawn();
        handle.set_chain(&[declared.to_string()]);
        handle.shape(req_in("hello", 20.0, Some(named)));
        assert!(named_face_is_handed(&handle, named));

        handle.set_chain(&[named.to_string()]);
        // Face ids restart with a new database, so compare against a chain that never knew the old
        // family rather than against the old ids.
        let fresh = ShapingHandle::spawn();
        fresh.set_chain(&[named.to_string()]);
        let sizes = |handle: &ShapingHandle| {
            let mut sizes: Vec<usize> = handle.font_chain_data().iter().map(|face| face.data.as_ref().len()).collect();
            sizes.sort_unstable();
            sizes
        };
        assert_eq!(sizes(&handle), sizes(&fresh), "the new database holds no family the old one had resolved");
    }

    /// An installed family other than `primary`, so a declared chain has something to move to.
    fn another_family(primary: &str) -> Option<String> {
        ["DejaVu Sans Mono", "Liberation Mono", "Noto Sans Mono"]
            .into_iter()
            .find(|family| *family != primary && fonts::family_installed(family))
            .map(str::to_string)
    }

    /// The chain a config declares has to reach the worker, or `fonts { ... }` is a no-op that
    /// looks like it worked. Skips rather than fails on a machine with no second family installed.
    #[test]
    fn a_declared_chain_replaces_the_one_the_worker_started_with() {
        let handle = ShapingHandle::spawn();
        let Some(wanted) = another_family(&handle.resolved_primary_family()) else {
            eprintln!("skip: no second family installed, so the chain could not change");
            return;
        };
        handle.set_chain(std::slice::from_ref(&wanted));
        assert_eq!(handle.resolved_primary_family(), wanted, "the worker measures against what the config asked for");
    }

    /// `font_chain_data` is what `TextPainter::new` loads into femtovg, so a `set_chain` that moved
    /// the measuring side and not this one would measure text against one font and paint it with
    /// another (ADR-0043 decision 2).
    #[test]
    fn a_declared_chain_reaches_the_faces_femtovg_paints_with_too() {
        let handle = ShapingHandle::spawn();
        let Some(wanted) = another_family(&handle.resolved_primary_family()) else {
            eprintln!("skip: no second family installed, so nothing could change");
            return;
        };
        let before: Vec<usize> = handle.font_chain_data().iter().map(|data| data.data.as_ref().len()).collect();
        handle.set_chain(&[wanted]);
        let after: Vec<usize> = handle.font_chain_data().iter().map(|data| data.data.as_ref().len()).collect();
        assert!(!after.is_empty(), "a one-family chain loads that family's faces");
        assert_ne!(after, before, "the bytes femtovg would load must be the new font's, not the old chain's");
        assert!(after[0] > 0, "and they are real bytes, not an empty mapping");
    }

    // ---- styled runs (ADR-0104) ----

    /// Paint draws the faces cosmic-text chose, so every face a shape names has to be one the
    /// painter is handed: the regular, a bold run's, and coverage like emoji (ADR-0211).
    #[test]
    fn every_face_a_shape_names_is_one_the_painter_is_handed() {
        let handle = ShapingHandle::spawn();
        let faces: Vec<fontdb::ID> = handle.font_chain_data().iter().map(|face| face.id).collect();
        let text = "Obelisk 🙏 شكرا";
        let bold = ShapeRequest { runs: vec![FontRun { range: 0..7, bold: true, italic: false }], ..req(text, 20.0) };
        for shaped in [handle.shape_glyphs(req(text, 20.0)), handle.shape_glyphs(bold)] {
            for glyph in shaped.shaped.iter().flat_map(|line| line.glyphs.iter()) {
                assert!(faces.contains(&glyph.face), "{glyph:?} names a face femtovg is never given");
            }
        }
    }

    /// A right-to-left paragraph says so, and lays the letter it starts with out rightmost.
    #[test]
    fn a_right_to_left_line_reports_its_direction_and_starts_at_the_right() {
        let handle = ShapingHandle::spawn();
        let shaped = handle.shape_glyphs(req("اول one", 20.0));
        let line = &shaped.shaped[0];
        assert!(line.rtl, "an Arabic first letter makes the paragraph right to left");
        let first = line.glyphs.iter().find(|glyph| glyph.start == 0).expect("a glyph for the first letter");
        assert!(line.glyphs.iter().all(|glyph| glyph.x <= first.x), "and that letter is drawn rightmost");
        assert!(!handle.shape(req("one اول", 20.0)).shaped[0].rtl);
    }

    /// ADR-0211: femtovg dropped `خامس`'s last letter and overlapped glyphs at a direction change.
    #[test]
    fn a_mixed_direction_line_has_a_glyph_per_letter_and_none_overlap() {
        let handle = ShapingHandle::spawn();
        let text = "ثالث three خامس four";
        let mut glyphs = handle.shape_glyphs(req(text, 20.0)).shaped[0].glyphs.to_vec();
        for (start, _) in text.char_indices() {
            assert!(glyphs.iter().any(|glyph| glyph.start == start), "no glyph for byte {start} of {text:?}");
        }
        glyphs.sort_by(|a, b| a.x.total_cmp(&b.x));
        for pair in glyphs.windows(2) {
            assert!(pair[0].x + pair[0].advance <= pair[1].x + 0.01, "{:?} overlaps {:?}", pair[0], pair[1]);
        }
    }

    #[test]
    fn a_bold_run_measures_wider_than_the_same_text_regular() {
        let handle = ShapingHandle::spawn();
        let plain = handle.shape_glyphs(req("Obelisk Shell Renderer", 20.0));
        let bold = handle.shape_glyphs(ShapeRequest {
            runs: vec![FontRun { range: 0..21, bold: true, italic: false }],
            ..req("Obelisk Shell Renderer", 20.0)
        });
        if bold.shaped[0].glyphs[0].face == plain.shaped[0].glyphs[0].face {
            eprintln!("skip: the default chain's family has no bold face installed");
            return;
        }
        assert!(bold.width > plain.width, "bold {} should be wider than regular {}", bold.width, plain.width);
    }

    /// The ranges are the whole point: a wrapped line has to say which bytes of the source it
    /// holds so `layout` can carry the runs across the break, including past an explicit newline.
    #[test]
    fn each_line_range_slices_the_source_to_exactly_that_line() {
        let handle = ShapingHandle::spawn();
        let text = "first paragraph that wraps\nsecond";
        let result = handle.shape(ShapeRequest {
            text: text.into(),
            font_size: 14.0,
            line_height: line_height(14.0),
            max_width: Some(90.0),
            runs: Vec::new(),
            font: None,
        });
        assert_eq!(result.lines.len(), result.line_ranges.len());
        assert!(result.lines.len() >= 3, "the first paragraph wraps and the second is its own line");
        for (line, range) in result.lines.iter().zip(result.line_ranges.iter()) {
            assert_eq!(&text[range.clone()], line.as_str(), "range {range:?} must slice to its line");
        }
        assert_eq!(result.lines.last().map(String::as_str), Some("second"));
        assert_eq!(result.line_ranges.last().cloned(), Some(27..33));
    }

    #[test]
    fn an_empty_declaration_leaves_the_default_chain_standing() {
        let handle = ShapingHandle::spawn();
        let before = handle.resolved_primary_family();
        handle.set_chain(&[]);
        assert_eq!(handle.resolved_primary_family(), before, "declaring nothing is not declaring an empty chain");
    }

    /// Every entry was measured against faces the new chain replaces, so keeping any of them would
    /// be answering with the old font's metrics under the new font's name.
    #[test]
    fn setting_a_chain_drops_every_measurement_taken_under_the_old_one() {
        let handle = ShapingHandle::spawn();
        handle.shape(req("Obelisk", 14.0));
        assert_eq!(handle.cached_len(), 1);
        handle.set_chain(&["Noto Sans".to_string()]);
        assert_eq!(handle.cached_len(), 0);
    }

    #[test]
    fn the_same_request_is_measured_once_and_remembered() {
        let handle = ShapingHandle::spawn();
        let first = handle.shape(req("network access point 1", 13.0));
        assert_eq!(handle.cached_len(), 1);
        let second = handle.shape(req("network access point 1", 13.0));
        assert_eq!(handle.cached_len(), 1, "asking again must not add a second entry");
        assert_eq!(first, second);
    }

    /// The bug a subset key would cause: a hit that returns another request's box. Each of these
    /// differs from the first in exactly one field, including `line_height`, which every caller
    /// derives from `font_size` today and which a three-field key would therefore have dropped.
    #[test]
    fn every_field_of_a_request_is_part_of_its_identity() {
        let handle = ShapingHandle::spawn();
        handle.shape(req("abc", 13.0));
        for (label, request) in [
            (
                "text",
                ShapeRequest {
                    text: "abd".into(),
                    font_size: 13.0,
                    line_height: 15.6,
                    max_width: None,
                    runs: Vec::new(),
                    font: None,
                },
            ),
            (
                "font_size",
                ShapeRequest {
                    text: "abc".into(),
                    font_size: 26.0,
                    line_height: 15.6,
                    max_width: None,
                    runs: Vec::new(),
                    font: None,
                },
            ),
            (
                "line_height",
                ShapeRequest {
                    text: "abc".into(),
                    font_size: 13.0,
                    line_height: 40.0,
                    max_width: None,
                    runs: Vec::new(),
                    font: None,
                },
            ),
            (
                "max_width",
                ShapeRequest {
                    text: "abc".into(),
                    font_size: 13.0,
                    line_height: 15.6,
                    max_width: Some(10.0),
                    runs: Vec::new(),
                    font: None,
                },
            ),
        ] {
            let before = handle.cached_len();
            handle.shape(request);
            assert_eq!(handle.cached_len(), before + 1, "a request differing only in `{label}` must not hit");
        }
    }

    /// A clock is why this bound exists: it shapes a string nobody asks for twice, once a second,
    /// forever. Driven here with distinct strings rather than by waiting.
    #[test]
    fn the_cache_clears_rather_than_growing_past_its_cap() {
        let handle = ShapingHandle::spawn();
        for i in 0..SHAPE_CACHE_CAPACITY {
            handle.shape(req(&format!("{i}"), 13.0));
        }
        assert_eq!(handle.cached_len(), SHAPE_CACHE_CAPACITY, "the cap is where it clears, not before");
        handle.shape(req("one too many", 13.0));
        assert_eq!(handle.cached_len(), 1, "the map is dropped whole, keeping only the request that overflowed it");
    }

    #[test]
    fn a_clone_shares_the_cache_rather_than_starting_its_own() {
        let handle = ShapingHandle::spawn();
        let clone = handle.clone();
        handle.shape(req("shared", 13.0));
        clone.shape(req("shared", 13.0));
        assert_eq!(
            clone.cached_len(),
            1,
            "`wayland::App` and the client it owns must not measure the same string twice"
        );
    }

    /// Cheap to hold and worth pinning: a cached answer has to be the answer the worker gave, not
    /// merely some answer. Compared against a second handle, whose cache is cold.
    #[test]
    fn a_cached_measurement_equals_what_the_worker_returns_cold() {
        let warm = ShapingHandle::spawn();
        let first = warm.shape(req("Obelisk", 14.0));
        let cached = warm.shape(req("Obelisk", 14.0));
        let cold = ShapingHandle::spawn().shape(req("Obelisk", 14.0));
        assert_eq!(cached, first);
        assert_eq!(cached, cold);
    }

    #[test]
    fn shapes_nonempty_text_to_a_nonzero_box() {
        let handle = ShapingHandle::spawn();
        let result = handle.shape(ShapeRequest {
            text: "Obelisk".into(),
            font_size: 14.0,
            line_height: 18.0,
            max_width: None,
            runs: Vec::new(),
            font: None,
        });
        assert!(result.width > 0.0, "expected nonzero width, got {}", result.width);
        assert_eq!(result.height, 18.0);
    }

    #[test]
    fn empty_text_measures_to_zero_width() {
        let handle = ShapingHandle::spawn();
        let result = handle.shape(ShapeRequest {
            text: String::new(),
            font_size: 14.0,
            line_height: 18.0,
            max_width: None,
            runs: Vec::new(),
            font: None,
        });
        assert_eq!(result.width, 0.0);
    }

    #[test]
    fn longer_text_measures_wider_than_shorter_text() {
        let handle = ShapingHandle::spawn();
        let short = handle.shape(ShapeRequest {
            text: "O".into(),
            font_size: 14.0,
            line_height: 18.0,
            max_width: None,
            runs: Vec::new(),
            font: None,
        });
        let long = handle.shape(ShapeRequest {
            text: "Obelisk Shell".into(),
            font_size: 14.0,
            line_height: 18.0,
            max_width: None,
            runs: Vec::new(),
            font: None,
        });
        assert!(long.width > short.width);
    }

    #[test]
    fn font_chain_data_is_all_loadable_fonts() {
        let handle = ShapingHandle::spawn();
        let chain = handle.font_chain_data();
        assert!(!chain.is_empty(), "the default chain must resolve to at least one loaded face");
        use cosmic_text::skrifa::{FontRef, raw::TableProvider};
        for data in &chain {
            let font = FontRef::from_index(data.data.as_ref(), data.index)
                .expect("every chain entry's bytes should parse as a font face");
            assert!(
                font.head().is_ok() && font.hhea().is_ok() && font.maxp().is_ok(),
                "every chain entry should have the head, hhea and maxp tables a face needs"
            );
        }
    }

    /// The point of `FontData`: two calls hand back the same mapping rather than two copies of
    /// the file. Compares the pointers the slices start at, since that is what "same pages" means
    /// -- equal *contents* would pass just as well for the copying implementation this replaced.
    #[test]
    fn two_asks_for_the_font_chain_share_one_mapping() {
        let handle = ShapingHandle::spawn();
        let first = handle.font_chain_data();
        let second = handle.font_chain_data();
        assert!(!first.is_empty(), "the default chain must resolve to at least one loaded face");
        assert_eq!(first.len(), second.len());
        for (a, b) in first.iter().zip(second.iter()) {
            assert_eq!(
                a.data.as_ref().as_ptr(),
                b.data.as_ref().as_ptr(),
                "each ask should share one mapping, not copy the file"
            );
        }
    }

    #[test]
    fn the_resolved_primary_family_is_findable_among_the_loaded_chain_fonts() {
        // Reconstructs a database from `font_chain_data()` -- the exact bytes
        // `text::atlas::TextPainter` loads into femtovg -- and queries it the way `shape()`
        // queries cosmic-text's. Comparing name records instead would pass for the wrong reason:
        // a font's `fontdb`-visible family and its raw TTF `FAMILY` record are different sources
        // of truth and aren't required to agree.
        let handle = ShapingHandle::spawn();
        let primary_family = handle.resolved_primary_family();

        let mut db = fontdb::Database::new();
        for data in handle.font_chain_data() {
            db.load_font_data(data.data.as_ref().to_vec());
        }

        let query = fontdb::Query { families: &[fontdb::Family::Name(&primary_family)], ..Default::default() };
        assert!(
            db.query(&query).is_some(),
            "primary_family {primary_family:?} is not findable in the loaded font chain"
        );
    }

    #[test]
    fn shape_measures_under_the_family_it_is_given() {
        // `shape()` is called twice against the *same* `FontSystem` -- built once, over a
        // database holding two Latin faces with very different metrics -- varying only the
        // `primary_family` argument. Holding the database fixed isolates that argument: varying
        // the chain instead (two separate `resolve_chain` calls) would also vary the database's
        // load order, so a width difference couldn't be pinned on the argument specifically.
        if !fonts::fc_match_available() {
            eprintln!("fc-match not available, skip");
            return;
        }
        if !fonts::family_installed("Noto Sans") || !fonts::family_installed("Noto Sans Mono") {
            eprintln!("\"Noto Sans\" and/or \"Noto Sans Mono\" not installed on this machine, skip");
            return;
        }

        let ResolvedFonts { db, .. } = fonts::resolve_chain(&["Noto Sans", "Noto Sans Mono"]);
        let mut font_system = FontSystem::new_with_locale_and_db(detect_locale(), db);

        let request = ShapeRequest {
            text: "Obelisk Shell Renderer".into(),
            font_size: 24.0,
            line_height: 28.8,
            max_width: None,
            runs: Vec::new(),
            font: None,
        };
        let proportional = shape(&mut font_system, "Noto Sans", &request, false);
        let monospace = shape(&mut font_system, "Noto Sans Mono", &request, false);

        // Near-equal widths would mean the family argument did nothing; 10% clears rounding noise.
        let diff = (proportional.width - monospace.width).abs();
        let tolerance = proportional.width.max(monospace.width) * 0.10;
        assert!(
            diff > tolerance,
            "\"Noto Sans\" measured {} and \"Noto Sans Mono\" measured {} for the same string at \
             the same size against the same database -- too close to prove `shape()` is actually \
             keying off the family argument rather than ignoring it",
            proportional.width,
            monospace.width
        );
    }

    /// The trap `lines` was written into: cosmic-text's `LayoutRun::text` is the whole *source*
    /// line, handed back once per visual line the wrap broke it into, so collecting it directly
    /// yields the entire string N times over. Only the glyph cluster indices delimit a run.
    #[test]
    fn each_wrapped_line_is_its_own_slice_and_not_the_whole_string_again() {
        const TEXT: &str = "Obelisk Shell Renderer";
        let handle = ShapingHandle::spawn();
        let unconstrained = handle.shape(req(TEXT, 14.0));
        assert_eq!(&*unconstrained.lines, [TEXT], "an unwrapped string is one line holding all of it");

        let wrapped = handle.shape(ShapeRequest {
            text: TEXT.into(),
            font_size: 14.0,
            line_height: line_height(14.0),
            max_width: Some(unconstrained.width / 2.0),
            runs: Vec::new(),
            font: None,
        });
        assert!(wrapped.lines.len() > 1, "expected a break, got {:?}", wrapped.lines);
        assert_eq!(
            wrapped.lines.join(" "),
            TEXT,
            "the lines have to rejoin to the source, not repeat it: {:?}",
            wrapped.lines
        );
    }

    /// Height is the line count times the step, so the two have to be derived from the same walk
    /// rather than counted twice.
    #[test]
    fn the_measured_height_is_the_lines_it_reports() {
        let handle = ShapingHandle::spawn();
        let result = handle.shape(ShapeRequest {
            text: "Obelisk Shell Renderer".into(),
            font_size: 14.0,
            line_height: 18.0,
            max_width: Some(40.0),
            runs: Vec::new(),
            font: None,
        });
        assert_eq!(result.height, result.lines.len() as f32 * 18.0);
    }

    /// A hard newline is a break the shaper already honoured for measurement and paint ignored,
    /// drawing the `\n` as a glyph. It arrives as two lines like any other break.
    #[test]
    fn an_explicit_newline_is_two_lines() {
        let handle = ShapingHandle::spawn();
        let result = handle.shape(req("first\nsecond", 14.0));
        assert_eq!(&*result.lines, ["first", "second"]);
    }

    #[test]
    fn a_max_width_narrower_than_the_unconstrained_text_wraps_to_more_lines() {
        let handle = ShapingHandle::spawn();
        let unconstrained = handle.shape(ShapeRequest {
            text: "Obelisk Shell Renderer".into(),
            font_size: 14.0,
            line_height: 18.0,
            max_width: None,
            runs: Vec::new(),
            font: None,
        });
        let wrapped = handle.shape(ShapeRequest {
            text: "Obelisk Shell Renderer".into(),
            font_size: 14.0,
            line_height: 18.0,
            max_width: Some(unconstrained.width / 2.0),
            runs: Vec::new(),
            font: None,
        });
        assert!(wrapped.height > unconstrained.height, "wrapping onto more lines must grow the measured height");
        assert!(wrapped.width <= unconstrained.width, "a wrapped line can't be wider than the unconstrained text");
    }
}
