//! Glyph rasterization and GPU texture atlas management, via FemtoVG.
//!
//! FemtoVG owns its glyph atlas entirely internally (see ADR-0012): rasterized glyphs pack
//! into private atlas pages that start at a fixed size and grow by adding further pages, not by
//! expanding one large texture. There is no public API to configure a single fixed-size page.
//! What it draws is cosmic-text's (ADR-0211): `fill_glyph_run` takes glyphs another shaper placed,
//! so femtovg rasterizes and packs, and shapes nothing.

use std::collections::HashMap;
use std::error::Error;
use std::ffi::c_void;
use std::sync::Arc;

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, Color, FontId, Paint, Path, PositionedGlyph, TextContext};

use crate::layout::node::{Rgba, StyleRun, TextAlign, font_runs};
use crate::text::shaping::{FontFace, Glyph, ShapingHandle};

use super::snap::{LogicalRect, snap_to_physical};

/// A FemtoVG canvas bound to the calling thread's current EGL/GL context, with every face the
/// shaping worker can place a glyph in registered and ready to draw with.
pub struct TextPainter {
    canvas: Canvas<OpenGl>,
    /// femtovg's id for each face, keyed by the id cosmic-text's glyphs name it by, with the face
    /// itself for its variation axes (ADR-0211).
    faces: HashMap<fontdb::ID, (FontId, FontFace)>,
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
    /// The worker and memo that measured each line, asked again for its glyphs and its faces.
    shaping: ShapingHandle,
}

/// What [`TextPainter::draw_text`] draws, apart from where: one `Draw::Text` command's worth,
/// borrowed rather than cloned out of it.
pub struct TextDraw<'a> {
    pub text: &'a str,
    pub runs: &'a [StyleRun],
    pub font_size: f32,
    /// The family this node named (ADR-0144), the same one the box was measured under. `None` is
    /// the declared chain.
    pub font: Option<&'a Arc<str>>,
    pub color: Rgba,
    pub align: TextAlign,
}

/// Registers every face of `font_chain` femtovg does not hold yet, and maps each face's shaping id
/// to its `FontId`. `registered` carries the ids femtovg minted across calls, because it mints a
/// new one every time it is asked (see [`TextPainter::registered`]).
fn register(
    text_context: &TextContext,
    registered: &mut HashMap<(usize, u32), FontId>,
    font_chain: Vec<FontFace>,
) -> HashMap<fontdb::ID, (FontId, FontFace)> {
    let mut faces = HashMap::with_capacity(font_chain.len());
    for face in font_chain {
        let key = (face.data.addr(), face.index);
        let id = match registered.get(&key) {
            Some(id) => *id,
            None => match text_context.add_shared_font_with_index(face.data.clone(), face.index) {
                Ok(id) => {
                    registered.insert(key, id);
                    id
                }
                // fontdb accepts faces femtovg's parser refuses; that one draws nothing, the rest draw.
                Err(e) => {
                    eprintln!("font chain: femtovg refused face {:?}, skipped: {e}", face.id);
                    continue;
                }
            },
        };
        faces.insert(face.id, (id, face));
    }
    faces
}

/// The spans under the glyphs `in_run` picks, one per visually contiguous group of them: bidi can
/// split a run around text that is not in it (ADR-0211). `glyphs` are in visual order.
fn underline_spans(glyphs: &[Glyph], in_run: impl Fn(&Glyph) -> bool) -> Vec<(f32, f32)> {
    glyphs
        .chunk_by(|a, b| in_run(a) == in_run(b))
        .filter(|group| in_run(&group[0]))
        .map(|group| {
            group.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), glyph| {
                (lo.min(glyph.x), hi.max(glyph.x + glyph.advance))
            })
        })
        .filter(|(x0, x1)| x0 < x1)
        .collect()
}

impl TextPainter {
    /// `load_fn` must resolve GL function pointers against a context that's already current on
    /// this thread -- FemtoVG doesn't make any context current itself.
    ///
    /// Registers `shaping`'s own `font_chain_data()`, so every face a glyph can name is one femtovg
    /// holds (ADR-0211). Errors if femtovg loads none of it -- there would be nothing to draw with.
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
        shaping: ShapingHandle,
    ) -> Result<Self, Box<dyn Error>> {
        // SAFETY: femtovg loads every GL entry point through `load_fn` and calls them on this
        // thread. The caller binds the context with `eglMakeCurrent` before constructing this
        // (`wayland::surface::ensure_bound`, or the headless EGL helper in paint's tests),
        // and the renderer is used only from that same thread.
        let renderer = unsafe { OpenGl::new_from_function(load_fn)? };
        let text_context = TextContext::default();
        let mut canvas = Canvas::new_with_text_context(renderer, text_context.clone())?;
        canvas.set_size(width, height, 1.0);
        let mut registered = HashMap::new();
        // Generation before faces: a face loaded between the two reads leaves this behind, not ahead.
        let generation = shaping.font_generation();
        let faces = register(&text_context, &mut registered, shaping.font_chain_data());
        if faces.is_empty() {
            return Err("TextPainter::new requires at least one loaded font".into());
        }
        Ok(Self { canvas, faces, generation, text_context, registered, shaping })
    }

    /// The shaping-worker face-set generation this painter's femtovg registry is built from.
    #[cfg(test)]
    pub fn font_generation(&self) -> u64 {
        self.generation
    }

    /// Registers any faces the shaping worker has loaded since this painter was built (ADR-0144).
    pub fn sync(&mut self) {
        let generation = self.shaping.font_generation();
        if generation == self.generation {
            return;
        }
        let font_chain = self.shaping.font_chain_data();
        if font_chain.is_empty() {
            return;
        }
        self.faces = register(&self.text_context, &mut self.registered, font_chain);
        self.generation = generation;
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

    /// femtovg's id for a face the shaping worker names -- the test seam for checking a face
    /// reached the painter, and kept the id it was first given.
    #[cfg(test)]
    pub fn font_id(&self, face: fontdb::ID) -> Option<FontId> {
        self.faces.get(&face).map(|(id, _)| *id)
    }

    /// Draws `text` with its snapped top-left corner at `rect`'s origin, in `color`, row by row.
    /// Does not flush or swap buffers: `layout::paint`'s tree walk draws a whole surface's worth of
    /// nodes onto this same canvas and flushes once at the end.
    ///
    /// Rows are the glyphs [`ShapingHandle::shape_lines`] laid out, so measurement and paint share
    /// one shaper (ADR-0211); `runs` (ADR-0104) colour and underline by the byte each glyph came from.
    pub fn draw_text(&mut self, line: TextDraw<'_>, rect: LogicalRect, scale: f32) {
        let TextDraw { text, runs, font_size, font, color, align } = line;
        let physical = snap_to_physical(rect, scale);
        // ponytail: glyphs are placed at logical size in a physical-pixel canvas whose dpi is 1.0,
        // so on a fractional or 2x output every glyph in this shell draws at logical size. Upgrade
        // path: shape at the scaled size; only reachable with a HiDPI output to verify against.
        let step = crate::text::shaping::line_height(font_size);
        let thickness = (font_size / 16.0).max(1.0).round();
        let mut row = 0;
        for (line_start, shaped) in self.shaping.shape_lines(text, &font_runs(runs), font_size, font) {
            for laid in shaped.shaped.iter() {
                let left = align.line_left(laid.rtl, physical.x0 as f32, physical.x1 as f32, laid.width);
                let baseline = physical.y0 as f32 + row as f32 * step + laid.baseline;
                row += 1;
                let style = |start: usize| runs.iter().find(|run| run.range.contains(&(line_start + start)));
                let key = |glyph: &Glyph| {
                    (glyph.face, glyph.weight, style(glyph.start).and_then(|run| run.color).unwrap_or(color))
                };
                for group in laid.glyphs.chunk_by(|a, b| key(a) == key(b)) {
                    let glyphs = group.iter().map(|glyph| PositionedGlyph {
                        x: left + glyph.x,
                        y: baseline + glyph.y,
                        glyph_id: glyph.id,
                    });
                    self.fill_run(key(&group[0]), glyphs, font_size);
                }

                for run in runs.iter().filter(|run| run.underline) {
                    let tint = run.color.unwrap_or(color);
                    for (x0, x1) in
                        underline_spans(&laid.glyphs, |glyph| run.range.contains(&(line_start + glyph.start)))
                    {
                        let mut path = Path::new();
                        path.rect(left + x0, (baseline + thickness).round(), x1 - x0, thickness);
                        self.canvas.fill_path(&path, &Paint::color(Color::rgbaf(tint.r, tint.g, tint.b, tint.a)));
                    }
                }
            }
        }
    }

    /// Draws `glyphs` in `face` at `weight`; a face femtovg never registered draws nothing.
    fn fill_run(
        &mut self,
        (face, weight, tint): (fontdb::ID, u16, Rgba),
        glyphs: impl IntoIterator<Item = PositionedGlyph>,
        font_size: f32,
    ) {
        let Some((font, face)) = self.faces.get(&face) else { return };
        // A variable family's bold is an instance of one face, which cosmic-text shaped at `weight`.
        let (font, coords) = (*font, face.coords(weight));
        let mut paint = Paint::color(Color::rgbaf(tint.r, tint.g, tint.b, tint.a));
        paint.set_font_size(font_size);
        let _ = self.canvas.fill_glyph_run(font, &coords, glyphs, &paint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run bidi splits around other text is underlined piece by piece, never across the text
    /// between the pieces.
    #[test]
    fn a_run_split_around_other_text_is_underlined_under_each_piece() {
        let glyph = |x: f32, start: usize| Glyph {
            face: fontdb::ID::dummy(),
            weight: 400,
            id: 0,
            x,
            y: 0.0,
            advance: 10.0,
            start,
        };
        let glyphs = [glyph(0.0, 4), glyph(10.0, 5), glyph(20.0, 0), glyph(30.0, 6)];
        assert_eq!(underline_spans(&glyphs, |glyph| glyph.start >= 4), vec![(0.0, 20.0), (30.0, 40.0)]);
        assert_eq!(underline_spans(&glyphs, |glyph| glyph.start == 9), Vec::new());
    }
}
