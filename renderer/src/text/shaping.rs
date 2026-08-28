//! Off-thread wrapper around `cosmic-text`'s font shaping (build-steps.md Phase 4,
//! point 1), so measuring a dynamic string (a media title, a signal-driven label)
//! never blocks the render thread.
//!
//! `FontSystem::new()` walks the system font database and can take up to ~1s per
//! cosmic-text's own docs -- that cost, and every shaping call after it, happens on
//! one dedicated worker thread with its own `FontSystem`, never touched from outside
//! it. A plain `std::thread` + `mpsc` channel is deliberately used instead of pulling
//! in tokio's multi-thread runtime feature: this is one dedicated CPU-bound worker,
//! not a general async-task need, and renderer's Cargo.toml only carries tokio's
//! `rt`/`net`/`macros` features today.

use std::sync::mpsc;
use std::thread;

use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping};
use fontdb::{Family, Query};

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
    DefaultFontBytes(mpsc::Sender<Vec<u8>>),
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
                let mut font_system = FontSystem::new();
                while let Ok(request) = rx.recv() {
                    match request {
                        Request::Shape(req, reply) => {
                            // A dropped receiver (caller stopped waiting) just means
                            // the result is discarded, not a worker-thread problem.
                            let _ = reply.send(shape(&mut font_system, &req));
                        }
                        Request::DefaultFontBytes(reply) => {
                            let _ = reply.send(default_font_bytes(&font_system));
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

    /// Returns the raw bytes of the system's default sans-serif font, as resolved by
    /// the same `FontSystem`/`fontdb` font discovery the shaping worker already uses --
    /// so whatever rasterizes glyphs (FemtoVG, which has no system font discovery of
    /// its own) draws with the exact font cosmic-text shaped against.
    pub fn default_font_bytes(&self) -> Vec<u8> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests
            .send(Request::DefaultFontBytes(reply_tx))
            .expect("oblisk-text-shaping worker thread died");
        reply_rx
            .recv()
            .expect("oblisk-text-shaping worker thread died before replying")
    }
}

fn shape(font_system: &mut FontSystem, request: &ShapeRequest) -> ShapeResult {
    let metrics = Metrics::new(request.font_size, request.line_height);
    let mut buffer = Buffer::new(font_system, metrics);
    buffer.set_size(request.max_width, None);
    buffer.set_text(&request.text, &Attrs::new(), Shaping::Advanced, None);
    buffer.shape_until_scroll(font_system, false);

    let mut width = 0.0f32;
    let mut line_count = 0u32;
    for run in buffer.layout_runs() {
        width = width.max(run.line_w);
        line_count += 1;
    }

    ShapeResult { width, height: line_count as f32 * metrics.line_height }
}

fn default_font_bytes(font_system: &FontSystem) -> Vec<u8> {
    let db = font_system.db();
    // cosmic-text aliases the generic `sans-serif` family to a specific font name
    // ("Open Sans") rather than resolving it against whatever's actually installed
    // (see FontSystem::new_with_fonts) -- that alias fails to resolve on a system
    // that doesn't happen to have that exact font. ponytail: fall back to the first
    // loaded face in that case rather than picking a real fallback policy, which is
    // a later phase's job (font selection isn't part of this milestone).
    let query = Query { families: &[Family::SansSerif], ..Default::default() };
    let id = db
        .query(&query)
        .or_else(|| db.faces().next().map(|face| face.id))
        .expect("fontdb has no loaded faces at all");
    db.with_face_data(id, |data, _face_index| data.to_vec())
        .expect("matched font's source data is unavailable")
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
    fn default_font_bytes_are_a_loadable_font() {
        let handle = ShapingHandle::spawn();
        let bytes = handle.default_font_bytes();
        assert!(!bytes.is_empty());
        // A real, parseable font file, not just nonempty bytes.
        ttf_parser::Face::parse(&bytes, 0).expect("default font bytes should parse as a font face");
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
