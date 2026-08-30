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

use std::env;
use std::sync::mpsc;
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

enum Request {
    Shape(ShapeRequest, mpsc::Sender<ShapeResult>),
    FontChainBytes(mpsc::Sender<Vec<Vec<u8>>>),
    // Test-only, `#[cfg(test)]` rather than `#[allow(dead_code)]`: nothing outside a test binary
    // sends this. The production diagnostic for the resolved chain is `fonts::resolve_chain`'s
    // own `eprintln!`.
    #[cfg(test)]
    ResolvedPrimaryFamily(mpsc::Sender<String>),
}

/// A handle to a dedicated shaping worker thread and its warm font cache.
///
/// `Clone` clones the request `Sender` alone, so every clone still addresses the one worker
/// thread and `FontSystem` -- what lets `wayland::App` and the `RendererClient` it owns share a
/// warm font cache instead of each paying `FontSystem::new()`'s ~1s startup (docs/adr/0023 item
/// 8, closed by docs/adr/0039 decision 3).
#[derive(Clone)]
pub struct ShapingHandle {
    requests: mpsc::Sender<Request>,
}

impl ShapingHandle {
    /// Spawns the worker thread and its own `FontSystem`, created inside the spawned closure
    /// rather than moved into it, so this doesn't require `FontSystem: Send`.
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel::<Request>();
        thread::Builder::new()
            .name("oblisk-text-shaping".into())
            .spawn(move || {
                let ResolvedFonts { db, primary_family } = fonts::resolve_chain(fonts::DEFAULT_CHAIN);
                // Read once: neither the chain nor its load order changes again for this
                // FontSystem's lifetime.
                let chain_bytes = font_chain_bytes(&db);
                let mut font_system = FontSystem::new_with_locale_and_db(detect_locale(), db);
                while let Ok(request) = rx.recv() {
                    match request {
                        Request::Shape(req, reply) => {
                            // A dropped receiver just means the result is discarded.
                            let _ = reply.send(shape(&mut font_system, &primary_family, &req));
                        }
                        Request::FontChainBytes(reply) => {
                            let _ = reply.send(chain_bytes.clone());
                        }
                        #[cfg(test)]
                        Request::ResolvedPrimaryFamily(reply) => {
                            let _ = reply.send(primary_family.clone());
                        }
                    }
                }
            })
            .expect("failed to spawn oblisk-text-shaping thread");
        Self { requests: tx }
    }

    /// Shapes `request` on the worker thread and blocks until the result comes back.
    /// Panics if the worker thread has died: a bug, not a recoverable runtime state.
    pub fn shape(&self, request: ShapeRequest) -> ShapeResult {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests
            .send(Request::Shape(request, reply_tx))
            .expect("oblisk-text-shaping worker thread died");
        reply_rx
            .recv()
            .expect("oblisk-text-shaping worker thread died before replying")
    }

    /// Returns the loaded font chain's raw bytes, in chain order -- what femtovg (no system font
    /// discovery of its own) loads via `add_font_mem` so paint rasterizes with the same chain,
    /// same order, that cosmic-text shaped against (`text::atlas::TextPainter::new`).
    pub fn font_chain_bytes(&self) -> Vec<Vec<u8>> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests
            .send(Request::FontChainBytes(reply_tx))
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
/// own `Inter.ttc` on this machine has 36). `with_face_data` hands back the *whole file*'s bytes
/// regardless of which face's `id` asked for them, so without deduping by source path, a
/// collection entry would return the same multi-megabyte buffer once per face inside it -- 36
/// identical copies for Inter.
///
/// ponytail: `femtovg::add_font_mem` takes no face index and always loads face 0 of whatever
/// it's given (confirmed by reading femtovg's own `add_font_mem_with_index(data, 0)`), so a
/// `.ttc`'s other 35 faces, and any weight or style declared only inside them, are unreachable
/// through this path regardless of deduping. That's already where this codebase sits: nothing
/// selects a weight or style anywhere today, so it costs nothing now. Reaching a face other
/// than 0 needs `add_shared_font_with_index`, which is a real API femtovg already has -- the
/// fix if a declared weight/style ever needs to come from inside a collection file.
fn font_chain_bytes(db: &fontdb::Database) -> Vec<Vec<u8>> {
    let mut seen_paths = std::collections::HashSet::new();
    let mut bytes = Vec::new();
    for face in db.faces() {
        // `fonts::resolve_chain` only loads through `Database::load_font_file`, which always
        // produces `Source::File`.
        let fontdb::Source::File(path) = &face.source else {
            bytes.push(db.with_face_data(face.id, |data, _face_index| data.to_vec()).expect("a face just enumerated by db.faces() must have its own source data"));
            continue;
        };
        if seen_paths.insert(path.clone()) {
            bytes.push(db.with_face_data(face.id, |data, _face_index| data.to_vec()).expect("a face just enumerated by db.faces() must have its own source data"));
        }
    }
    bytes
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
    fn font_chain_bytes_are_all_loadable_fonts() {
        let handle = ShapingHandle::spawn();
        let chain = handle.font_chain_bytes();
        assert!(!chain.is_empty(), "the default chain must resolve to at least one loaded face");
        for bytes in &chain {
            ttf_parser::Face::parse(bytes, 0).expect("every chain entry's bytes should parse as a font face");
        }
    }

    #[test]
    fn the_resolved_primary_family_is_findable_among_the_loaded_chain_fonts() {
        // Reconstructs a database from `font_chain_bytes()` -- the exact bytes
        // `text::atlas::TextPainter` loads into femtovg -- and queries it the way `shape()`
        // queries cosmic-text's. Comparing name records instead would pass for the wrong reason:
        // a font's `fontdb`-visible family and its raw TTF `FAMILY` record are different sources
        // of truth and aren't required to agree.
        let handle = ShapingHandle::spawn();
        let primary_family = handle.resolved_primary_family();

        let mut db = fontdb::Database::new();
        for bytes in handle.font_chain_bytes() {
            db.load_font_data(bytes);
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
