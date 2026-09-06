//! Glyph rasterization and GPU texture atlas management, via FemtoVG.
//!
//! FemtoVG owns its glyph atlas entirely internally (see ADR-0012): rasterized glyphs pack
//! into private atlas pages that start at a fixed size and grow by adding further pages, not by
//! expanding one large texture. There is no public API to configure a single fixed-size page,
//! and no public API to feed it glyphs shaped by anything other than FemtoVG's own internal
//! shaper. This module drives FemtoVG's atlas through its public font/text API (`add_font_mem`,
//! `fill_text`) rather than reimplementing packing on top of it.

use std::error::Error;
use std::ffi::c_void;

use femtovg::renderer::OpenGl;
use femtovg::{Align, Canvas, Color, FontId, Paint, Path, TextContext};

use std::collections::HashMap;
use std::sync::Arc;

use crate::layout::node::{Rgba, StyleRun, TextAlign, segments};
use crate::text::shaping::{FaceRole, FontFace};

use super::snap::{LogicalRect, snap_to_physical};

/// A FemtoVG canvas bound to the calling thread's current EGL/GL context, with the declared
/// font chain loaded and ready to draw with.
pub struct TextPainter {
    canvas: Canvas<OpenGl>,
    /// One chain per face variant, indexed by [`variant`]: the declared family's face for that
    /// variant (or its regular face, for a variant it does not ship) followed by every fallback
    /// face, so per-glyph fallback works the same in bold as in regular (ADR-0104).
    fonts: [Vec<FontId>; 4],
    /// The same four chains for each family a node named by hand, keyed by the name the node
    /// wrote (ADR-0144). A name that is absent -- never asked for, or asked for and unresolvable
    /// -- falls back to `fonts`, so a typo draws in the declared family rather than as nothing.
    named: HashMap<Arc<str>, [Vec<FontId>; 4]>,
    /// The shaping worker's face-set generation this was built from, so [`TextPainter::sync`] can
    /// tell in one atomic load whether femtovg's registry is behind.
    generation: u64,
    /// femtovg's font registry, kept so a family first named at runtime can be added without
    /// rebuilding the canvas and losing its warm glyph atlas.
    text_context: TextContext,
    /// The `FontId` already registered for each `(FontData::addr, face index)`, so
    /// [`TextPainter::sync`] adds only what femtovg does not hold yet.
    ///
    /// Not an optimisation: `add_shared_font_with_index` is `self.fonts.insert(font)` into a
    /// `SlotMap`, which mints a new key every call and never dedups by bytes. Re-registering the
    /// whole list would re-parse every face, strand the previous entries in the slot map for the
    /// life of the surface, and call femtovg's `clear_caches` once per face (ADR-0144).
    registered: HashMap<(usize, u32), FontId>,
}

/// What [`TextPainter::draw_text`] draws, apart from where: one `Draw::Text` command's worth,
/// borrowed rather than cloned out of it.
pub struct TextDraw<'a> {
    pub text: &'a str,
    pub runs: &'a [StyleRun],
    pub font_size: f32,
    /// The family this node named (ADR-0144), the same one the box was measured under. `None` is
    /// the declared chain.
    pub font: Option<&'a str>,
    pub color: Rgba,
    pub align: TextAlign,
}

/// Which of a family's four chains a run draws with.
fn variant(bold: bool, italic: bool) -> usize {
    usize::from(bold) | (usize::from(italic) << 1)
}

/// Registers `font_chain` with femtovg and builds the declared family's four variant chains plus
/// one set of four per family a node named (ADR-0104, ADR-0144).
///
/// Every chain leads with its own family's face for the variant, then the shared fallback
/// coverage, then -- for a named family -- the declared family's regular face, so a node that
/// named a display font and then drew ordinary prose in it still gets glyphs that font lacks.
///
/// Not the mirror of that: a named family is never coverage for the declared chain. Plain text
/// must not drift into whichever family some unrelated node happened to name, and the declared
/// chain carries its own CJK and emoji fallbacks already.
/// `registered` carries the `FontId` femtovg minted for each face across calls, because it mints a
/// new one every time it is asked (see [`TextPainter::registered`]).
type Chains = ([Vec<FontId>; 4], HashMap<Arc<str>, [Vec<FontId>; 4]>);
fn build_chains(
    text_context: &TextContext,
    registered: &mut HashMap<(usize, u32), FontId>,
    font_chain: &[FontFace],
) -> Result<Chains, Box<dyn Error>> {
    let mut declared: [Option<FontId>; 4] = [None; 4];
    let mut named_faces: HashMap<Arc<str>, [Option<FontId>; 4]> = HashMap::new();
    let mut fallbacks = Vec::new();
    for face in font_chain {
        let id = match registered.get(&(face.data.addr(), face.index)) {
            Some(id) => *id,
            None => {
                let id = text_context.add_shared_font_with_index(face.data.clone(), face.index)?;
                registered.insert((face.data.addr(), face.index), id);
                id
            }
        };
        match &face.role {
            FaceRole::Declared => declared[variant(face.bold, face.italic)] = Some(id),
            FaceRole::Named(asked) => {
                named_faces.entry(Arc::clone(asked)).or_insert([None; 4])[variant(face.bold, face.italic)] = Some(id)
            }
            FaceRole::Fallback => fallbacks.push(id),
        }
    }

    let fonts = std::array::from_fn(|which| {
        let mut chain = Vec::with_capacity(fallbacks.len() + 1);
        chain.extend(declared[which].or(declared[0]));
        chain.extend(fallbacks.iter().copied());
        chain
    });

    let named = named_faces
        .into_iter()
        .map(|(asked, variants)| {
            let chains = std::array::from_fn(|which| {
                let mut chain = Vec::with_capacity(fallbacks.len() + 2);
                chain.extend(variants[which].or(variants[0]));
                chain.extend(fallbacks.iter().copied());
                chain.extend(declared[0]);
                chain
            });
            (asked, chains)
        })
        .collect();
    Ok((fonts, named))
}

/// Which femtovg alignment to set, and what x to hand `fill_text` under it.
///
/// `set_text_align` decides what the x it is given *means*, so the anchor moves with the alignment:
/// the near edge, the centre, or the far edge of the snapped box.
///
/// Its own function because everything around it needs a live GL context and this is arithmetic.
fn text_anchor(align: TextAlign, x0: i32, x1: i32) -> (Align, f32) {
    match align {
        TextAlign::Start => (Align::Left, x0 as f32),
        // Averaged in `f32` rather than `(x0 + x1) / 2` in `i32`, which would truncate an odd-width
        // box half a pixel to the left of its own centre.
        TextAlign::Center => (Align::Center, (x0 as f32 + x1 as f32) / 2.0),
        TextAlign::End => (Align::Right, x1 as f32),
    }
}

impl TextPainter {
    /// `load_fn` must resolve GL function pointers against a context that's already current on
    /// this thread -- FemtoVG doesn't make any context current itself.
    ///
    /// `font_chain` is `ShapingHandle::font_chain_data()`'s own output, in the same chain order
    /// cosmic-text shaped against, so measurement and paint resolve the one declared chain rather
    /// than two independently-discovered fonts that can disagree (ADR-0043 decision 2).
    /// Errors if the slice is empty -- `draw_text` cannot fall back to a font it was never given.
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
        font_chain: &[FontFace],
        generation: u64,
    ) -> Result<Self, Box<dyn Error>> {
        if font_chain.is_empty() {
            return Err("TextPainter::new requires at least one loaded font".into());
        }
        // SAFETY: femtovg loads every GL entry point through `load_fn` and calls them on this
        // thread. The caller binds the context with `eglMakeCurrent` before constructing this
        // (`wayland::surface` at its two call sites, the headless EGL helper in paint's tests),
        // and the renderer is used only from that same thread.
        let renderer = unsafe { OpenGl::new_from_function(load_fn)? };
        let text_context = TextContext::default();
        let mut canvas = Canvas::new_with_text_context(renderer, text_context.clone())?;
        canvas.set_size(width, height, 1.0);
        let mut registered = HashMap::new();
        let (fonts, named) = build_chains(&text_context, &mut registered, font_chain)?;
        Ok(Self { canvas, fonts, named, generation, text_context, registered })
    }

    /// The shaping-worker face-set generation this painter's femtovg registry is built from.
    pub fn font_generation(&self) -> u64 {
        self.generation
    }

    /// Registers any faces the shaping worker has loaded since this painter was built, and
    /// rebuilds the chains around them (ADR-0144).
    ///
    /// The point of the whole runtime path: a family a node names for the first time is resolved
    /// on the shaping worker, and paint has to reach the same faces or it measures in one family
    /// and draws in another. Called once a frame from `wayland::surface`, and a no-op on the
    /// generation it was built from -- which is every frame but the few where a new family lands.
    ///
    /// Chains are rebuilt, but faces are registered only once: `self.registered` is what keeps
    /// this from re-parsing every face and stranding femtovg's previous `Font` entries. The canvas
    /// itself -- and its warm glyph atlas -- is kept either way.
    pub fn sync(&mut self, font_chain: &[FontFace], generation: u64) {
        if generation == self.generation || font_chain.is_empty() {
            return;
        }
        match build_chains(&self.text_context, &mut self.registered, font_chain) {
            Ok((fonts, named)) => {
                self.fonts = fonts;
                self.named = named;
                self.generation = generation;
            }
            // Keep the chains that work rather than dropping to none. A font that failed to
            // register is a node drawing in the declared family, not a dead surface.
            Err(e) => eprintln!("font chain: femtovg refused a face after a runtime load, keeping the old chains: {e}"),
        }
    }

    /// Updates the canvas's viewport to match the surface's current size. Cheap and idempotent --
    /// callers should call this every frame rather than caching a size from construction time.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.canvas.set_size(width, height, 1.0);
    }

    /// The same canvas `draw_text` fills text onto, exposed so `layout::paint`'s tree walk can
    /// draw a node's background/border on it too (ADR-0023): one canvas per surface, shared by
    /// every paint operation, not one per property kind.
    pub fn canvas_mut(&mut self) -> &mut Canvas<OpenGl> {
        &mut self.canvas
    }

    /// The loaded chain's `FontId`s, in chain order -- the test seam `layout::paint`'s
    /// divergence test uses to compare femtovg's measurement against `ShapingHandle::shape`'s.
    /// `draw_text` reaches `self.fonts` directly and has no need of this.
    #[cfg(test)]
    pub fn fonts(&self) -> &[FontId] {
        &self.fonts[0]
    }

    /// The chain for a bold and/or italic run -- the test seam for checking a variant chain leads
    /// with a different face than the regular one when the family ships it.
    #[cfg(test)]
    pub fn variant_fonts(&self, bold: bool, italic: bool) -> &[FontId] {
        &self.fonts[variant(bold, italic)]
    }

    /// The chain a named family leads, or `None` when nothing on the system answered that name --
    /// the test seam for checking a named family gets a chain of its own.
    #[cfg(test)]
    pub fn named_fonts(&self, family: &str) -> Option<&[FontId]> {
        self.named.get(family).map(|chains| &chains[0][..])
    }

    /// The chain a node draws with (ADR-0144): the family it named when that family resolved, and
    /// the declared chain's matching variant otherwise. A name nothing answered is absent from the
    /// map, and answering that with the declared chain is what makes a typo'd family draw the text
    /// in the wrong face rather than not at all.
    fn chain_for(&self, font: Option<&str>, bold: bool, italic: bool) -> &[FontId] {
        font.and_then(|family| self.named.get(family))
            .map_or(&self.fonts[variant(bold, italic)], |chains| &chains[variant(bold, italic)])
    }

    /// Draws `text` with its snapped top-left corner at `rect`'s origin, in `color`, one
    /// `fill_text` per line. Does not flush or swap buffers: `layout::paint`'s tree walk draws a
    /// whole surface's worth of nodes onto this same canvas and flushes once at the end.
    ///
    /// Lines are `\n`-separated, put there by `layout::scene`'s `fit_text_to_box` from the breaks
    /// cosmic-text found. femtovg has no line breaker and draws `\n` as a glyph, so splitting here
    /// is not a convenience -- it is the only reason a wrapped `text` renders as more than one
    /// clipped line. A string with no newline in it takes exactly the path it always did.
    ///
    /// `runs` are the styled stretches of `text` (ADR-0104), byte ranges into it. With none, each
    /// line is one `fill_text` under femtovg's own alignment, the path this always took. With
    /// some, a line is drawn piece by piece -- each piece in its run's face and colour, advanced
    /// by femtovg's own measurement of it -- and the alignment is computed from the pieces' total,
    /// since femtovg's `set_text_align` can only place one run.
    pub fn draw_text(&mut self, line: TextDraw<'_>, rect: LogicalRect, scale: f32) {
        let TextDraw { text, runs, font_size, font, color, align } = line;
        let physical = snap_to_physical(rect, scale);
        // `Rgba`'s four `f32` fields exist so `Color::rgbaf` takes them with no conversion.
        let mut paint = Paint::color(Color::rgbaf(color.r, color.g, color.b, color.a));
        // The whole chain, in chain order: FemtoVG's `set_font` does its own per-glyph fallback
        // across the slice it's given, the same way cosmic-text's shaping falls back across
        // `db`'s loaded faces.
        paint.set_font(self.chain_for(font, false, false));
        paint.set_font_size(font_size);
        // fill_text's y is the text baseline, not the box top, so it belongs at the snapped top
        // edge plus the font's ascender -- not the snapped bottom edge, which would cut
        // descenders off outside the box.
        // femtovg's own alignment rather than a measured offset: `set_text_align` decides what the
        // x it is handed *means*, so a centred run needs the box's centre and a right-aligned one
        // its far edge. Measuring the run here to compute a left offset would be a second
        // measurement, against femtovg's metrics rather than the cosmic-text ones the box was sized
        // with, which is exactly the disagreement `text::shaping`'s module doc records.
        let (femto_align, anchor_x) = text_anchor(align, physical.x0, physical.x1);
        paint.set_text_align(femto_align);
        let ascender = self.canvas.measure_font(&paint).map(|m| m.ascender()).unwrap_or(font_size);
        // The same step `text::shaping` measured the box with, so the lines land where the height
        // was reserved for them.
        //
        // ponytail: unscaled, matching `set_font_size` just above, which is handed the logical size
        // against a canvas whose dpi is 1.0 -- so on a fractional or 2x output every glyph in this
        // shell already draws at logical size in a physical-pixel canvas. Advancing by a scaled
        // step would space correctly-spaced lines around wrong-sized glyphs. Upgrade path: scale
        // the font size here and let this follow it; only reachable with a HiDPI output to verify
        // against.
        let step = crate::text::shaping::line_height(font_size);
        if runs.is_empty() {
            for (index, line) in text.lines().enumerate() {
                let baseline_y = physical.y0 as f32 + ascender + index as f32 * step;
                let _ = self.canvas.fill_text(anchor_x, baseline_y, line, &paint);
            }
            return;
        }

        // Styled: every piece is placed by hand from the left, so femtovg's alignment is turned
        // off and the anchor is the line's own left edge under the requested alignment.
        paint.set_text_align(Align::Left);
        let mut line_start = 0usize;
        for (index, line) in text.split('\n').enumerate() {
            let baseline_y = physical.y0 as f32 + ascender + index as f32 * step;
            let pieces = segments(line_start..line_start + line.len(), runs);
            let mut painted: Vec<(&str, Paint, Rgba, f32, Option<&StyleRun>)> = Vec::with_capacity(pieces.len());
            for (range, run) in pieces {
                let piece = &text[range];
                let mut piece_paint = paint.clone();
                let mut piece_color = color;
                if let Some(run) = run {
                    piece_paint.set_font(self.chain_for(font, run.bold, run.italic));
                    if let Some(c) = run.color {
                        piece_color = c;
                        piece_paint.set_color(Color::rgbaf(c.r, c.g, c.b, c.a));
                    }
                }
                let width = self.canvas.measure_text(0.0, 0.0, piece, &piece_paint).map(|m| m.width()).unwrap_or(0.0);
                painted.push((piece, piece_paint, piece_color, width, run));
            }
            let total: f32 = painted.iter().map(|(_, _, _, width, _)| width).sum();
            let mut x = match align {
                TextAlign::Start => physical.x0 as f32,
                TextAlign::Center => (physical.x0 as f32 + physical.x1 as f32) / 2.0 - total / 2.0,
                TextAlign::End => physical.x1 as f32 - total,
            };
            for (piece, piece_paint, piece_color, width, run) in painted {
                let _ = self.canvas.fill_text(x, baseline_y, piece, &piece_paint);
                if run.is_some_and(|run| run.underline) {
                    // Just under the baseline, a stroke proportional to the size and never thinner
                    // than a pixel: a 12px label gets a hairline, a 32px heading a 2px rule.
                    let thickness = (font_size / 16.0).max(1.0).round();
                    let mut path = Path::new();
                    path.rect(x, (baseline_y + thickness).round(), width, thickness);
                    let rule = Paint::color(Color::rgbaf(piece_color.r, piece_color.g, piece_color.b, piece_color.a));
                    self.canvas.fill_path(&path, &rule);
                }
                x += width;
            }
            line_start += line.len() + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The anchor has to move with the alignment, because `set_text_align` changes what the x
    /// means rather than shifting the run under a fixed one.
    #[test]
    fn the_anchor_is_the_edge_the_alignment_measures_from() {
        assert_eq!(text_anchor(TextAlign::Start, 10, 90), (Align::Left, 10.0));
        assert_eq!(text_anchor(TextAlign::Center, 10, 90), (Align::Center, 50.0));
        assert_eq!(text_anchor(TextAlign::End, 10, 90), (Align::Right, 90.0));
    }

    /// An odd-width box centres on a half pixel. Averaging in `i32` first would truncate it left,
    /// which is a half-pixel drift that only shows on some box widths and not others.
    #[test]
    fn an_odd_width_box_centres_on_its_true_middle() {
        assert_eq!(text_anchor(TextAlign::Center, 0, 15).1, 7.5);
    }

    #[test]
    fn the_variant_index_is_bold_then_italic() {
        assert_eq!(variant(false, false), 0);
        assert_eq!(variant(true, false), 1);
        assert_eq!(variant(false, true), 2);
        assert_eq!(variant(true, true), 3);
    }

    /// A zero-width box is degenerate but reachable (a `Content`-sized node holding an empty
    /// string), and all three alignments have to agree on it rather than one of them drifting.
    #[test]
    fn a_zero_width_box_anchors_every_alignment_at_the_same_point() {
        let x = 42;
        assert_eq!(text_anchor(TextAlign::Start, x, x).1, 42.0);
        assert_eq!(text_anchor(TextAlign::Center, x, x).1, 42.0);
        assert_eq!(text_anchor(TextAlign::End, x, x).1, 42.0);
    }
}
