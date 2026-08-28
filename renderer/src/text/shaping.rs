//! Off-thread wrapper around `cosmic-text`'s font shaping (build-steps.md Phase 4,
//! point 1), so measuring a dynamic string (a media title, a signal-driven label)
//! never blocks the render thread.
//!
//! The worker builds its `FontSystem` from `text::fonts::resolve_chain`'s declared chain
//! rather than `FontSystem::new()`, which walks the whole system font database (up to ~1s
//! per cosmic-text's own docs -- docs/adr/0043 decision 2, docs/build-steps.md Phase 19
//! item 10). That cost, and every shaping call after it, happens on one dedicated worker
//! thread with its own `FontSystem`, never touched from outside it. A plain `std::thread`
//! plus an `mpsc` channel is deliberately used instead of pulling in tokio's multi-thread
//! runtime feature: this is one dedicated CPU-bound worker, not a general async-task
//! need, and renderer's Cargo.toml only carries tokio's `rt`/`net`/`macros` features
//! today.
//!
//! `shape()` asks for `Family::Name(primary_family)`, the resolved chain's own first hit,
//! instead of the bare `Attrs::new()`/`Family::SansSerif` this used to measure with. That
//! match matters beyond cosmic-text's own layout: `text::atlas::TextPainter` loads the
//! exact same chain, in the exact same order, into femtovg -- so measurement and paint
//! resolve one font, not two independently-discovered ones that can (and, measured on the
//! dev machine before this fix, did) disagree. The old code measured under
//! `Family::SansSerif` while a separate `fontdb` query for paint missed that alias
//! entirely and fell back to `db.faces().next()`, the first face in scan order --
//! Adwaita Mono was drawn under text that had been measured in a proportional face,
//! leaving a `text` node's box roughly 30% narrower than the glyphs painted into it.

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
    // Test-only: `resolved_primary_family` below has no production caller (the real,
    // process-startup diagnostic for "what did the font chain resolve to" is
    // `fonts::resolve_chain`'s own `eprintln!`, which runs whether or not anything reads this
    // channel) -- `#[cfg(test)]` rather than `#[allow(dead_code)]` since nothing outside a test
    // binary sends this request at all.
    #[cfg(test)]
    ResolvedPrimaryFamily(mpsc::Sender<String>),
}

/// A handle to a dedicated shaping worker thread and its warm font cache.
///
/// `Clone` clones the request `Sender` alone, so every clone still addresses the one worker
/// thread and the one `FontSystem` behind it -- that is what lets `wayland::App` and the
/// `RendererClient` it owns share a single warm font cache instead of each paying
/// `FontSystem::new()`'s ~1s startup (docs/adr/0023 item 8, closed by docs/adr/0039 decision 3).
#[derive(Clone)]
pub struct ShapingHandle {
    requests: mpsc::Sender<Request>,
}

impl ShapingHandle {
    /// Spawns the worker thread and its own `FontSystem`. The `FontSystem` is created
    /// inside the spawned closure, not moved into it, so this doesn't require
    /// `FontSystem: Send` -- only the request/reply channels cross the thread boundary.
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel::<Request>();
        thread::Builder::new()
            .name("oblisk-text-shaping".into())
            .spawn(move || {
                let ResolvedFonts { db, primary_family } = fonts::resolve_chain(fonts::DEFAULT_CHAIN);
                // Read once: `db.faces()` iterates in load order, i.e. chain order (fonts::
                // resolve_chain never reorders what it loads), and neither changes again for
                // this FontSystem's lifetime, so there's no reason to recompute this per request.
                let chain_bytes = font_chain_bytes(&db);
                let mut font_system = FontSystem::new_with_locale_and_db(detect_locale(), db);
                while let Ok(request) = rx.recv() {
                    match request {
                        Request::Shape(req, reply) => {
                            // A dropped receiver (caller stopped waiting) just means
                            // the result is discarded, not a worker-thread problem.
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
    /// Panics if the worker thread has died (a bug, not a recoverable runtime state --
    /// matches this codebase's existing convention of treating a broken invariant as
    /// fatal rather than silently continuing, see `wayland::mod`'s EGL bind failures).
    pub fn shape(&self, request: ShapeRequest) -> ShapeResult {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests
            .send(Request::Shape(request, reply_tx))
            .expect("oblisk-text-shaping worker thread died");
        reply_rx
            .recv()
            .expect("oblisk-text-shaping worker thread died before replying")
    }

    /// Returns the loaded font chain's raw bytes, in chain order -- what FemtoVG (which has no
    /// system font discovery of its own) loads via `add_font_mem` so it rasterizes with the
    /// exact same chain, in the exact same fallback order, that cosmic-text shaped against
    /// (`text::atlas::TextPainter::new`).
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

/// One entry per unique source *file*, in the database's own iteration order (load order, i.e.
/// the chain order `fonts::resolve_chain` loaded them in) -- not one per face.
///
/// `db.faces()` yields one `FaceInfo` per face, and a `.ttc` collection can hold many (Inter's
/// own `Inter.ttc` on this machine has 36). `with_face_data` hands back the *whole file*'s bytes
/// regardless of which face's `id` asked for them, so without deduping by source path, a chain
/// entry that resolves to a collection would return the same multi-megabyte buffer once per
/// face inside it -- 36 identical copies for Inter, every one of them read by
/// `TextPainter::new`'s `add_font_mem`.
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
        // `fonts::resolve_chain` only ever loads through `Database::load_font_file`, which
        // always produces `Source::File` -- the other two `Source` variants exist for the
        // `Binary`/mmap-sharing APIs this module never calls into.
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

/// The process locale, read the way glibc's own env-var chain does: `LC_ALL`, then
/// `LC_CTYPE`, then `LANG`, defaulting to `"en-US"` when none are set (or all name no real
/// locale, `"C"`/`"POSIX"`). `FontSystem::new_with_locale_and_db` takes the locale as a plain
/// argument instead of reading it itself the way `FontSystem::new()` does internally (via the
/// `sys_locale` crate, not a dependency of this workspace) -- this is what replaces that lookup
/// now that the chain construction bypasses `new()`.
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
            // A real, parseable font file, not just nonempty bytes.
            ttf_parser::Face::parse(bytes, 0).expect("every chain entry's bytes should parse as a font face");
        }
    }

    #[test]
    fn the_resolved_primary_family_is_findable_among_the_loaded_chain_fonts() {
        // The invariant this whole module exists to establish, asserted the way it actually
        // matters: `shape()` measures under `Family::Name(resolved_primary_family())`, so
        // cosmic-text's own database query has to find a face for that exact name, not merely
        // have some loaded face whose own name record happens to read the same string.
        // Comparing name records (what this test used to do) passes for a reason that isn't the
        // one that matters -- a font's `fontdb`-visible family and its raw TTF `FAMILY` name
        // record are two different sources of truth and aren't required to agree, so that
        // version could pass by luck. This instead reconstructs a database from
        // `font_chain_bytes()`, the exact bytes `text::atlas::TextPainter` loads into femtovg,
        // and queries it the same way `shaping::shape` queries cosmic-text's -- if
        // `resolved_primary_family()` weren't actually findable there, `shape()` would silently
        // get whatever cosmic-text falls back to for an unresolvable `Family::Name`, while paint
        // would go on drawing the chain it was actually given, and the two would disagree again.
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
        // What `femtovg_and_cosmic_text_measure_the_same_string_to_the_same_width`
        // (`layout::paint`'s own tests) cannot check today: today's default chain resolves to
        // exactly one Latin-covering face (`Noto Sans CJK JP` misses on every machine this has
        // been run on so far), so every path -- a named family, `Family::SansSerif`, whatever --
        // converges on that same face regardless of what `shape()` actually asks for. That test
        // would stay green even if `shape()` stopped honouring `primary_family` entirely.
        //
        // This test closes that gap directly, at the one seam that can: `shape()` is a private
        // function taking `primary_family: &str` as a plain argument, so it can be called twice
        // against the *same* `FontSystem` -- built once, over a database holding two Latin faces
        // with very different metrics -- varying only that argument. Same database both calls is
        // the whole point: varying the chain instead (asking for "Noto Sans" once and "Noto Sans
        // Mono" a second time through two separate `resolve_chain` calls) would also vary the
        // database's own contents and load order, and a width difference then couldn't be
        // pinned on the family argument specifically -- it could just as well be a different
        // fallback chain doing the work. Holding the database fixed and varying only the
        // argument `shape()` itself takes is what isolates the one thing this test exists to
        // check.
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

        // A proportional face and a monospace face measuring the same Latin string to
        // (near enough) the same width would mean the family argument did nothing -- 10% is
        // well clear of font-to-font rounding noise and well under the gap between these two
        // families' actual advance widths for this string.
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
