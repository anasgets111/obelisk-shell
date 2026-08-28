//! Draws a resolved layout tree onto a shared femtovg canvas (build-steps.md Phase 19 item 6).
//!
//! `layout::node`'s paint-property parsers (`parse_background`, `parse_radius`,
//! `parse_border_color`, `parse_border_width`, `parse_foreground`) landed with no caller in the
//! commit before this one -- this module is that caller. It walks a `layout::scene::ResolvedNode`
//! tree (`Scene::surface`'s output, docs/adr/0023) and turns each node's already-resolved
//! properties into femtovg draw calls on `text::atlas::TextPainter`'s canvas, the same canvas
//! `draw_line` already draws glyphs on -- one canvas, one flush per surface per frame, not one
//! flush per node.
//!
//! Draws in tree order, parent then children in order: that is what makes the stacking model
//! docs/adr/0023 item 4 already implements resolve overlaps the same way layout resolved them --
//! a later sibling or a child paints over what an earlier one already put down. An invisible node
//! (`visible == false`) and its whole subtree draw nothing, the same collapse
//! `layout::scene::resolve_and_reconcile`'s own doc comment already picked for row/column space
//! reservation.
//!
//! `ResolvedNode.rect` is parent-relative (`layout::scene::position_children` sets a child's
//! `rect.x`/`rect.y` to its parent's padding plus its offset *within* the parent's content box),
//! so [`paint_node`] accumulates an absolute origin as it descends rather than trusting `rect.x`/
//! `rect.y` as already-absolute. Get this wrong and every subtree nests at the surface's top-left
//! corner instead of its real position.

use std::collections::HashMap;

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, Color, Paint, Path};
use mlua::Value;

use crate::layout::node::{self, BorderColor, EdgeInsets, LayoutError, Rgba};
use crate::layout::scene::ResolvedNode;
use crate::text::atlas::TextPainter;
use crate::text::snap::{snap_border_band, snap_to_physical, LogicalRect};

/// Draws `root` and its whole subtree onto `painter`'s canvas, then flushes once. `scale` is the
/// physical/logical pixel ratio `text::snap::snap_to_physical` and `TextPainter::draw_line` take
/// everywhere else in this crate -- every existing call site in `wayland::mod` hardcodes `1.0`
/// today, and this function makes no different assumption.
///
/// ponytail: no production caller yet. `socket.rs`'s `RendererClient` keys a `Scene` by the `id`
/// a config writes (`"bar"`); `wayland::mod` keys a `wl_surface` by `SurfaceRole::label()`
/// (`"main_bar"`, `"overlay_canvas"`, `"wallpaper_layer@{output}"`). Those two id spaces don't
/// overlap, so there is no surface whose retained tree a lookup could find -- see `socket.rs`'s
/// `PLACEHOLDER_OUTPUT_SIZE` doc comment for the fuller account. Any mapping between the two id
/// spaces invented here would be policy build-steps.md Phase 20 item 4 deletes outright, once it
/// removes `SurfaceRole` and makes the ids one space; that is this function's first real caller.
/// Exercised by this module's own headless-EGL tests only.
#[allow(dead_code)]
pub fn paint_tree(painter: &mut TextPainter, root: &ResolvedNode, scale: f32) {
    paint_node(painter, root, 0.0, 0.0, scale);
    painter.canvas_mut().flush();
}

/// One node, then its children (tree order, see this module's doc comment). `origin_x`/`origin_y`
/// is the absolute position of this node's parent's content box -- added to `node.rect.x`/`.y`
/// (parent-relative) to get this node's absolute rect, which is in turn what the next recursion
/// level's origin becomes.
fn paint_node(painter: &mut TextPainter, node: &ResolvedNode, origin_x: f32, origin_y: f32, scale: f32) {
    if !node.visible {
        return;
    }

    let x = origin_x + node.rect.x;
    let y = origin_y + node.rect.y;
    let rect = LogicalRect { x, y, width: node.rect.width, height: node.rect.height };

    // build-steps.md Phase 19 item 17: a node's own draw and its whole subtree are clipped to
    // this box, snapped the same way `draw_line` snaps its glyph origin (`crate::text::snap`) so
    // the clip edge and the glyph's physical placement agree. `intersect_scissor`, not `scissor`:
    // `save`/`restore` nest through the recursive call below, and `intersect_scissor` composes
    // the new box with whatever clip a parent already pushed, so a child can only shrink the
    // clipped region further -- never escape its parent's box the way `scissor` (which replaces
    // the active clip outright) would let it.
    //
    // ponytail: clipping is the floor, not the finished behavior (build-steps.md Phase 19 item
    // 17 names this directly). § 3.2 gives `text` a wrap at the available width, and
    // `layout::scene::intrinsic_content_size` already measures a `Content`-sized text box against
    // exactly that width (its `text_wrap_width` local, passed to `ShapingHandle::shape` as
    // `max_width`) -- but `ShapeResult` returns only a bounding `width`/`height`, not the wrapped
    // lines that produced it, and `paint_text` re-reads the raw `content` string and hands the
    // whole thing to `draw_line` in one `fill_text` call. So a box sized correctly for N wrapped
    // lines gets one unwrapped line painted into it, and this clip cuts that line off at the
    // first line's width instead of showing the rest on line two. The honest fix is paint asking
    // for the same wrapped line breaks layout already measured with, not a new clip strategy.
    //
    // ponytail: this clip is always rectangular, so a node with `radius > 0.0` clips overflowing
    // children to square corners while its own background underneath is rounded -- an escaping
    // child's corner pixels sit outside the round fill but inside the square clip. femtovg's
    // `intersect_rounded_scissor` exists and `paint_box` already computes `radius` for this exact
    // node, but that function's own doc comment (femtovg 0.26.0 src/lib.rs:895-899) only gives
    // exact rounded corners "when this is the first active scissor or... the previous clip is a
    // containing rectangle with the same transform" -- false in general here, since a nested
    // node's clip is intersected against every ancestor's, not the first. Rather than ship
    // rounded corners that are exact for a root node and silently degrade to square for anything
    // nested under one, this keeps the clip rectangular everywhere; switching to
    // `intersect_rounded_scissor` is the upgrade path once a config actually needs it.
    let physical = snap_to_physical(rect, scale);
    painter.canvas_mut().save();
    painter.canvas_mut().intersect_scissor(
        physical.x0 as f32,
        physical.y0 as f32,
        (physical.x1 - physical.x0) as f32,
        (physical.y1 - physical.y0) as f32,
    );

    match node.kind.as_str() {
        // `oblisk-idl-api-specs.md` § 5.2: row/column/button have no paint properties of their
        // own beyond the base `rect` ones they share the property table with, and a panel's
        // own root paints exactly like a rect -- one code path serves all five.
        "rect" | "row" | "column" | "button" | "panel" => paint_box(painter.canvas_mut(), &node.kind, &node.properties, rect, scale),
        "text" => paint_text(painter, &node.properties, rect, scale),
        // Deferred (build-steps.md Phase 19, "Also deferred: icon"): § 5.2 item 5's theme-name
        // `icon.name` and `oblisk-supervisor-services-dbus.md` § 9.2's path-taking
        // `system:find_icon` are two competing resolvers, neither built yet, and picking one here
        // would be settling that conflict as a side effect of a paint commit rather than the
        // phase actually settling it. Draws nothing.
        "icon" => {}
        // `textfield` reaches here, and drawing nothing is the spec-correct answer rather than an
        // omission: § 5.1's base properties carry no paint at all, and § 5.2 item 8 gives
        // `textfield` only `placeholder`/`mask_character`/`secure_submit`/`on_change`/`on_submit`,
        // so there is no background, border or foreground on it for this pass to read. What it
        // does need drawn (its placeholder, its masked or unmasked value, a caret) is input state
        // this module cannot see, and belongs with Phase 21's input routing.
        //
        // `layout::scene::ensure_supported_kind`'s list is what bounds this arm: every kind it
        // admits is named above or here, so a new kind added there without a decision here draws
        // nothing silently. Named rather than left to a bare catch-all for exactly that reason.
        _ => {}
    }

    for child in &node.children {
        paint_node(painter, child, x, y, scale);
    }

    // Inside the clip, not after it: a child's own `save`/`restore` pair balances within this
    // one, so the subtree above painted under this node's box as well as its own.
    painter.canvas_mut().restore();
}

/// One paint-property parse gone wrong: logged and treated as absent/default rather than
/// aborting the whole tree walk over one node's malformed `background`/`radius`/`border_*` --
/// `layout::scene::Scene::apply` never calls these parsers (build-steps.md Phase 19 item 5's own
/// doc comment: paint-only properties are resolved but never parsed there), so this is genuinely
/// the first validation a bad literal like `background = 5` ever meets, and a config error in one
/// node's paint properties shouldn't blank the surface around it.
///
/// ponytail: one line per bad property per node per call, and once `paint_tree` has a real caller
/// that is per frame, not per config edit. A single `background = 5` becomes an unbounded log
/// stream on the Wayland dispatch thread, and each line re-formats the rejected value's whole
/// `Debug` form, which build-steps.md Phase 19 item 13 measures at 20 MB for a hostile string. That
/// item judged the cost acceptable while it was paid once per apply; this pass is what changes the
/// cadence, and item 13 now records that. The real fix is not rate-limiting here: it is parsing
/// paint properties once at apply time, where a failure reaches `rescue` and rolls back the way a
/// bad `align_v` already does, instead of being logged past on every frame. That is the same
/// "parse geometry once into the retained node" item 5 defers, and it wants both halves at once.
fn log_paint_error(kind: &str, property: &str, err: &LayoutError) {
    eprintln!("[oblisk-renderer] paint: {kind}.{property}: {err}");
}

/// `rect`/`row`/`column`/`button`/`panel`'s shared paint: background fill, then borders
/// (`oblisk-idl-api-specs.md` § 5.2 item 1).
fn paint_box(canvas: &mut Canvas<OpenGl>, kind: &str, properties: &HashMap<String, Value>, rect: LogicalRect, scale: f32) {
    let radius = match node::parse_radius(properties) {
        Ok(r) => r,
        Err(e) => {
            log_paint_error(kind, "radius", &e);
            0.0
        }
    };

    match node::parse_background(properties) {
        // `None` skips the fill entirely; `Some` with alpha 0 still paints a transparent rect --
        // `parse_background`'s own doc comment calls this distinction deliberate, so both arms
        // below matter even though a transparent fill is invisible either way.
        Ok(Some(color)) => fill_rect(canvas, rect, radius, color),
        Ok(None) => {}
        Err(e) => log_paint_error(kind, "background", &e),
    }

    let colors = match node::parse_border_color(properties) {
        Ok(c) => c,
        Err(e) => {
            log_paint_error(kind, "border_color", &e);
            BorderColor::default()
        }
    };
    let widths = match node::parse_border_width(properties) {
        Ok(w) => w,
        Err(e) => {
            log_paint_error(kind, "border_width", &e);
            EdgeInsets::default()
        }
    };
    paint_border(canvas, rect, radius, colors, widths, scale);
}

fn fill_rect(canvas: &mut Canvas<OpenGl>, rect: LogicalRect, radius: f32, color: Rgba) {
    let mut path = Path::new();
    if radius > 0.0 {
        path.rounded_rect(rect.x, rect.y, rect.width, rect.height, radius);
    } else {
        path.rect(rect.x, rect.y, rect.width, rect.height);
    }
    canvas.fill_path(&path, &Paint::color(Color::rgbaf(color.r, color.g, color.b, color.a)));
}

/// femtovg has no per-edge border primitive, so this covers exactly two cases (the simplification
/// this slice is deliberately handed, not a gap found later). Uniform borders -- all four edges
/// the same width and colour -- with `radius` above 0 get one `stroke_path` over the rounded
/// rect, inset by half the stroke width: femtovg strokes centred on the path, so drawing directly
/// on `rect`'s own edge would have the stroke straddle it, half inside and half outside the box.
/// Everything else -- any edge differing from another, or radius 0 -- fills each edge that
/// declares both a non-zero width and a colour as its own rectangle.
///
/// ponytail: the per-edge-rectangle fallback ignores `radius` entirely, so a config that combines
/// a radius with per-edge widths or colours gets square corners where the rounded background
/// underneath shows through. femtovg exposes no public per-corner arc primitive this could build
/// on without duplicating its internal `rounded_rect_varying` construction; the upgrade path is
/// exactly that -- four independent corner arcs plus four edge segments, mitred at each join --
/// once a real config needs a rounded per-edge border rather than this approximation.
fn paint_border(canvas: &mut Canvas<OpenGl>, rect: LogicalRect, radius: f32, colors: BorderColor, widths: EdgeInsets, scale: f32) {
    let uniform_width = widths.top == widths.right && widths.right == widths.bottom && widths.bottom == widths.left;
    let uniform_color = matches!(
        (colors.top, colors.right, colors.bottom, colors.left),
        (Some(t), Some(r), Some(b), Some(l)) if t == r && r == b && b == l
    );

    if uniform_width && uniform_color && widths.top > 0.0 && radius > 0.0 {
        let color = colors.top.expect("uniform_color's match arm above guarantees Some on every edge");
        // Rounding a box's own two edges to nearest is exactly what `snap_border_band`
        // does, so it also snaps the node's span on each axis, not only a hairline's
        // thickness. The stroke's thickness is snapped the same way (band-of-one
        // starting at `rect.x`, only the thickness half of the result kept) so that
        // with integer box edges and an integer thickness, the centerline lands on an
        // integer for an even width and a half-integer for an odd one -- the parity
        // femtovg actually rasterizes (this module's `snap_border_band` doc comment).
        let (box_x, box_width) = snap_border_band(rect.x, rect.width, scale);
        let (box_y, box_height) = snap_border_band(rect.y, rect.height, scale);
        let (_, width) = snap_border_band(rect.x, widths.top, scale);
        let inset = width / 2.0;
        let mut path = Path::new();
        path.rounded_rect(box_x + inset, box_y + inset, (box_width - width).max(0.0), (box_height - width).max(0.0), radius);
        let mut paint = Paint::color(Color::rgbaf(color.r, color.g, color.b, color.a));
        paint.set_line_width(width);
        canvas.stroke_path(&path, &paint);
        return;
    }

    // Corners overlap here rather than mitre -- each edge is its own filled rect spanning the
    // node's full width or height, so two adjacent non-zero edges both cover the corner they
    // share.
    paint_border_edge(canvas, colors.top, widths.top, LogicalRect { x: rect.x, y: rect.y, width: rect.width, height: widths.top }, EdgeAxis::Horizontal, scale);
    paint_border_edge(
        canvas,
        colors.bottom,
        widths.bottom,
        LogicalRect { x: rect.x, y: rect.y + rect.height - widths.bottom, width: rect.width, height: widths.bottom },
        EdgeAxis::Horizontal,
        scale,
    );
    paint_border_edge(canvas, colors.left, widths.left, LogicalRect { x: rect.x, y: rect.y, width: widths.left, height: rect.height }, EdgeAxis::Vertical, scale);
    paint_border_edge(
        canvas,
        colors.right,
        widths.right,
        LogicalRect { x: rect.x + rect.width - widths.right, y: rect.y, width: widths.right, height: rect.height },
        EdgeAxis::Vertical,
        scale,
    );
}

/// Which dimension of an edge rect is the thin one -- the top/bottom edges span the
/// node's full width and are thin in y, left/right span the full height and are thin
/// in x. `paint_border_edge` needs this to know which axis to hand `snap_border_band`;
/// inferring it from the rect's own width/height would be ambiguous whenever a node's
/// height happens to equal its border width.
enum EdgeAxis {
    Horizontal,
    Vertical,
}

/// One border edge: paints only where both a colour and a non-zero width say so
/// (`node::parse_border_color`'s doc comment -- `border_width` alone is documented § 5.2
/// behaviour, not a bug this should work around). Snaps the edge's thin axis with
/// `snap_border_band` before building the path, so a filled-edge border gets the same
/// whole-physical-pixel treatment as the uniform-radius stroke above (build-steps.md
/// Phase 19 item 7) -- the long axis is left alone, since only the thin axis can
/// straddle a pixel boundary and blur.
fn paint_border_edge(canvas: &mut Canvas<OpenGl>, color: Option<Rgba>, width: f32, edge_rect: LogicalRect, axis: EdgeAxis, scale: f32) {
    let Some(color) = color else { return };
    if width <= 0.0 {
        return;
    }
    let edge_rect = match axis {
        EdgeAxis::Horizontal => {
            let (y, height) = snap_border_band(edge_rect.y, edge_rect.height, scale);
            LogicalRect { y, height, ..edge_rect }
        }
        EdgeAxis::Vertical => {
            let (x, width) = snap_border_band(edge_rect.x, edge_rect.width, scale);
            LogicalRect { x, width, ..edge_rect }
        }
    };
    let mut path = Path::new();
    path.rect(edge_rect.x, edge_rect.y, edge_rect.width, edge_rect.height);
    canvas.fill_path(&path, &Paint::color(Color::rgbaf(color.r, color.g, color.b, color.a)));
}

/// `text` (`oblisk-idl-api-specs.md` § 5.2 item 4): `content` through `TextPainter`, at `rect`,
/// coloured by `foreground`. A parse error on `font_size`/`foreground` falls back to the same
/// defaults those parsers already return for an absent key, so a malformed value degrades to
/// "as if omitted" rather than blanking the node's text entirely.
///
/// ponytail: a `Content`-sized `text` box comes from cosmic-text's measurement
/// (`layout::scene::intrinsic_content_size`), while `paint_node`'s clip now cuts this draw off at
/// that same box -- so if femtovg ever renders wider than cosmic-text measured, the clip shaves
/// the overrun off the right edge instead of letting it paint over a neighbour. Checked against
/// the current font chain (single-face Noto Sans, no fallback triggered) with a scratch test
/// rendering unclipped and scanning pixels past the measured edge, for both a short string at
/// 24px and a 53-character string at 32px: femtovg's `measure_text` agreed with cosmic-text's
/// `shape()` to within 0.0001px on the longer string, and the last lit (non-background) pixel in
/// both cases sat 3-4 physical pixels inside the measured edge, not past it. No shaving observed
/// on this chain. It is still the correct outcome if a future font or fallback face renders wider
/// than it measures: the alternative is the overrun landing on whatever sits to the right, which
/// is the exact bug this item fixes.
fn paint_text(painter: &mut TextPainter, properties: &HashMap<String, Value>, rect: LogicalRect, scale: f32) {
    let content = match node::parse_content(properties) {
        Ok(c) => c,
        Err(e) => {
            log_paint_error("text", "content", &e);
            String::new()
        }
    };
    let font_size = match node::parse_font_size(properties) {
        Ok(s) => s,
        Err(e) => {
            log_paint_error("text", "font_size", &e);
            12.0
        }
    };
    let color = match node::parse_foreground(properties) {
        Ok(c) => c,
        Err(e) => {
            log_paint_error("text", "foreground", &e);
            Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }
        }
    };
    painter.draw_line(&content, rect, font_size, scale, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::ffi::c_void;

    use khronos_egl as egl;
    use mlua::Lua;

    use crate::lua::nodes::{deserialize_lua_table, register_node_constructors};
    use crate::lua::signal;
    use crate::layout::scene::{LogicalSize, Scene};
    use crate::text::shaping::{ShapeRequest, ShapingHandle};

    // `EGL_PLATFORM_SURFACELESS_MESA` -- not in khronos-egl 6.0.0's constant list (confirmed by
    // reading its source; the crate ships no `PLATFORM_*` constants at all), so this is the raw
    // value from Mesa's own `EGL/eglmesaext.h`. Requesting it via `get_platform_display` is what
    // gives a GLES context with no on-screen display or compositor at all, which is what makes
    // this harness runnable in CI as well as on a developer's desktop with a real GPU.
    const PLATFORM_SURFACELESS_MESA: egl::Enum = 0x31DD;

    /// `None` on any failure, with an `eprintln!` naming which step -- "EGL init failed, skip" is
    /// the gate a driverless CI box takes; this machine has a working Mesa/Iris (and llvmpipe
    /// under `LIBGL_ALWAYS_SOFTWARE=1`) and is expected to actually run every test below, not
    /// skip them.
    ///
    /// Returns just the `Instance` -- `Display`/`Surface`/`Context` (confirmed by reading
    /// khronos-egl's source: bare handle newtypes, no `Drop` impl) need no further Rust-side
    /// ownership once `make_current` below has bound them to this thread; only `instance` is
    /// read again, for `get_proc_address` in [`text_painter`]. Binds a pbuffer surface (confirmed
    /// working on this machine's Mesa 26.2 -- no FBO plumbing needed, unlike a real window
    /// surface) current before returning.
    fn init_headless_egl(width: i32, height: i32) -> Option<egl::Instance<egl::Static>> {
        let instance = egl::Instance::new(egl::Static);

        // SAFETY: `PLATFORM_SURFACELESS_MESA` needs no native display handle -- `DEFAULT_DISPLAY`
        // (null) is the documented argument for it, the same convention EGL's other platformless
        // extensions use.
        let display = match unsafe { instance.get_platform_display(PLATFORM_SURFACELESS_MESA, egl::DEFAULT_DISPLAY, &[egl::ATTRIB_NONE]) } {
            Ok(d) => d,
            Err(e) => {
                eprintln!("EGL init failed, skip: eglGetPlatformDisplay(SURFACELESS_MESA): {e}");
                return None;
            }
        };

        if let Err(e) = instance.initialize(display) {
            eprintln!("EGL init failed, skip: eglInitialize: {e}");
            return None;
        }

        if let Err(e) = instance.bind_api(egl::OPENGL_ES_API) {
            eprintln!("EGL init failed, skip: eglBindAPI(OPENGL_ES_API): {e}");
            return None;
        }

        // Own config chooser, not `wayland::egl::satisfies_requirements`: that function requires
        // `WINDOW_BIT`, the wrong surface type for a pbuffer -- this asks for `PBUFFER_BIT`
        // instead, otherwise the same GLES3/8-bit-per-channel shape.
        let attribs = [
            egl::SURFACE_TYPE,
            egl::PBUFFER_BIT,
            egl::RENDERABLE_TYPE,
            egl::OPENGL_ES3_BIT,
            egl::RED_SIZE,
            8,
            egl::GREEN_SIZE,
            8,
            egl::BLUE_SIZE,
            8,
            egl::ALPHA_SIZE,
            8,
            egl::NONE,
        ];
        let config = match instance.choose_first_config(display, &attribs) {
            Ok(Some(c)) => c,
            Ok(None) => {
                eprintln!("EGL init failed, skip: no EGL config satisfies PBUFFER+GLES3+8-bit-RGBA");
                return None;
            }
            Err(e) => {
                eprintln!("EGL init failed, skip: eglChooseConfig: {e}");
                return None;
            }
        };

        let pbuffer_attribs = [egl::WIDTH, width, egl::HEIGHT, height, egl::NONE];
        let surface = match instance.create_pbuffer_surface(display, config, &pbuffer_attribs) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("EGL init failed, skip: eglCreatePbufferSurface: {e}");
                return None;
            }
        };

        let context_attribs = [egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE];
        let context = match instance.create_context(display, config, None, &context_attribs) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("EGL init failed, skip: eglCreateContext: {e}");
                return None;
            }
        };

        if let Err(e) = instance.make_current(display, Some(surface), Some(surface), Some(context)) {
            eprintln!("EGL init failed, skip: eglMakeCurrent: {e}");
            return None;
        }

        Some(instance)
    }

    /// Builds a `TextPainter` against `instance`'s already-current context -- same
    /// `font_chain_bytes` source `wayland::mod`'s own `draw_main_bar_proof_text` uses, so this
    /// harness draws with the exact declared font chain cosmic-text shaped against
    /// (docs/adr/0043 decision 2).
    fn text_painter(instance: &egl::Instance<egl::Static>, shaping: &ShapingHandle, width: u32, height: u32) -> Option<TextPainter> {
        let font_chain_bytes = shaping.font_chain_bytes();
        TextPainter::new(
            |s| instance.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void),
            width,
            height,
            &font_chain_bytes,
        )
        .map_err(|e| eprintln!("EGL init failed, skip: FemtoVG init: {e}"))
        .ok()
    }

    /// Evaluates `lua_src` as one surface's tree (same `surface_from` shape `layout::scene`'s own
    /// tests use, private to that module's test list so rebuilt here), applies it, and returns the
    /// resolved root at `size`. Panics on any layout error -- every fixture below is a config this
    /// harness controls, so a rejection is this test's own bug, not something to assert on.
    fn resolved_surface(lua: &Lua, lua_src: &str, size: LogicalSize) -> ResolvedNode {
        register_node_constructors(lua).unwrap();
        signal::register(lua).unwrap();
        let table: mlua::Table = lua.load(lua_src).eval().unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let shaping = ShapingHandle::spawn();
        let mut scene = Scene::new();
        scene.apply(&[surface], size, &shaping, lua).unwrap();
        scene.surface("bar").unwrap()
    }

    /// `#RRGGBBAA` at logical `(x, y)` from `canvas.screenshot()` -- femtovg's own `Canvas::
    /// screenshot` already does the GL readback and the bottom-up-to-top-down row flip
    /// (`femtovg::renderer::opengl`'s `screenshot` impl), so this harness needs no raw
    /// `glReadPixels`/`glow` context of its own. `scale` here is always `1.0` (matching every
    /// existing call site in this crate), so logical and physical pixel coordinates coincide.
    fn pixel_at(canvas: &mut Canvas<OpenGl>, x: usize, y: usize) -> (u8, u8, u8, u8) {
        let image = canvas.screenshot().expect("screenshot reads back the pbuffer's own framebuffer");
        let px = image[(x, y)];
        (px.r, px.g, px.b, px.a)
    }

    #[test]
    fn a_background_fills_the_surface_with_the_exact_colour() {
        // Catches: `parse_background`/`fill_rect` never wired up at all, or a wrong-cased/wrong-
        // channel colour reaching `Color::rgbaf`. `glReadPixels` returning an exact `#FF0000FF`
        // after a red clear is confirmed working on this machine (Iris and llvmpipe both), so
        // exact equality is the right assertion here, not a tolerance.
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        // `panel`/the child both need an explicit size: an unsized `panel` is Content-sized
        // (docs/adr/0023 item 4's stacking model), which collapses to its *children's* bounding
        // box, not `available` -- and an unsized childless `rect` is `LogicalSize::default()`,
        // zero -- so leaving either implicit would size this whole fixture to 0x0.
        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, child = rect { width = "Fill", height = "Fill", background = "#FF0000FF" } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 32, 32), (255, 0, 0, 255));
    }

    #[test]
    fn a_later_child_paints_over_its_parent_at_the_overlap_and_the_parent_shows_outside_it() {
        // Catches: tree order not respected (docs/adr/0023 item 4) -- if children painted before
        // their parent, or the whole tree painted in the wrong order, the parent's opaque fill
        // would cover the child's smaller opaque fill instead of the reverse.
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, child = rect { width = "Fill", height = "Fill", background = "#0000FFFF", children = {
                rect { background = "#00FF00FF", width = 20, height = 20 },
            } } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &root, 1.0);

        // Inside the child's box (top-left corner of the stacking model, no alignment set):
        // green, the child's own colour, painted over the parent.
        assert_eq!(pixel_at(painter.canvas_mut(), 5, 5), (0, 255, 0, 255));
        // Outside the child's box, still inside the parent: the parent's blue, untouched.
        assert_eq!(pixel_at(painter.canvas_mut(), 40, 40), (0, 0, 255, 255));
    }

    #[test]
    fn a_childs_padded_offset_position_is_honoured_across_two_levels_of_nesting() {
        // Catches exactly the parent-relative-coordinate bug this module's own doc comment warns
        // about, and needs two nesting levels to actually catch it: `blue`'s own absolute position
        // (5, 5) comes entirely from *its* parent's (the surface's) padding, so a `paint_node` that
        // forgot to accumulate an origin at all would still place `blue` correctly at the first
        // level (both a correct walk and a "treat every rect.x/y as absolute" bug start recursing
        // from origin (0, 0)) -- the bug only shows up one level further down.
        //
        // `green`'s parent-relative rect is `(15, 15, 10, 10)` (`blue`'s own padding). Correctly
        // accumulated, its absolute position is `blue`'s origin (5, 5) plus that offset: (20, 20).
        // A buggy walk that painted `green` at its raw (parent-relative) `rect.x`/`rect.y` as if
        // already absolute would place it at (15, 15) instead -- (27, 27) sits inside the correct
        // box (20..30) but outside that wrong one (15..25), so it reads green only if two levels
        // of origin both accumulated correctly.
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, padding = { top = 5, left = 5 }, child = rect {
                width = 50, height = 50, background = "#0000FFFF", padding = { top = 15, left = 15 },
                children = { rect { background = "#00FF00FF", width = 10, height = 10 } },
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &root, 1.0);

        // Inside `blue`'s box, well clear of `green`'s box under either the correct or the buggy
        // placement -- a sanity check that `blue` itself landed at its own correct offset.
        assert_eq!(pixel_at(painter.canvas_mut(), 10, 10), (0, 0, 255, 255));
        // The discriminator: green only here if both levels of origin accumulated.
        assert_eq!(pixel_at(painter.canvas_mut(), 27, 27), (0, 255, 0, 255));
    }

    #[test]
    fn a_per_edge_border_paints_only_the_edge_that_declared_both_a_colour_and_a_width() {
        // Catches: `border_width` alone (no colour) painting anyway (a documented § 5.2 non-
        // error, not a bug to "fix"), or the per-edge fallback path not reading `border_color`/
        // `border_width` per edge at all.
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, child = rect {
                width = 40, height = 40, background = "#000000FF",
                border_width = { top = 4, bottom = 4 },
                border_color = { top = "#FFFFFFFF" },
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &root, 1.0);

        // Top edge: width 4 and colour both declared -- painted white.
        assert_eq!(pixel_at(painter.canvas_mut(), 20, 1), (255, 255, 255, 255));
        // Bottom edge: width 4 declared, no colour -- § 5.2's documented "paints nothing", so this
        // stays the rect's own black background rather than a border colour it never got.
        assert_eq!(pixel_at(painter.canvas_mut(), 20, 38), (0, 0, 0, 255));
    }

    #[test]
    fn text_foreground_colour_puts_non_background_pixels_inside_its_rect() {
        // Catches: `paint_text` not calling `draw_line` at all, or `parse_foreground` not
        // reaching it -- if either were true every pixel in the text's box would stay the
        // surface's own background colour.
        let Some(instance) = init_headless_egl(120, 40) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 120, 40) else { return };

        // `rect` and `panel` both need an explicit size here, same reason as the first test:
        // an unsized `rect` with a `text` child takes the stacking model's bounding-union size
        // (docs/adr/0023 item 4), which would leave everything outside that (possibly small) box
        // unpainted, and the scan below would be reading undefined pbuffer content rather than the
        // rect's own deterministic black background.
        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 120, height = 40, child = rect { width = "Fill", height = "Fill", background = "#000000FF", children = {
                text { content = "Oblisk", font_size = 24, foreground = "#00FF00FF" },
            } } }"##,
            LogicalSize { width: 120.0, height: 40.0 },
        );
        paint_tree(&mut painter, &root, 1.0);

        // Green specifically, not merely "not the background". Asserting non-background alone
        // cannot see this commit's own change: `draw_line` hardcoded `Color::white()` until now,
        // and white is not black either, so a `draw_line` that ignored its new `color` parameter
        // entirely passed that weaker assertion. Measured, by reverting exactly that one line: all
        // five tests here stayed green. The glyph is antialiased against black, so interior pixels
        // run from `#000000` to `#00FF00` and the discriminator is the channel *ratio*, not an
        // exact value: any lit pixel must be green-dominant, and white would fail on `r`.
        let mut lit_pixels = 0usize;
        for y in 0..30usize {
            for x in 0..100usize {
                let (r, g, b, _) = pixel_at(painter.canvas_mut(), x, y);
                if (r, g, b) == (0, 0, 0) {
                    continue;
                }
                lit_pixels += 1;
                assert!(
                    g > r && g > b,
                    "a glyph pixel at ({x}, {y}) is {:?}, which is not the green `foreground` asked for -- white here means `foreground` never reached `draw_line`",
                    (r, g, b)
                );
            }
        }
        assert!(lit_pixels > 0, "text with a foreground colour must paint at least one non-background pixel inside its rect");
    }

    /// The uniform-border-with-radius branch of [`paint_border`] had no test at all: the per-edge
    /// case above takes the fallback path, so the whole `stroke_path` arm, including the half-width
    /// inset its doc comment reasons carefully about, went unexercised. That inset is the part
    /// worth pinning: femtovg strokes centred on the path, so an uninset stroke straddles the
    /// node's own edge with half of it painted outside the box.
    #[test]
    fn a_uniform_border_with_a_radius_strokes_inside_the_nodes_own_box() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        // The surface paints red, the bordered rect black with a white border, so a stroke that
        // leaked outside the rect's box would show up as white on the surface's red.
        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, background = "#FF0000FF", padding = { top = 10, left = 10 }, child = rect {
                width = 40, height = 40, background = "#000000FF",
                radius = 8, border_width = 4, border_color = "#FFFFFFFF",
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &root, 1.0);

        // Mid-edge of the left border, 2px in: inside the 4px stroke, so white.
        assert_eq!(pixel_at(painter.canvas_mut(), 12, 30), (255, 255, 255, 255));
        // Just inside the stroke's inner edge: the rect's own black fill, not border colour.
        assert_eq!(pixel_at(painter.canvas_mut(), 16, 30), (0, 0, 0, 255));
        // One pixel outside the rect's own box: the surface's red. A stroke drawn on the box edge
        // rather than inset by half its width would have painted half of itself out here.
        assert_eq!(pixel_at(painter.canvas_mut(), 9, 30), (255, 0, 0, 255));
    }

    /// The regression test for docs/build-steps.md Phase 19 item 10 / docs/adr/0043 decision 2:
    /// measurement (`ShapingHandle::shape`, cosmic-text) and paint (`TextPainter`, femtovg) must
    /// resolve the same font, or a `text` node's laid-out box and its painted glyphs disagree.
    /// Measured live on the dev machine before this fix: cosmic-text measured under
    /// `Family::SansSerif` while paint's own separate `fontdb` query missed that alias and fell
    /// back to the first face in scan order (Adwaita Mono) -- a `text` node's box came out
    /// roughly 30% narrower than the glyphs drawn into it.
    ///
    /// Both sides now measure/paint the exact same declared chain
    /// (`ShapingHandle::font_chain_bytes`), so this compares cosmic-text's `shape()` against
    /// femtovg's own `measure_text` for the identical string at the identical size and asserts
    /// they land within 2% -- not exact equality, since the two shapers round glyph advances
    /// slightly differently even reading the same font file.
    ///
    /// What this actually covers, stated plainly rather than implied: it catches paint and
    /// measurement loading two *different font sets* -- confirmed real by temporarily having
    /// `TextPainter` load `font_chain_bytes()[1..]` (dropping the chain's first entry) instead
    /// of the full chain, which produced a 61.6% divergence and failed here as expected. It does
    /// *not* catch `shape()` asking for the wrong family while both sides still load the *same*
    /// set: today's default chain resolves to exactly one Latin-covering face (`Noto Sans CJK
    /// JP` misses on every machine this has been run on), so cosmic-text's own per-run fallback
    /// converges on that one face regardless of which family `shape()` names -- confirmed by
    /// temporarily reverting `shape()` to bare `Attrs::new()`, which left this test green.
    /// `text::shaping::tests::shape_measures_under_the_family_it_is_given` is what covers that
    /// second case: it holds one `FontSystem` fixed over a database with two distinct Latin
    /// faces and varies only the family argument `shape()` is given.
    #[test]
    fn femtovg_and_cosmic_text_measure_the_same_string_to_the_same_width() {
        let Some(instance) = init_headless_egl(400, 60) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 400, 60) else { return };

        const TEXT: &str = "Oblisk Shell Renderer";
        const FONT_SIZE: f32 = 24.0;

        let shaped = shaping.shape(ShapeRequest { text: TEXT.into(), font_size: FONT_SIZE, line_height: FONT_SIZE * 1.2, max_width: None });

        let mut paint = Paint::color(Color::black());
        paint.set_font(painter.fonts());
        paint.set_font_size(FONT_SIZE);
        let metrics = painter.canvas_mut().measure_text(0.0, 0.0, TEXT, &paint).expect("measure_text should succeed with the chain fonts loaded");
        let femtovg_width = metrics.width();

        let tolerance = shaped.width.max(femtovg_width) * 0.02;
        let diff = (shaped.width - femtovg_width).abs();
        assert!(
            diff <= tolerance,
            "cosmic-text measured {} but femtovg measured {} for the same string at the same size -- \
             a {:.1}% divergence, over the 2% tolerance, meaning the two are not resolving the same font",
            shaped.width,
            femtovg_width,
            (diff / shaped.width.max(femtovg_width)) * 100.0
        );
    }

    /// The regression test for build-steps.md Phase 19 item 17 itself: a `text` node's content
    /// wider than the box layout gave it must stop at that box's edge, not paint over whatever
    /// sits to its right -- the live MPRIS-title-through-two-cells bug the item's own doc comment
    /// describes.
    ///
    /// Proved this test is real, not just a green test: with `paint_node`'s `save`/
    /// `intersect_scissor`/`restore` temporarily removed, this failed at the first scanned pixel
    /// row with a mix of white (glyph) and black-background pixels found past the box, exactly
    /// the escape this test exists to catch. Restored before finishing.
    #[test]
    fn a_text_wider_than_its_box_paints_nothing_outside_it() {
        let Some(instance) = init_headless_egl(200, 50) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 200, 50) else { return };

        // Surface blue, box black, glyphs white -- three colours distinct from each other, so an
        // escaped glyph pixel (white, or a white/black antialiased edge) cannot be mistaken for
        // the surface's own blue background the scan below asserts on.
        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 200, height = 50, background = "#0000FFFF", child = rect {
                width = 40, height = 50, background = "#000000FF", children = {
                    text { content = "Oblisk Shell Renderer Overflow", font_size = 24, foreground = "#FFFFFFFF" },
                } } }"##,
            LogicalSize { width: 200.0, height: 50.0 },
        );
        paint_tree(&mut painter, &root, 1.0);

        // The text's own content is long enough at this font size to reach well past the box's
        // 40px width unclipped -- a 5px gap (40..45) is left unscanned so this isn't sensitive to
        // the box edge's own antialiasing, only to a genuine escape further out.
        for y in 0..50usize {
            for x in 45..200usize {
                assert_eq!(
                    pixel_at(painter.canvas_mut(), x, y),
                    (0, 0, 255, 255),
                    "pixel ({x}, {y}) outside the text's 40px-wide box is not the surface's plain blue -- \
                     the overflowing glyph escaped its clip"
                );
            }
        }
    }

    /// Regression test for docs/build-steps.md Phase 19 item 7: a 1px border at a fractional
    /// position must land on exactly one physical pixel row, not blur across two.
    /// `padding.top = 10.3` puts the bordered rect's absolute y at a fractional offset --
    /// unsnapped, femtovg's own antialiasing fills part of row 10 and part of row 11 at
    /// partial coverage instead of one row at full coverage.
    ///
    /// Proved this catches the bug it exists for: with `snap_border_band` removed from
    /// `paint_border_edge`'s horizontal branch (using the raw, unsnapped `edge_rect` instead),
    /// row 10 came back `(201, 178, 178, 255)`, a red/white blend rather than white, and the
    /// row-10 assertion failed. Restored before finishing.
    #[test]
    fn a_1px_border_at_a_fractional_position_is_exactly_one_pixel_row() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, background = "#FF0000FF", padding = { top = 10.3 }, child = rect {
                width = 30, height = 20, background = "#000000FF",
                border_width = { top = 1 }, border_color = { top = "#FFFFFFFF" },
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &root, 1.0);

        // Row 9, entirely above the rect (which starts at unsnapped y = 10.3): the surface's
        // own red, untouched by any border bleed.
        assert_eq!(pixel_at(painter.canvas_mut(), 15, 9), (255, 0, 0, 255));
        // Row 10: the snapped border band, fully lit white -- the discriminator this test
        // exists for.
        assert_eq!(pixel_at(painter.canvas_mut(), 15, 10), (255, 255, 255, 255));
        // Row 11, past the 1px band: the rect's own black fill, not a blend.
        assert_eq!(pixel_at(painter.canvas_mut(), 15, 11), (0, 0, 0, 255));
    }

    /// The same snap as the 1px test above, at a width wide enough that "one row either way"
    /// is not the whole story: a 4px band must stay exactly four rows, neither growing to five
    /// nor losing one, which is what pins `snap_border_band` rounding both edges independently
    /// rather than rounding the near edge and adding an unrounded thickness. `padding.top =
    /// 31.3` is deliberately close to the dev config's own fractional geometry --
    /// `notification_area` resolves to a height of 31.6 -- so this is the real shape of the
    /// bug, not a contrived one.
    ///
    /// Note this is the filled-edge branch, not the stroke: `border_width = { top = 4 }` leaves
    /// the other three edges at zero, so `paint_border`'s `uniform_width` test fails and it
    /// takes the per-edge path. Stroke parity, the thing the deleted `snap_border_to_physical`
    /// actually got wrong, is covered by
    /// `a_uniform_stroked_border_at_a_fractional_position_covers_whole_pixel_columns` below.
    ///
    /// Proved this catches the bug: with the same unsnapped `edge_rect` change as the 1px test
    /// above, row 31 came back `(201, 178, 178, 255)` instead of full white -- the loop below
    /// fails on the first row it checks, so this only pins that one value, not all four rows'
    /// worth of blend. Restored before finishing.
    #[test]
    fn a_4px_border_at_a_fractional_position_stays_exactly_four_rows() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, background = "#FF0000FF", padding = { top = 31.3 }, child = rect {
                width = 30, height = 20, background = "#000000FF",
                border_width = { top = 4 }, border_color = { top = "#FFFFFFFF" },
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &root, 1.0);

        // Row 30, above the band: the surface's own red.
        assert_eq!(pixel_at(painter.canvas_mut(), 15, 30), (255, 0, 0, 255));
        // Rows 31 through 34: the snapped 4px border band, every row fully white.
        for y in 31..35usize {
            assert_eq!(pixel_at(painter.canvas_mut(), 15, y), (255, 255, 255, 255), "row {y} is not fully white");
        }
        // Row 35, past the band: the rect's own black fill, not a blend.
        assert_eq!(pixel_at(painter.canvas_mut(), 15, 35), (0, 0, 0, 255));
    }

    /// The uniform-width-with-radius branch of `paint_border` strokes rather than fills, and a
    /// stroke is where physical-pixel parity actually bites: femtovg centres a stroke on its
    /// path, so a 4-wide stroke centred on a half-integer spreads across five rows at partial
    /// coverage while the same stroke centred on an integer covers exactly four. Snapping the
    /// box span and the thickness as bands is what puts the centreline on the right side of that
    /// split without the caller reasoning about parity at all.
    ///
    /// `padding.left = 10.3` is what makes this a real test: with an integer padding the stroke
    /// already lands on whole pixels and passes without any snapping, which is why the existing
    /// `a_uniform_border_with_a_radius_strokes_inside_the_nodes_own_box` test above cannot see
    /// this. Measured with the `snap_border_band` calls in that branch removed: column 10 came
    /// back `(201, 178, 178, 255)`, a red/white blend, instead of full white.
    #[test]
    fn a_uniform_stroked_border_at_a_fractional_position_covers_whole_pixel_columns() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, background = "#FF0000FF", padding = { top = 10, left = 10.3 }, child = rect {
                width = 40, height = 40, background = "#000000FF",
                radius = 8, border_width = 4, border_color = "#FFFFFFFF",
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &root, 1.0);

        // Row 30 is mid-edge, clear of the radius-8 corners on both ends, so the left border
        // there is a straight vertical band four columns wide.
        assert_eq!(pixel_at(painter.canvas_mut(), 9, 30), (255, 0, 0, 255), "column 9 should be the surface's red, outside the border");
        for x in 10..14usize {
            assert_eq!(pixel_at(painter.canvas_mut(), x, 30), (255, 255, 255, 255), "column {x} is not fully white");
        }
        assert_eq!(pixel_at(painter.canvas_mut(), 14, 30), (0, 0, 0, 255), "column 14 should be the rect's own black fill, past the border");
    }

    /// A `row`/`column`/`rect` container clips its children just as much as a `text` node clips
    /// its glyphs -- build-steps.md Phase 19 item 17 calls out that a `row` whose children
    /// overflow is the same defect as an overflowing `text`, not a separate case. This is the
    /// non-text half of that claim: a child rect explicitly larger than its parent must not paint
    /// past the parent's own box.
    ///
    /// Proved this test is real the same way: with the clip removed, the assertion at (50, 50)
    /// failed, reading the child's green instead of the surface's magenta. Restored before
    /// finishing.
    #[test]
    fn an_oversized_child_rect_is_clipped_to_its_parents_box() {
        let Some(instance) = init_headless_egl(80, 80) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 80, 80) else { return };

        // Parent (black, 30x30) sits at (10, 10) via the surface's own padding. Its child (green,
        // 60x60) has no alignment set, so the stacking model places it at the parent's own
        // content-box origin -- same (10, 10) -- and it would span to (70, 70) unclipped, well
        // outside the parent's (10, 10)..(40, 40) box.
        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 80, height = 80, background = "#FF00FFFF", padding = { top = 10, left = 10 }, child = rect {
                width = 30, height = 30, background = "#000000FF", children = {
                    rect { background = "#00FF00FF", width = 60, height = 60 },
                } } }"##,
            LogicalSize { width: 80.0, height: 80.0 },
        );
        paint_tree(&mut painter, &root, 1.0);

        // Inside both the parent's and the child's box: the child's green, painted over the
        // parent (tree order).
        assert_eq!(pixel_at(painter.canvas_mut(), 15, 15), (0, 255, 0, 255));
        // Inside the child's unclipped 60x60 span but outside the parent's 30x30 box: the
        // surface's magenta, not the child's green -- the discriminator this test exists for.
        assert_eq!(pixel_at(painter.canvas_mut(), 50, 50), (255, 0, 255, 255));
    }
}
