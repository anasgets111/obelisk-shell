//! Glyph rasterization and GPU texture atlas management, via FemtoVG (build-steps.md
//! Phase 4, point 2).
//!
//! FemtoVG owns its glyph atlas entirely internally (see docs/adr/0012): loaded fonts'
//! rasterized glyphs are packed into private atlas pages that start at a fixed size and
//! grow by adding further pages, not by expanding one large texture -- there is no
//! public API to configure a single fixed-size page the way build-steps.md's "2048x2048"
//! literally describes, and no public API to feed it glyphs shaped by anything other
//! than FemtoVG's own internal shaper (its atlas-filling path is `pub(crate)`). This
//! module drives FemtoVG's atlas through its public font/text API (`add_font_mem`,
//! `fill_text`) rather than reimplementing packing on top of it -- "does an installed
//! dependency solve it" (AGENTS.md) applies directly here.

use std::error::Error;
use std::ffi::c_void;

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, Color, FontId, Paint};

use crate::layout::node::Rgba;

use super::snap::{snap_to_physical, LogicalRect};

/// A FemtoVG canvas bound to the calling thread's current EGL/GL context, with the declared
/// font chain loaded and ready to draw with.
pub struct TextPainter {
    canvas: Canvas<OpenGl>,
    fonts: Vec<FontId>,
}

impl TextPainter {
    /// `load_fn` must resolve GL function pointers against a context that's already
    /// current on this thread -- FemtoVG doesn't make any context current itself, it
    /// only calls GL through the pointers it's given.
    ///
    /// `font_chain_bytes` is `ShapingHandle::font_chain_bytes()`'s own output, in the same
    /// chain order cosmic-text shaped against -- loading it here rather than doing FemtoVG's
    /// own font discovery is the other half of docs/adr/0043 decision 2's fix: measurement and
    /// paint resolve the one declared chain, not two independently-discovered fonts that can
    /// disagree (see `text::shaping`'s module doc comment for the bug this replaces). Errors if
    /// the slice is empty -- `draw_line` cannot fall back to a font it was never given.
    pub fn new(
        load_fn: impl FnMut(&str) -> *const c_void,
        width: u32,
        height: u32,
        font_chain_bytes: &[Vec<u8>],
    ) -> Result<Self, Box<dyn Error>> {
        if font_chain_bytes.is_empty() {
            return Err("TextPainter::new requires at least one loaded font".into());
        }
        let renderer = unsafe { OpenGl::new_from_function(load_fn)? };
        let mut canvas = Canvas::new(renderer)?;
        canvas.set_size(width, height, 1.0);
        let fonts = font_chain_bytes.iter().map(|bytes| canvas.add_font_mem(bytes)).collect::<Result<Vec<_>, _>>()?;
        Ok(Self { canvas, fonts })
    }

    /// Updates the canvas's viewport to match the surface's current size. Cheap and
    /// idempotent -- callers should call this every frame rather than caching a size
    /// from construction time, since the surface can resize after the painter is built.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.canvas.set_size(width, height, 1.0);
    }

    /// The same canvas `draw_line` fills text onto, exposed so `layout::paint`'s tree walk can
    /// draw a node's background/border on it too (build-steps.md Phase 19 item 6, docs/adr/0023)
    /// -- one canvas per surface, shared by every paint operation on it, not one per property
    /// kind.
    pub fn canvas_mut(&mut self) -> &mut Canvas<OpenGl> {
        &mut self.canvas
    }

    /// The loaded chain's `FontId`s, in chain order -- the test seam `layout::paint`'s
    /// divergence test uses to build its own `Paint` and call `canvas_mut().measure_text` with
    /// the exact fonts `draw_line` itself paints with, so that test can compare femtovg's
    /// measurement against `ShapingHandle::shape`'s. `draw_line` itself reaches `self.fonts`
    /// directly and has no need of this; it exists for that one test caller.
    #[cfg(test)]
    pub fn fonts(&self) -> &[FontId] {
        &self.fonts
    }

    /// Draws `text` with its snapped top-left corner at `rect`'s origin (build-steps.md
    /// Phase 4, point 3) in `color`. Does not flush or swap buffers -- `layout::paint`'s tree
    /// walk draws a whole surface's worth of nodes onto this same canvas and flushes once at
    /// the end (build-steps.md Phase 19 item 6), not once per line the way this used to. That
    /// walk is now the only caller: `wayland::mod`'s `draw_main_bar_proof_text`, which used to
    /// flush for itself right after calling this, was deleted in Phase 20 item 4 along with the
    /// rest of the fixed-role scaffolding.
    pub fn draw_line(&mut self, text: &str, rect: LogicalRect, font_size: f32, scale: f32, color: Rgba) {
        let physical = snap_to_physical(rect, scale);
        // `Rgba`'s four `f32` fields exist precisely so `Color::rgbaf` takes them with no
        // conversion (see that struct's own doc comment in `layout::node`).
        let mut paint = Paint::color(Color::rgbaf(color.r, color.g, color.b, color.a));
        // The whole chain, in chain order: FemtoVG's `set_font` does its own per-glyph
        // fallback across the slice it's given, so this is what carries the declared fallback
        // chain (a CJK codepoint a UI font lacks, an emoji neither covers) into paint, the same
        // way cosmic-text's own shaping already falls back across `db`'s loaded faces.
        paint.set_font(&self.fonts);
        paint.set_font_size(font_size);
        // FemtoVG's fill_text baseline follows the HTML5 Canvas API it's modeled on
        // (see femtovg's crate-level docs): y is the text baseline, not the box top,
        // so it belongs at the snapped top edge plus the font's ascender -- not at
        // y1 (the snapped *bottom* edge), which would push the glyph body below the
        // box entirely and cut descenders off outside it.
        let ascender = self.canvas.measure_font(&paint).map(|m| m.ascender()).unwrap_or(font_size);
        let baseline_y = physical.y0 as f32 + ascender;
        let _ = self.canvas.fill_text(physical.x0 as f32, baseline_y, text, &paint);
    }
}
