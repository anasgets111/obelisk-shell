//! Off-thread wrapper around `cosmic-text`'s font shaping, so measuring a dynamic string never
//! blocks the render thread. The worker builds its `FontSystem` from `text::fonts::resolve_chain`'s
//! declared chain, not `FontSystem::new()`, which takes up to ~1s per cosmic-text's own docs
//! (ADR-0043 decision 2). A plain `std::thread` plus `mpsc` runs it rather than tokio's
//! multi-thread runtime: this is one dedicated CPU-bound worker, and renderer's Cargo.toml only
//! carries tokio's `rt`/`net`/`macros` features.
//!
//! `shape()` asks for `Family::Name(primary_family)`, the resolved chain's own first hit, so
//! `text::atlas::TextPainter` loads the same chain in the same order into femtovg: not theoretical,
//! since the old code measured under `Family::SansSerif` while a separate `fontdb` query for paint
//! missed that alias, falling back to the first face in scan order and measuring a `text` node's
//! box roughly 30% narrower than the glyphs painted into it.

use std::collections::HashMap;
use std::env;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread;

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping};

use super::fonts::{self, ResolvedFonts};

/// A shaping request: the text to measure and the metrics to shape it at.
pub struct ShapeRequest {
    pub text: String,
    pub font_size: f32,
    pub line_height: f32,
    /// Logical-pixel width to wrap at. `None` measures the text unconstrained, on one line, which
    /// is what every caller uses today.
    pub max_width: Option<f32>,
}

/// The measured result of shaping a request: its tight bounding box in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapeResult {
    pub width: f32,
    pub height: f32,
}

/// One font file's bytes, held once and shared by every reader. The inner `Arc` is `fontdb`'s:
/// `Database::make_shared_face_data` maps the file and rewrites every face from it to point at
/// the mapping, so cosmic-text (which owns the `Database`) and femtovg (handed this) read the
/// same `Shared_Clean`, page-cache-backed pages instead of each holding a copy. The newtype exists
/// because femtovg's `add_shared_font_with_index` takes `T: AsRef<[u8]> + 'static` by value, and
/// `Arc<dyn AsRef<[u8]>>` doesn't itself implement `AsRef<[u8]>`.
#[derive(Clone)]
pub struct FontData(std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>);

impl AsRef<[u8]> for FontData {
    fn as_ref(&self) -> &[u8] {
        (*self.0).as_ref()
    }
}

enum Request {
    Shape(ShapeRequest, mpsc::Sender<ShapeResult>),
    FontChainData(mpsc::Sender<Vec<FontData>>),
    /// Replace the chain this worker measures against, once the config has said what it wants
    /// (ADR-0043 decision 2). Replies once the new `FontSystem` is live, so the next
    /// `font_chain_data` call answers with the new faces.
    SetChain(Vec<String>, mpsc::Sender<()>),
    // Test-only (`#[cfg(test)]`, not `#[allow(dead_code)]`): nothing outside a test binary sends
    // this. `fonts::resolve_chain`'s own `eprintln!` is the production diagnostic.
    #[cfg(test)]
    ResolvedPrimaryFamily(mpsc::Sender<String>),
}

/// How many measured strings [`ShapingHandle`] remembers before it drops the lot. Sized against
/// the working set (the shipped dev config resolves about twenty text nodes, the largest list
/// this engine carries is a few hundred) with room for churn: a clock shapes a string nobody asks
/// for again every second, so an unbounded map would grow by 86,400 dead entries a day; this cap
/// clears roughly hourly instead. Full, the map holds on the order of 400KB (4096 short `String`
/// entries plus four integers and two floats, plus `HashMap` overhead), under one percent of
/// ADR-0043's 50MB-per-monitor budget.
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
}

impl ShapingHandle {
    /// Spawns the worker thread and its own `FontSystem`, created inside the spawned closure
    /// rather than moved into it, so this doesn't require `FontSystem: Send`.
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel::<Request>();
        thread::Builder::new()
            .name("oblisk-text-shaping".into())
            .spawn(move || {
                let ResolvedFonts { mut db, mut primary_family } = fonts::resolve_chain(fonts::DEFAULT_CHAIN);
                // Mapped once, before the `Database` reaches cosmic-text, so the mappings stay
                // *in* the database instead of being mapped twice.
                let mut chain_data = font_chain_data(&mut db);
                let mut font_system = FontSystem::new_with_locale_and_db(detect_locale(), db);
                while let Ok(request) = rx.recv() {
                    match request {
                        Request::Shape(req, reply) => {
                            // A dropped receiver just means the result is discarded.
                            let _ = reply.send(shape(&mut font_system, &primary_family, &req));
                        }
                        Request::FontChainData(reply) => {
                            let _ = reply.send(chain_data.clone());
                        }
                        Request::SetChain(chain, reply) => {
                            // Rebuilt rather than respawned: a new worker would drop the mapped
                            // faces the caller's cache clear already accounts for.
                            let borrowed: Vec<&str> = chain.iter().map(String::as_str).collect();
                            let ResolvedFonts { db: mut new_db, primary_family: new_primary } =
                                fonts::resolve_chain(&borrowed);
                            chain_data = font_chain_data(&mut new_db);
                            font_system = FontSystem::new_with_locale_and_db(detect_locale(), new_db);
                            primary_family = new_primary;
                            let _ = reply.send(());
                        }
                        #[cfg(test)]
                        Request::ResolvedPrimaryFamily(reply) => {
                            let _ = reply.send(primary_family.clone());
                        }
                    }
                }
            })
            .expect("failed to spawn oblisk-text-shaping thread");
        Self { requests: tx, cache: Arc::new(Mutex::new(HashMap::new())) }
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
        // The text moves into the key rather than being cloned into it, so a hit allocates
        // nothing and only a miss pays for the copy the worker needs.
        let key = ShapeKey {
            text: request.text,
            font_size: request.font_size.to_bits(),
            line_height: request.line_height.to_bits(),
            max_width: request.max_width.map(f32::to_bits),
        };
        // A poisoned lock is recovered rather than propagated: this map is a pure memo, so no
        // invariant can break, and an unrelated thread's death shouldn't kill text measurement.
        if let Some(hit) = self.cache.lock().unwrap_or_else(PoisonError::into_inner).get(&key) {
            return *hit;
        }

        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests
            .send(Request::Shape(
                ShapeRequest {
                    text: key.text.clone(),
                    font_size: f32::from_bits(key.font_size),
                    line_height: f32::from_bits(key.line_height),
                    max_width: key.max_width.map(f32::from_bits),
                },
                reply_tx,
            ))
            .expect("oblisk-text-shaping worker thread died");
        let result = reply_rx.recv().expect("oblisk-text-shaping worker thread died before replying");

        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        // Cleared wholesale rather than evicted one at a time: an LRU needs a recency order
        // maintained on every hit, work on the path this exists to make cheap.
        //
        // ponytail: a working set genuinely larger than the cap would re-shape everything every
        // pass. The largest list measured is 500 rows; the fix if that changes is an LRU.
        if cache.len() >= SHAPE_CACHE_CAPACITY {
            cache.clear();
        }
        cache.insert(key, result);
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
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests
            .send(Request::SetChain(chain.to_vec(), reply_tx))
            .expect("oblisk-text-shaping worker thread died");
        let _ = reply_rx.recv();
    }

    /// How many measurements are memoized. Test-only: exposing this in production would invite a
    /// caller to reason about cache state instead of treating [`shape`](Self::shape) as pure.
    #[cfg(test)]
    fn cached_len(&self) -> usize {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    /// Returns the loaded font chain's shared bytes, in chain order. Femtovg has no system font
    /// discovery of its own; it loads these via `add_shared_font_with_index` so paint rasterizes
    /// with the exact chain cosmic-text shaped against (`text::atlas::TextPainter::new`). Cloning
    /// `FontData` clones an `Arc`, so repeated calls are cheap and copy no font file.
    pub fn font_chain_data(&self) -> Vec<FontData> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests.send(Request::FontChainData(reply_tx)).expect("oblisk-text-shaping worker thread died");
        reply_rx.recv().expect("oblisk-text-shaping worker thread died before replying")
    }

    /// Returns the font chain's resolved primary family: the same name `shape()` asks
    /// cosmic-text for via `Family::Name`. Test-only. See `Request::ResolvedPrimaryFamily`.
    #[cfg(test)]
    pub fn resolved_primary_family(&self) -> String {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests.send(Request::ResolvedPrimaryFamily(reply_tx)).expect("oblisk-text-shaping worker thread died");
        reply_rx.recv().expect("oblisk-text-shaping worker thread died before replying")
    }
}

fn shape(font_system: &mut FontSystem, primary_family: &str, request: &ShapeRequest) -> ShapeResult {
    let metrics = Metrics::new(request.font_size, request.line_height);
    let mut buffer = Buffer::new(font_system, metrics);
    buffer.set_size(request.max_width, None);
    let attrs = Attrs::new().family(Family::Name(primary_family));
    buffer.set_text(&request.text, &attrs, Shaping::Advanced, None);
    buffer.shape_until_scroll(font_system, false);

    let mut width = 0.0f32;
    let mut line_count = 0u32;
    for run in buffer.layout_runs() {
        width = width.max(run.line_w);
        line_count += 1;
    }

    ShapeResult { width, height: line_count as f32 * metrics.line_height }
}

/// One entry per unique source *file*, not one per face, in the database's own load/chain order:
/// `db.faces()` yields one `FaceInfo` per face, a `.ttc` can hold many (Inter's own `Inter.ttc`
/// here has 36), and a shared mapping covers the *whole file*, so without deduping by path Inter
/// would appear 36 times. Maps rather than reads, the entire memory story here: `Noto Color Emoji`
/// is an 11MB CBDT bitmap font, and the `data.to_vec()` this replaces held it three times over
/// (worker `Vec<Vec<u8>>`, femtovg's `add_font_mem` copy, cosmic-text's own mapping); dropping it
/// on an idle eleven-surface session cut the Renderer's private-dirty memory from 49.7MB to
/// 22.7MB. `make_shared_face_data` rewrites every face sharing the path to `Source::SharedFile`,
/// so this is also cosmic-text's map.
///
/// SAFETY: `make_shared_face_data` is `unsafe` because a font file rewritten on disk changes
/// under the mapping, which can fault or produce nonsense glyphs. That is the same bargain
/// cosmic-text already makes internally for every font it renders, and the alternative is paying
/// a private copy per font per process to defend against someone editing a system font in place.
///
/// A face whose mapping cannot be established is skipped rather than fatal, matching
/// `resolve_chain`'s own treatment of an entry it can't honor: losing the emoji font is a missing
/// glyph, not a dead shell, and `fonts[0]` is still the primary since chain order survives.
/// ponytail: femtovg gets face index 0 of every file, leaving a `.ttc`'s other faces unreachable,
/// costing nothing since nothing selects a weight or style today; `add_shared_font_with_index`
/// already takes the index a fix would need.
fn font_chain_data(db: &mut fontdb::Database) -> Vec<FontData> {
    // Collected first: `make_shared_face_data` needs `&mut db`, so nothing may be borrowing it.
    let faces: Vec<(fontdb::ID, Option<std::path::PathBuf>)> = db
        .faces()
        .map(|face| {
            let path = match &face.source {
                fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => Some(path.clone()),
                fontdb::Source::Binary(_) => None,
            };
            (face.id, path)
        })
        .collect();

    let mut seen_paths = std::collections::HashSet::new();
    let mut data = Vec::new();
    for (id, path) in faces {
        if path.is_some_and(|path| !seen_paths.insert(path)) {
            continue;
        }
        // SAFETY: mapping a font file the process does not own, as the doc comment above spells
        // out. A rewrite in place changes the bytes under the mapping. Same bargain cosmic-text
        // already makes for every font it renders.
        match unsafe { db.make_shared_face_data(id) } {
            Some((bytes, _face_index)) => data.push(FontData(bytes)),
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
        ShapeRequest { text: text.into(), font_size, line_height: font_size * 1.2, max_width: None }
    }

    /// The chain a config declares has to reach the worker, or `fonts { ... }` is a no-op that
    /// looks like it worked. Uses the two families the shipped dev config names, and skips rather
    /// than fails on a machine that has neither installed.
    #[test]
    fn a_declared_chain_replaces_the_one_the_worker_started_with() {
        let handle = ShapingHandle::spawn();
        let before = handle.resolved_primary_family();
        let wanted = "CaskaydiaCove Nerd Font Propo".to_string();
        handle.set_chain(std::slice::from_ref(&wanted));
        let after = handle.resolved_primary_family();
        if after == before {
            eprintln!("skip: {wanted} is not installed, so the chain could not change");
            return;
        }
        assert_eq!(after, wanted, "the worker measures against what the config asked for");
    }

    /// The half the primary-family test does not reach, and the one that matters most here.
    /// `font_chain_data` is what `TextPainter::new` loads into femtovg, so a `set_chain` that moved
    /// the measuring side and not this one would reproduce the exact defect this module's doc
    /// records: text measured against one font and painted with another.
    #[test]
    fn a_declared_chain_reaches_the_faces_femtovg_paints_with_too() {
        let handle = ShapingHandle::spawn();
        let before: Vec<usize> = handle.font_chain_data().iter().map(|data| data.as_ref().len()).collect();
        handle.set_chain(&["CaskaydiaCove Nerd Font Propo".to_string()]);
        let after: Vec<usize> = handle.font_chain_data().iter().map(|data| data.as_ref().len()).collect();
        if handle.resolved_primary_family() != "CaskaydiaCove Nerd Font Propo" {
            eprintln!("skip: the family is not installed, so nothing could change");
            return;
        }
        assert_eq!(after.len(), 1, "a one-family chain loads one file");
        assert_ne!(after, before, "the bytes femtovg would load must be the new font's, not the old chain's");
        assert!(after[0] > 0, "and they are real bytes, not an empty mapping");
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
        handle.shape(req("Oblisk", 14.0));
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
            ("text", ShapeRequest { text: "abd".into(), font_size: 13.0, line_height: 15.6, max_width: None }),
            ("font_size", ShapeRequest { text: "abc".into(), font_size: 26.0, line_height: 15.6, max_width: None }),
            ("line_height", ShapeRequest { text: "abc".into(), font_size: 13.0, line_height: 40.0, max_width: None }),
            (
                "max_width",
                ShapeRequest { text: "abc".into(), font_size: 13.0, line_height: 15.6, max_width: Some(10.0) },
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
        let first = warm.shape(req("Oblisk", 14.0));
        let cached = warm.shape(req("Oblisk", 14.0));
        let cold = ShapingHandle::spawn().shape(req("Oblisk", 14.0));
        assert_eq!(cached, first);
        assert_eq!(cached, cold);
    }

    #[test]
    fn shapes_nonempty_text_to_a_nonzero_box() {
        let handle = ShapingHandle::spawn();
        let result =
            handle.shape(ShapeRequest { text: "Oblisk".into(), font_size: 14.0, line_height: 18.0, max_width: None });
        assert!(result.width > 0.0, "expected nonzero width, got {}", result.width);
        assert_eq!(result.height, 18.0);
    }

    #[test]
    fn empty_text_measures_to_zero_width() {
        let handle = ShapingHandle::spawn();
        let result =
            handle.shape(ShapeRequest { text: String::new(), font_size: 14.0, line_height: 18.0, max_width: None });
        assert_eq!(result.width, 0.0);
    }

    #[test]
    fn longer_text_measures_wider_than_shorter_text() {
        let handle = ShapingHandle::spawn();
        let short =
            handle.shape(ShapeRequest { text: "O".into(), font_size: 14.0, line_height: 18.0, max_width: None });
        let long = handle.shape(ShapeRequest {
            text: "Oblisk Shell".into(),
            font_size: 14.0,
            line_height: 18.0,
            max_width: None,
        });
        assert!(long.width > short.width);
    }

    #[test]
    fn font_chain_data_is_all_loadable_fonts() {
        let handle = ShapingHandle::spawn();
        let chain = handle.font_chain_data();
        assert!(!chain.is_empty(), "the default chain must resolve to at least one loaded face");
        for data in &chain {
            ttf_parser::Face::parse(data.as_ref(), 0).expect("every chain entry's bytes should parse as a font face");
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
                a.as_ref().as_ptr(),
                b.as_ref().as_ptr(),
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
            db.load_font_data(data.as_ref().to_vec());
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

        let request =
            ShapeRequest { text: "Oblisk Shell Renderer".into(), font_size: 24.0, line_height: 28.8, max_width: None };
        let proportional = shape(&mut font_system, "Noto Sans", &request);
        let monospace = shape(&mut font_system, "Noto Sans Mono", &request);

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

    #[test]
    fn a_max_width_narrower_than_the_unconstrained_text_wraps_to_more_lines() {
        let handle = ShapingHandle::spawn();
        let unconstrained = handle.shape(ShapeRequest {
            text: "Oblisk Shell Renderer".into(),
            font_size: 14.0,
            line_height: 18.0,
            max_width: None,
        });
        let wrapped = handle.shape(ShapeRequest {
            text: "Oblisk Shell Renderer".into(),
            font_size: 14.0,
            line_height: 18.0,
            max_width: Some(unconstrained.width / 2.0),
        });
        assert!(wrapped.height > unconstrained.height, "wrapping onto more lines must grow the measured height");
        assert!(wrapped.width <= unconstrained.width, "a wrapped line can't be wider than the unconstrained text");
    }
}
