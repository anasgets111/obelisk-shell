//! Glyph rasterization and GPU texture atlas management, via FemtoVG (build-steps.md
//! Phase 4, point 2).
//!
//! FemtoVG owns its glyph atlas entirely internally (see docs/adr/0012): rasterized glyphs pack
//! into private atlas pages that start at a fixed size and grow by adding further pages, not by
//! expanding one large texture -- there is no public API to configure a single fixed-size page
//! the way build-steps.md's "2048x2048" literally describes, and no public API to feed it glyphs
//! shaped by anything other than FemtoVG's own internal shaper. This module drives FemtoVG's
//! atlas through its public font/text API (`add_font_mem`, `fill_text`) rather than
//! reimplementing packing on top of it.

use std::error::Error;
use std::ffi::c_void;

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, Color, FontId, Paint, TextContext};

use crate::layout::node::Rgba;
use crate::text::shaping::FontData;

use super::snap::{snap_to_physical, LogicalRect};

/// A FemtoVG canvas bound to the calling thread's current EGL/GL context, with the declared
/// font chain loaded and ready to draw with.
pub struct TextPainter {
    canvas: Canvas<OpenGl>,
    fonts: Vec<FontId>,
}

impl TextPainter {
    /// `load_fn` must resolve GL function pointers against a context that's already current on
    /// this thread -- FemtoVG doesn't make any context current itself.
    ///
    /// `font_chain` is `ShapingHandle::font_chain_data()`'s own output, in the same chain order
    /// cosmic-text shaped against, so measurement and paint resolve the one declared chain rather
    /// than two independently-discovered fonts that can disagree (docs/adr/0043 decision 2).
    /// Errors if the slice is empty -- `draw_line` cannot fall back to a font it was never given.
    ///
    /// Registers through a `TextContext` and `add_shared_font_with_index` rather than
    /// `Canvas::add_font_mem`, because `add_font_mem` is `data.to_owned()` inside femtovg: it
    /// would give the canvas a private copy of every font file and undo the sharing `FontData`
    /// exists for. `Canvas::add_font_mem` is the only route femtovg exposes on the canvas itself,
    /// so reaching the shared API means building the context first and handing it over.
    pub fn new(
        load_fn: impl FnMut(&str) -> *const c_void,
        width: u32,
        height: u32,
        font_chain: &[FontData],
    ) -> Result<Self, Box<dyn Error>> {
        if font_chain.is_empty() {
            return Err("TextPainter::new requires at least one loaded font".into());
        }
        let renderer = unsafe { OpenGl::new_from_function(load_fn)? };
        let text_context = TextContext::default();
        let mut canvas = Canvas::new_with_text_context(renderer, text_context.clone())?;
        canvas.set_size(width, height, 1.0);
        let fonts = font_chain
            .iter()
            .map(|data| text_context.add_shared_font_with_index(data.clone(), 0))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { canvas, fonts })
    }

    /// Updates the canvas's viewport to match the surface's current size. Cheap and idempotent --
    /// callers should call this every frame rather than caching a size from construction time.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.canvas.set_size(width, height, 1.0);
    }

    /// The same canvas `draw_line` fills text onto, exposed so `layout::paint`'s tree walk can
    /// draw a node's background/border on it too (build-steps.md Phase 19 item 6, docs/adr/0023)
    /// -- one canvas per surface, shared by every paint operation, not one per property kind.
    pub fn canvas_mut(&mut self) -> &mut Canvas<OpenGl> {
        &mut self.canvas
    }

    /// The loaded chain's `FontId`s, in chain order -- the test seam `layout::paint`'s
    /// divergence test uses to compare femtovg's measurement against `ShapingHandle::shape`'s.
    /// `draw_line` reaches `self.fonts` directly and has no need of this.
    #[cfg(test)]
    pub fn fonts(&self) -> &[FontId] {
        &self.fonts
    }

    /// Draws `text` with its snapped top-left corner at `rect`'s origin (build-steps.md
    /// Phase 4, point 3) in `color`. Does not flush or swap buffers -- `layout::paint`'s tree
    /// walk draws a whole surface's worth of nodes onto this same canvas and flushes once at
    /// the end (build-steps.md Phase 19 item 6).
    pub fn draw_line(&mut self, text: &str, rect: LogicalRect, font_size: f32, scale: f32, color: Rgba) {
        let physical = snap_to_physical(rect, scale);
        // `Rgba`'s four `f32` fields exist so `Color::rgbaf` takes them with no conversion.
        let mut paint = Paint::color(Color::rgbaf(color.r, color.g, color.b, color.a));
        // The whole chain, in chain order: FemtoVG's `set_font` does its own per-glyph fallback
        // across the slice it's given, the same way cosmic-text's shaping falls back across
        // `db`'s loaded faces.
        paint.set_font(&self.fonts);
        paint.set_font_size(font_size);
        // fill_text's y is the text baseline, not the box top, so it belongs at the snapped top
        // edge plus the font's ascender -- not the snapped bottom edge, which would cut
        // descenders off outside the box.
        let ascender = self.canvas.measure_font(&paint).map(|m| m.ascender()).unwrap_or(font_size);
        let baseline_y = physical.y0 as f32 + ascender;
        let _ = self.canvas.fill_text(physical.x0 as f32, baseline_y, text, &paint);
    }
}
