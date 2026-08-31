//! Off-thread wrapper around `cosmic-text`'s font shaping (build-steps.md Phase 4, point 1), so
//! measuring a dynamic string never blocks the render thread.
//!
//! The worker builds its `FontSystem` from `text::fonts::resolve_chain`'s declared chain rather
//! than `FontSystem::new()`, which walks the whole system font database (up to ~1s per
//! cosmic-text's own docs -- docs/adr/0043 decision 2, Phase 19 item 10). A plain `std::thread`
//! plus `mpsc`, not tokio's multi-thread runtime: this is one dedicated CPU-bound worker, and
//! renderer's Cargo.toml only carries tokio's `rt`/`net`/`macros` features.
//!
//! `shape()` asks for `Family::Name(primary_family)`, the resolved chain's own first hit, so
//! `text::atlas::TextPainter` loads the exact same chain in the exact same order into femtovg.
//! A mismatch here is not theoretical: the old code measured under `Family::SansSerif` while a
//! separate `fontdb` query for paint missed that alias and fell back to the first face in scan
//! order, leaving a `text` node's box measured roughly 30% narrower than the glyphs painted
//! into it.

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
    /// Logical-pixel width to wrap at (`oblisk-layout-engine-geometry.md` § 3.2: "wraps text
    /// bounds when exceeding available width limits"). `None` measures the text unconstrained,
    /// on one line -- the pre-Phase-12 behavior every existing caller still gets.
    pub max_width: Option<f32>,
}

/// The measured result of shaping a request: its tight bounding box in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapeResult {
    pub width: f32,
    pub height: f32,
}

/// One font file's bytes, held once and shared by every reader.
///
/// The inner `Arc` is `fontdb`'s: `Database::make_shared_face_data` maps the file and rewrites
/// every face that came from it to point at the mapping, so cosmic-text (which owns the
/// `Database`) and femtovg (which is handed this) read the same pages rather than each holding a
/// copy. That is the whole point of the type. The bytes are `Shared_Clean` and page-cache backed,
/// so a second process using the same font pays nothing for it, and the kernel can evict them.
///
/// The newtype is what femtovg's `add_shared_font_with_index` needs: it takes
/// `T: AsRef<[u8]> + 'static` by value, and `Arc<dyn AsRef<[u8]>>` does not itself implement
/// `AsRef<[u8]>`.
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
    // Test-only, `#[cfg(test)]` rather than `#[allow(dead_code)]`: nothing outside a test binary
    // sends this. The production diagnostic for the resolved chain is `fonts::resolve_chain`'s
    // own `eprintln!`.
    #[cfg(test)]
    ResolvedPrimaryFamily(mpsc::Sender<String>),
}

/// How many measured strings [`ShapingHandle`] remembers before it drops the lot.
///
/// Sized against the working set, which is the text on screen: the shipped dev config resolves
/// about twenty text nodes, and the largest list this engine is meant to carry (a notification
/// history, a full launcher) is a few hundred. 4096 holds all of that with room left for churn.
///
/// Churn is the reason there is a bound at all. A clock shapes a string nobody will ask for again
/// every second, so an unbounded map grows by 86,400 dead entries a day. At this cap it clears
/// roughly hourly instead and costs one cold pass, a few milliseconds, once.
///
/// Full, the map holds on the order of 400KB: 4096 entries of a short `String` plus four integers
/// and two floats, with `HashMap`'s own overhead. That is under one percent of docs/adr/0043's
/// 50MB-per-monitor budget, which is what makes a cap this generous the cheap choice.
const SHAPE_CACHE_CAPACITY: usize = 4096;

/// What a measurement is keyed by: every field of a [`ShapeRequest`], so a request this cannot
/// tell apart from another genuinely shapes the same.
///
/// Keying on a subset would be a wrong-answer bug rather than a slow one, which is why
/// `line_height` is here even though every caller today derives it as `font_size * 1.2`.
///
/// Floats are held as bit patterns because `f32` is not `Hash` or `Eq`. That makes equality here
/// stricter than `==` in both directions, and both are the harmless direction: two `NaN` sizes with
/// the same bit pattern share an entry where `==` would call them different, and `0.0` and `-0.0`
/// get separate entries where `==` would call them equal. The first re-uses a measurement of the
/// same request; the second wastes one entry on a duplicate. Neither can return another request's
/// answer, which is the only outcome that would be a bug.
#[derive(PartialEq, Eq, Hash)]
struct ShapeKey {
    text: String,
    font_size: u32,
    line_height: u32,
    max_width: Option<u32>,
}

/// A handle to a dedicated shaping worker thread and its warm font cache.
///
/// `Clone` clones the request `Sender` alone, so every clone still addresses the one worker
/// thread and `FontSystem` -- what lets `wayland::App` and the `RendererClient` it owns share a
/// warm font cache instead of each paying `FontSystem::new()`'s ~1s startup (docs/adr/0023 item
/// 8, closed by docs/adr/0039 decision 3).
///
/// The measurement cache is `Arc`-shared for the same reason and hangs on this side of the
/// channel rather than inside the worker, which is the whole point of it: a worker-side cache
/// would still pay an `mpsc` round trip and a thread wake per text node, and measured against a
/// 500-row list those are a real fraction of the 17us each node costs, not a rounding error.
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
                let ResolvedFonts { mut db, primary_family } = fonts::resolve_chain(fonts::DEFAULT_CHAIN);
                // Mapped once, before the `Database` is handed to cosmic-text: neither the chain
                // nor its load order changes again for this `FontSystem`'s lifetime, and doing it
                // here is what leaves the shared mappings *in* the database for cosmic-text to
                // find rather than mapping the same files a second time.
                let chain_data = font_chain_data(&mut db);
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

    /// Measures `request`, from [`SHAPE_CACHE_CAPACITY`]'s memo when it has been asked before and
    /// from the worker thread otherwise. Blocks only on a miss.
    ///
    /// Nothing invalidates an entry, because nothing can change one. The font chain is resolved
    /// once inside [`ShapingHandle::spawn`] and never reloaded, so a given key shapes to the same
    /// box for the life of the process.
    ///
    /// Why this exists: `Scene::apply` re-measures every text node it resolves, and ADR-0044
    /// decision 2's dirty flag turned that from once per config edit into once per push. Measured
    /// on a 500-row list, this memo takes 6.64ms off a 10.61ms pass, and takes exactly the same
    /// 6.64ms off the 12.78ms variant that also draws an icon, which is what says the saving is
    /// measurement and not something else moving.
    ///
    /// Panics on a miss if the worker thread has died: a bug, not a recoverable runtime state. A
    /// hit does not reach the worker at all, so a key measured while it was alive keeps answering
    /// after it dies. That is correct rather than lucky (nothing can change what a key measures to)
    /// but it does mean the first symptom of a dead worker is the next new string, not the next
    /// call.
    pub fn shape(&self, request: ShapeRequest) -> ShapeResult {
        // The text moves into the key rather than being cloned into it, so a hit allocates
        // nothing and only a miss pays for the copy the worker needs.
        let key = ShapeKey {
            text: request.text,
            font_size: request.font_size.to_bits(),
            line_height: request.line_height.to_bits(),
            max_width: request.max_width.map(f32::to_bits),
        };
        // A poisoned lock is recovered rather than propagated: this map is a pure memo, so a panic
        // while holding it can leave no invariant broken, and refusing to measure text because an
        // unrelated thread died would take the shell down over a cache.
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
        let result = reply_rx
            .recv()
            .expect("oblisk-text-shaping worker thread died before replying");

        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        // Cleared wholesale rather than evicted one entry at a time. An LRU needs a recency order
        // maintained on every hit, which is work on the path this exists to make cheap, and the
        // thing that overflows this map is a clock producing strings nobody asks for twice -- so
        // the entries worth keeping are re-measured on the next pass anyway.
        //
        // ponytail: a working set genuinely larger than the cap would re-shape everything on every
        // pass, which is slower than no cache at all because it also pays the hashing. Nothing this
        // engine renders is near it (the largest list measured is 500 rows), and the fix when
        // something is would be an LRU rather than a bigger number.
        if cache.len() >= SHAPE_CACHE_CAPACITY {
            cache.clear();
        }
        cache.insert(key, result);
        result
    }

    /// How many measurements are memoized. Test-only: production has no reason to ask, and a
    /// getter that reports it would invite a caller to reason about cache state instead of
    /// treating [`shape`](Self::shape) as the pure function it is.
    #[cfg(test)]
    fn cached_len(&self) -> usize {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    /// Returns the loaded font chain's shared bytes, in chain order -- what femtovg (no system
    /// font discovery of its own) loads via `add_shared_font_with_index` so paint rasterizes with
    /// the exact chain, in the exact order, that cosmic-text shaped against
    /// (`text::atlas::TextPainter::new`).
    ///
    /// Cheap and repeatable: cloning `FontData` clones an `Arc`, so this no longer copies a font
    /// file per call the way the `Vec<Vec<u8>>` it replaces did.
    pub fn font_chain_data(&self) -> Vec<FontData> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests
            .send(Request::FontChainData(reply_tx))
            .expect("oblisk-text-shaping worker thread died");
        reply_rx
            .recv()
            .expect("oblisk-text-shaping worker thread died before replying")
    }

    /// Returns the font chain's resolved primary family: the same name `shape()` asks
    /// cosmic-text for via `Family::Name`. Test-only -- see `Request::ResolvedPrimaryFamily`.
    #[cfg(test)]
    pub fn resolved_primary_family(&self) -> String {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests
            .send(Request::ResolvedPrimaryFamily(reply_tx))
            .expect("oblisk-text-shaping worker thread died");
        reply_rx
            .recv()
            .expect("oblisk-text-shaping worker thread died before replying")
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

/// One entry per unique source *file*, in the database's own iteration (load/chain) order --
/// not one per face.
///
/// `db.faces()` yields one `FaceInfo` per face, and a `.ttc` collection can hold many (Inter's
/// own `Inter.ttc` on this machine has 36). A shared mapping covers the *whole file* regardless
/// of which face's `id` asked for it, so without deduping by source path a collection entry
/// would appear once per face inside it -- 36 identical entries for Inter.
///
/// Maps rather than reads, which is the entire memory story here. `Noto Color Emoji` is an 11MB
/// CBDT bitmap font, and the `data.to_vec()` this replaces held it three times over: once in the
/// worker's own `Vec<Vec<u8>>`, once again in femtovg's `add_font_mem` copy, and once more in
/// cosmic-text's independent mapping. Measured on an idle eleven-surface session, dropping that
/// one font from the chain cut the Renderer's private-dirty memory from 49.7MB to 22.7MB, which
/// is what those copies cost. `make_shared_face_data` rewrites every face sharing the path to
/// `Source::SharedFile`, so the mapping this returns is also the one cosmic-text goes on to use.
///
/// SAFETY: `make_shared_face_data` is `unsafe` because a font file rewritten on disk changes
/// under the mapping, which can fault or produce nonsense glyphs. That is the same bargain
/// cosmic-text already makes internally for every font it renders, and the alternative is paying
/// a private copy per font per process to defend against someone editing a system font in place.
///
/// A face whose mapping cannot be established is skipped rather than fatal: `resolve_chain`
/// already treats a chain entry it cannot honor as a skip, and losing the emoji font is a
/// missing glyph, not a dead shell. Chain order survives, so `fonts[0]` is still the primary.
///
/// ponytail: femtovg is handed face index 0 of every file, so a `.ttc`'s other faces, and any
/// weight or style declared only inside them, stay unreachable. Nothing selects a weight or
/// style anywhere today, so it costs nothing now; `add_shared_font_with_index` already takes the
/// index this would need.
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
        match unsafe { db.make_shared_face_data(id) } {
            Some((bytes, _face_index)) => data.push(FontData(bytes)),
            None => eprintln!("font chain: face {id:?} could not be mapped, skipped"),
        }
    }
    data
}

/// The process locale, read the way glibc's own env-var chain does: `LC_ALL`, then `LC_CTYPE`,
/// then `LANG`, defaulting to `"en-US"` when none are set (or all name no real locale,
/// `"C"`/`"POSIX"`). Replaces the lookup `FontSystem::new()` does internally via `sys_locale`,
/// now that construction bypasses `new()`.
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
            ("max_width", ShapeRequest { text: "abc".into(), font_size: 13.0, line_height: 15.6, max_width: Some(10.0) }),
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
        assert_eq!(clone.cached_len(), 1, "`wayland::App` and the client it owns must not measure the same string twice");
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
        let result = handle.shape(ShapeRequest {
            text: "Oblisk".into(),
            font_size: 14.0,
            line_height: 18.0,
            max_width: None,
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
        });
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
            assert_eq!(a.as_ref().as_ptr(), b.as_ref().as_ptr(), "each ask should share one mapping, not copy the file");
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
        assert!(db.query(&query).is_some(), "primary_family {primary_family:?} is not findable in the loaded font chain");
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

        let request = ShapeRequest { text: "Oblisk Shell Renderer".into(), font_size: 24.0, line_height: 28.8, max_width: None };
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
        assert!(
            wrapped.height > unconstrained.height,
            "wrapping onto more lines must grow the measured height"
        );
        assert!(
            wrapped.width <= unconstrained.width,
            "a wrapped line can't be wider than the unconstrained text"
        );
    }
}
