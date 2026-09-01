//! Draws a resolved layout tree onto a shared femtovg canvas (build-steps.md Phase 19 item 6).
//!
//! Two halves, and the split is the point. [`build`] walks a `layout::scene::ResolvedNode` tree
//! (`Scene::surface`'s output) and flattens it into a [`DisplayList`] of plain Rust data.
//! [`execute`] turns that list into femtovg draw calls on `text::atlas::TextPainter`'s canvas, the
//! same canvas `draw_line` already draws glyphs on -- one canvas, one flush per surface per frame.
//!
//! Nothing here parses. `node::paint_style` did that while `Scene::apply` resolved the node, so
//! [`build_node`] reads a typed `node::PaintStyle` and this module names no property and holds no
//! `mlua::Value`. It used to run all fourteen paint-property parsers on every node on every frame,
//! because the list comparison below is what makes a frame skippable and the parse was the price of
//! finding out nothing had changed.
//!
//! They were one function until the list existed. Splitting them buys two things a single walk
//! could not: `wayland::App::paint_surface` compares this frame's list against the one it last
//! painted and skips the whole draw plus `eglSwapBuffers` when they match, and [`build`] is
//! testable without standing up an EGL context. Measured on an idle bar with a clock: the
//! 1920x1200 wallpaper went from repainting roughly twice a second to never, and niri's own CPU
//! fell about a third with it, since a full-surface commit made the compositor recomposite the
//! screen behind it.
//!
//! Draws in tree order, parent then children: that is what makes the stacking model docs/adr/0023
//! item 4 already implements resolve overlaps the same way layout resolved them -- a later sibling
//! or a child paints over what an earlier one already put down. An invisible node (`visible ==
//! false`) and its whole subtree draw nothing, the same collapse
//! `layout::scene` picked for row/column space reservation.
//!
//! `ResolvedNode.rect` is parent-relative, so [`build_node`] accumulates an absolute origin as it
//! descends rather than trusting `rect.x`/`rect.y` as already-absolute. Get this wrong and every
//! subtree nests at the surface's top-left corner instead of its real position.

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, Color, Paint, Path};

use crate::image::{self, Fit, ImageCache};
use crate::layout::node::{self, BorderColor, EdgeInsets, PaintStyle, Rgba, TextAlign};
use crate::layout::scene::ResolvedNode;
use crate::text::atlas::TextPainter;
use crate::text::snap::{LogicalRect, PhysicalRect, snap_border_band, snap_to_physical};

/// What one node actually draws, with every paint property already parsed. The variants are the
/// four kind arms that draw anything; a `textfield` or an unrecognised kind contributes no
/// [`DrawCmd`] at all rather than an empty variant here.
///
/// Plain Rust data on purpose, and that is the whole point of this type. The alternative --
/// deriving `PartialEq` on `ResolvedNode` and comparing trees -- cannot work: its properties are a
/// `HashMap<String, mlua::Value>`, and mlua compares tables by identity, so a signal resolving to
/// a table yields a fresh unequal table every pass (the same trap `Signal::set_changed` documents
/// for hover rects). Nothing below holds a Lua value, so equality means what it says.
#[derive(Debug, Clone, PartialEq)]
pub enum Draw {
    /// `rect`/`row`/`column`/`button` and all four surface roles: the fill, then the border.
    Box {
        background: Option<Rgba>,
        radius: f32,
        colors: BorderColor,
        widths: EdgeInsets,
    },
    Text {
        content: String,
        font_size: f32,
        color: Rgba,
        align: TextAlign,
    },
    /// The theme *name*, not the resolved path: [`execute`] does the `image::icons::resolve`
    /// lookup. Keeping the filesystem hit out of [`build`] is what lets the build run on every
    /// re-resolve without touching the icon theme, and the name plus the size is what decides the
    /// pixels either way.
    /// `alpha` rather than a tinted colour, because an icon is blitted rather than filled:
    /// `femtovg`'s `Paint::image` takes the alpha as its last argument, where a `Box` or a `Text`
    /// can carry the same information inside the `Rgba` it already had.
    Icon {
        name: String,
        px: u32,
        alpha: f32,
        /// What a `currentColor` fill in the resolved SVG resolves to (docs/adr/0072). Part of the
        /// command and not just of the draw call because `ImageCache` keys on it: the same file
        /// tinted two ways is two textures.
        color: Option<Rgba>,
    },
    Image {
        source: String,
        fit: Fit,
        px: u32,
        alpha: f32,
    },
}

/// One drawable node: what, where, and the clip it draws under.
///
/// `clip` is the intersection of this node's snapped box with every ancestor's, computed during
/// [`build`] rather than rebuilt from a `save`/`intersect_scissor`/`restore` nest at draw time.
/// The two are equivalent because every clip here is an axis-aligned rect, intersection is
/// associative, and this crate applies no canvas transform (`TextPainter::resize` calls
/// `set_size(w, h, 1.0)` and nothing else touches it).
#[derive(Debug, Clone, PartialEq)]
pub struct DrawCmd {
    pub rect: LogicalRect,
    pub clip: PhysicalRect,
    pub draw: Draw,
}

/// Everything one surface draws, in draw order (parent before child, earlier sibling before
/// later), flattened out of the tree.
///
/// Exists to be compared. `wayland::App::paint_surface` keeps the list it last painted and skips
/// the whole paint plus `eglSwapBuffers` when the new one is equal, which is what stops a
/// 1920x1200 wallpaper being redrawn every second because a clock's seconds digit advanced.
/// Before this, `repaint_mapped_surfaces` repainted every mapped surface on every re-resolve,
/// because ADR-0044 decision 2's single dirty flag left the process no way to tell which surface
/// had changed. This gives it one without touching that flag: the flag still says *something*
/// changed, and the list says *what*.
///
/// Float equality is the right comparison here even though it is usually the wrong one. Both
/// sides come from the same parsers over the same property values, so an unchanged input is
/// bit-identical, not merely close. A `NaN` compares unequal to itself and so repaints forever,
/// which is the safe direction to fail: too many frames, never a stale one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DisplayList {
    pub commands: Vec<DrawCmd>,
}

/// A clip that excludes nothing, which is what the canvas starts with before any scissor is
/// pushed. Intersecting against it is the identity, so [`build`] can treat the root like every
/// other node instead of special-casing it.
const UNCLIPPED: PhysicalRect = PhysicalRect { x0: i32::MIN, y0: i32::MIN, x1: i32::MAX, y1: i32::MAX };

/// Overlap of two snapped boxes. Empty (`x1 <= x0` or `y1 <= y0`) means nothing can draw, which
/// [`build_node`] treats as a whole-subtree skip -- a child's clip is this intersected further,
/// so it can only be empty too.
fn intersect(a: PhysicalRect, b: PhysicalRect) -> PhysicalRect {
    PhysicalRect { x0: a.x0.max(b.x0), y0: a.y0.max(b.y0), x1: a.x1.min(b.x1), y1: a.y1.min(b.y1) }
}

fn is_empty(clip: PhysicalRect) -> bool {
    clip.x1 <= clip.x0 || clip.y1 <= clip.y0
}

/// The focused `secure_submit` field's state, as much of it as paint is allowed to know.
///
/// A count, never the bytes. `shared::SecureBuffer` has one sanctioned read (`expose_secret`,
/// into an outgoing IPC envelope) and this is deliberately not it: ADR-0005 keeps a typed secret
/// out of the Lua VM, and a display list that carried the characters would put it somewhere just
/// as wrong -- a `Draw::Text` this process clones, compares and keeps in `last_painted` until the
/// surface changes.
///
/// `target` rather than a node id because that is what the input layer actually tracks:
/// `wayland::input::FocusedField` names a surface and a `{ capability, action }` pair, and the
/// node that declared that pair is the focused one.
pub struct SecureField<'a> {
    pub target: &'a node::SecureSubmitTarget,
    /// `shared::SecureBuffer::char_count`, so one glyph is drawn per keystroke.
    pub filled: usize,
}

/// Flattens `root` into the list of draws it would produce, touching no canvas and no GL context.
///
/// Pure, so the whole paint stage is testable without EGL for the first time: every existing test
/// in this module below has to stand up a headless pbuffer and read pixels back, and none of them
/// could say "these two trees paint the same" at all.
pub fn build(root: &ResolvedNode, scale: f32, focus: Option<&SecureField>) -> DisplayList {
    let mut commands = Vec::new();
    build_node(root, 0.0, 0.0, scale, UNCLIPPED, 1.0, focus, &mut commands);
    DisplayList { commands }
}

/// One node, then its children (tree order, see this module's doc comment). `origin_x`/`origin_y`
/// is the absolute position of this node's parent's content box -- added to `node.rect.x`/`.y`
/// (parent-relative) to get this node's absolute rect, which is in turn what the next recursion
/// level's origin becomes.
// Eight parameters, and the same answer `layout::scene`'s own walks give: three of them (`origin`,
// `clip`, `inherited_opacity`) are what this recursion accumulates and the rest are invariants it
// carries, so a struct would be a bag holding the same eight fields through the same one caller.
#[allow(clippy::too_many_arguments)]
fn build_node(
    node: &ResolvedNode,
    origin_x: f32,
    origin_y: f32,
    scale: f32,
    clip: PhysicalRect,
    inherited_opacity: f32,
    focus: Option<&SecureField>,
    out: &mut Vec<DrawCmd>,
) {
    if !node.visible {
        return;
    }

    let x = origin_x + node.rect.x;
    let y = origin_y + node.rect.y;
    let rect = LogicalRect { x, y, width: node.rect.width, height: node.rect.height };

    // build-steps.md Phase 19 item 17: a node's own draw and its whole subtree are clipped to
    // this box, snapped the same way `draw_line` snaps its glyph origin so the clip edge and the
    // glyph's physical placement agree. Intersected with the ancestors' clip rather than
    // replacing it, so a child can only shrink the clipped region further, never escape its
    // parent's box.
    //
    // ponytail: clipping is the floor, not the finished behavior (build-steps.md Phase 19 item
    // 17 names this directly). § 3.2 gives `text` a wrap at the available width, and
    // `layout::scene`'s measure callback already measures a `Content`-sized text box against
    // exactly that width (its `text_wrap_width` local, passed to `ShapingHandle::shape` as
    // `max_width`) -- but `ShapeResult` returns only a bounding `width`/`height`, not the wrapped
    // lines that produced it, and `Draw::Text` carries the raw `content` string that `draw_line`
    // renders in one `fill_text` call. So a box sized correctly for N wrapped lines gets one
    // unwrapped line painted into it, and this clip cuts that line off at the first line's width
    // instead of showing the rest on line two. The honest fix is paint asking for the same
    // wrapped line breaks layout already measured with, not a new clip strategy.
    //
    // ponytail: this clip is always rectangular, so a node with `radius > 0.0` clips overflowing
    // children to square corners while its own background underneath is rounded -- an escaping
    // child's corner pixels sit outside the round fill but inside the square clip. femtovg's
    // `intersect_rounded_scissor` exists and `Draw::Box` already carries `radius` for this exact
    // node, but that function's own doc comment (femtovg 0.26.0 src/lib.rs:895-899) only gives
    // exact rounded corners "when this is the first active scissor or... the previous clip is a
    // containing rectangle with the same transform" -- false in general here, since a nested
    // node's clip is intersected against every ancestor's, not the first. Rather than ship
    // rounded corners that are exact for a root node and silently degrade to square for anything
    // nested under one, this keeps the clip rectangular everywhere; switching to
    // `intersect_rounded_scissor` is the upgrade path once a config actually needs it.
    let clip = intersect(clip, snap_to_physical(rect, scale));
    // Nothing in this subtree can put down a pixel, so none of it reaches the list. Behaviourally
    // identical to the `save`/`intersect_scissor`/`restore` walk this replaced, which recursed
    // into fully-clipped children and had every draw discarded by the scissor.
    if is_empty(clip) {
        return;
    }

    // `node.kind` is not consulted at all: `node::paint_style` already made that decision, once,
    // while `Scene::apply` resolved this node. A kind it does not recognise carries no style and
    // draws nothing, which stays deliberate -- on a lock surface that silence is a transparent
    // buffer over a locked session, the black screen docs/adr/0052 decision 3 refuses a lock to
    // avoid, reached by another route.
    // Multiplied down the tree the same way `clip` is intersected down it, and for the same
    // reason: a child can only ever be fainter than its parent, never solid inside a faded panel.
    // Baked into the draw here rather than applied when `execute` runs, because ADR-0063 skips a
    // repaint when the new display list equals the last one -- a fade that lived outside the list
    // would be a change the surface never noticed.
    let opacity = inherited_opacity * node.opacity;
    if let Some(draw) = node.paint.as_ref().and_then(|style| draw_for(style, rect, scale, opacity, focus)) {
        out.push(DrawCmd { rect, clip, draw });
    }

    for child in &node.children {
        build_node(child, x, y, scale, clip, opacity, focus, out);
    }
}

/// Draws `root` and its whole subtree onto `painter`'s canvas, then flushes once. `scale` is the
/// physical/logical pixel ratio `text::snap::snap_to_physical` and `TextPainter::draw_line` take
/// everywhere else in this crate -- every existing call site in `wayland::mod` hardcodes `1.0`
/// today, and this function makes no different assumption.
///
/// `crate::wayland::App::paint_surface` is the production caller, since build-steps.md Phase 20
/// item 4: `socket.rs`'s `RendererClient` keys a `Scene` by the `id` a config writes, and
/// `wayland::App` keys a `wl_surface` the same way since docs/adr/0038 decision 1 deleted the
/// fixed Rust-owned role enum that used to keep the two id spaces from overlapping.
/// Test-only since the skip landed: production paints through [`build`] and [`execute`]
/// separately, because `wayland::App::paint_surface` has to compare the list between the two.
/// Kept because every pixel test below is written against "paint this tree and read the
/// framebuffer", and routing them through the same two calls would say nothing extra.
#[cfg(test)]
pub fn paint_tree(painter: &mut TextPainter, images: &mut ImageCache, root: &ResolvedNode, scale: f32) {
    execute(painter, images, &build(root, scale, None), scale);
}

/// Draws an already-built list. Split from [`build`] so the canvas half holds no geometry and the
/// geometry half holds no canvas -- which is what makes a list comparable, and [`build`] testable
/// without an EGL context.
pub fn execute(painter: &mut TextPainter, images: &mut ImageCache, list: &DisplayList, scale: f32) {
    // Before the draws, never during them: the previous frame's flush has happened, this one has
    // recorded nothing yet, so this is the only point where deleting a texture cannot pull it out
    // from under a queued draw call (see `ImageCache::release_evicted`).
    images.release_evicted(painter.canvas_mut());
    for command in &list.commands {
        // `scissor`, not `intersect_scissor`: the intersection with every ancestor's box is
        // already in `command.clip` (see [`DrawCmd`]), so each draw sets the finished clip
        // outright instead of rebuilding it through a save/restore nest.
        painter.canvas_mut().scissor(
            command.clip.x0 as f32,
            command.clip.y0 as f32,
            (command.clip.x1 - command.clip.x0) as f32,
            (command.clip.y1 - command.clip.y0) as f32,
        );
        let rect = command.rect;
        match &command.draw {
            Draw::Box { background, radius, colors, widths } => {
                // `None` skips the fill entirely; `Some` with alpha 0 still paints a transparent
                // rect -- `parse_background`'s own doc comment calls this distinction deliberate,
                // so both arms matter even though a transparent fill is invisible either way.
                if let Some(color) = background {
                    fill_rect(painter.canvas_mut(), rect, *radius, *color);
                }
                paint_border(painter.canvas_mut(), rect, *radius, *colors, *widths, scale);
            }
            Draw::Text { content, font_size, color, align } => {
                painter.draw_line(content, rect, *font_size, scale, *color, *align)
            }
            Draw::Icon { name, px, alpha, color } => {
                // `u16` is `freedesktop-icons`'s own size type, and a theme has no directory above
                // 512 anyway.
                if let Some(path) = image::icons::resolve(name, (*px).min(512) as u16) {
                    let draw = FileDraw { fit: Fit::Contain, rect, px: *px, alpha: *alpha, tint: *color };
                    draw_file(painter.canvas_mut(), images, &path, draw);
                }
            }
            Draw::Image { source, fit, px, alpha } => {
                let draw = FileDraw { fit: *fit, rect, px: *px, alpha: *alpha, tint: None };
                draw_file(painter.canvas_mut(), images, std::path::Path::new(source), draw)
            }
        }
    }
    painter.canvas_mut().reset_scissor();
    painter.canvas_mut().flush();
}

/// One node's parsed paint properties as the draw they produce, or `None` when they produce none.
///
/// `scale` and `focus` are the whole reason this is here rather than in `node::paint_style`: an
/// `icon` or an `image` needs the physical pixel count its resolved rect works out to, and a
/// `textfield` needs to know whether it holds the keyboard. Both are arithmetic over parsed data.
///
/// Nothing here can fail. A malformed paint property never reaches this function: `Scene::apply`
/// refused the tree that carried it (see `node::paint_style`'s module doc comment), which is what
/// replaced the per-frame log-and-default this function used to be five of.
/// One colour at `opacity`, multiplied into the alpha it already carries.
///
/// Multiplied rather than replaced: a half-transparent colour inside a half-faded panel is a
/// quarter, and a config that wrote both meant both.
fn fade(color: Rgba, opacity: f32) -> Rgba {
    Rgba { a: color.a * opacity, ..color }
}

/// Every edge of a border at `opacity`. `None` stays `None`: an edge with no colour draws nothing,
/// and fading nothing is still nothing.
fn fade_border(colors: BorderColor, opacity: f32) -> BorderColor {
    BorderColor {
        top: colors.top.map(|c| fade(c, opacity)),
        right: colors.right.map(|c| fade(c, opacity)),
        bottom: colors.bottom.map(|c| fade(c, opacity)),
        left: colors.left.map(|c| fade(c, opacity)),
    }
}

fn draw_for(
    style: &PaintStyle,
    rect: LogicalRect,
    scale: f32,
    opacity: f32,
    focus: Option<&SecureField>,
) -> Option<Draw> {
    match style {
        // The shared paint of `rect`/`row`/`column`/`button` and all four surface roles: background
        // fill, then borders (`oblisk-idl-api-specs.md` § 5.2 item 1).
        PaintStyle::Box { background, radius, colors, widths } => Some(Draw::Box {
            background: background.map(|color| fade(color, opacity)),
            radius: *radius,
            colors: fade_border(*colors, opacity),
            widths: *widths,
        }),

        // `text` (§ 5.2 item 4): `content` through `TextPainter`, at `rect`, coloured by
        // `foreground`.
        //
        // ponytail: a `Content`-sized `text` box comes from cosmic-text's measurement
        // (`layout::scene`'s measure callback), while `build_node`'s clip cuts this draw off at
        // that same box -- so if femtovg ever renders wider than cosmic-text measured, the clip
        // shaves the overrun off the right edge instead of letting it paint over a neighbour.
        // Checked against the current font chain (single-face Noto Sans, no fallback triggered)
        // with a scratch test rendering unclipped and scanning pixels past the measured edge, for
        // both a short string at 24px and a 53-character string at 32px: femtovg's `measure_text`
        // agreed with cosmic-text's `shape()` to within 0.0001px on the longer string, and the last
        // lit (non-background) pixel in both cases sat 3-4 physical pixels inside the measured edge,
        // not past it. No shaving observed on this chain. It is still the correct outcome if a
        // future font or fallback face renders wider than it measures: the alternative is the
        // overrun landing on whatever sits to the right, which is the exact bug this fixes.
        // `elide` is absent here on purpose: `Scene::apply` has already rewritten `content` to the
        // string that fits, because that is the only place the box width and the shaping worker are
        // both in reach. By the time a draw is built there is nothing left to decide.
        PaintStyle::Text { content, font_size, color, align, elide: _ } => Some(Draw::Text {
            content: content.clone(),
            font_size: *font_size,
            color: fade(*color, opacity),
            align: *align,
        }),

        // `icon` (§ 5.2 item 5): the theme name, resolved to a file by [`execute`]
        // (build-steps.md Phase 29 item 3, docs/adr/0054).
        //
        // `Contain` rather than `Cover`, and the *shorter* edge as the resolved size: `size` is
        // § 5.2's "bounding box diameter", so an icon in a box that is not square should sit inside
        // it whole rather than be cropped to fill it. An icon is the one case where showing less of
        // the image is never the right answer.
        PaintStyle::Icon { name, color } => Some(Draw::Icon {
            name: name.clone(),
            px: physical_edge(rect.width.min(rect.height), scale),
            alpha: opacity,
            color: *color,
        }),

        // `image` (docs/adr/0054 decision 3): the file at `source`, fitted by `fit`. An empty
        // `source` is the absent-key default, so it draws nothing rather than reaching the cache
        // with a path of "".
        //
        // The *longer* edge, unlike an icon's: `Cover` scales the image up until it covers the box,
        // so rasterizing an SVG wallpaper against the shorter edge would upload it at exactly the
        // resolution the fit is about to scale past.
        PaintStyle::Image { source, fit } => (!source.is_empty()).then(|| Draw::Image {
            source: source.clone(),
            fit: *fit,
            px: physical_edge(rect.width.max(rect.height), scale),
            alpha: opacity,
        }),

        // `textfield` (§ 5.2 item 8): the placeholder while empty, one `mask_character` per typed
        // character once it is not.
        //
        // Until this existed the arm drew nothing, and `lock.lua` carried a comment measuring what
        // that cost: a lock screen that "swallows keystrokes while showing no masked characters at
        // all". That is worse than cosmetic. `pam_unix` answers a wrong password with a two second
        // `pam_fail_delay` and `pam_faillock` locks the account after three, so typing blind means a
        // typo is invisible, indistinguishable from a slow unlock, and three of them lock you out of
        // your own session for ten minutes.
        //
        // Only the focused field fills. An unfocused one shows its placeholder, which is also the
        // honest thing to draw: `input::retarget_secure_submit` zeroizes the buffer whenever focus
        // moves, so there is no typed state left anywhere else to represent.
        PaintStyle::TextField { target, placeholder, mask, font_size, color, align } => {
            let filled = focus
                .filter(|focus| target.as_ref().is_some_and(|declared| declared == focus.target))
                .map_or(0, |focus| focus.filled);
            let content = if filled == 0 { placeholder.clone() } else { mask.repeat(filled) };
            (!content.is_empty()).then_some(Draw::Text {
                content,
                font_size: *font_size,
                color: fade(*color, opacity),
                align: *align,
            })
        }
    }
}

/// The shared half of [`Draw::Icon`] and [`Draw::Image`]: cache lookup, then one `fill_path` over
/// exactly the rect the image occupies.
///
/// The fill path is the *fitted* rect, not the node's box. femtovg clamps to the edge outside a
/// paint's extent unless `REPEAT_X`/`REPEAT_Y` are set, so filling the whole box with a `Contain`
/// paint would smear the image's outermost pixel row across the letterbox. `Cover`'s fitted rect is
/// larger than the box instead, and `paint_node`'s scissor is what crops it.
/// Everything about one file draw except which file: the two `Draw` variants that reach
/// [`draw_file`] carry the same five values and always travel together.
#[derive(Debug, Clone, Copy)]
struct FileDraw {
    fit: Fit,
    rect: LogicalRect,
    px: u32,
    alpha: f32,
    /// `None` for an `image`, which names a file the config chose rather than a themed icon.
    tint: Option<Rgba>,
}

fn draw_file(canvas: &mut Canvas<OpenGl>, images: &mut ImageCache, file: &std::path::Path, draw: FileDraw) {
    let FileDraw { fit, rect, px, alpha, tint } = draw;
    let Some(id) = images.image(canvas, file, px, tint) else {
        return;
    };
    let Ok((width, height)) = canvas.image_size(id) else {
        return;
    };
    let fitted = image::fitted_rect(rect, width as f32, height as f32, fit);
    let mut path = Path::new();
    path.rect(fitted.x, fitted.y, fitted.width, fitted.height);
    canvas.fill_path(&path, &Paint::image(id, fitted.x, fitted.y, fitted.width, fitted.height, 0.0, alpha));
}

/// One logical edge in whole physical pixels, floored at 1. `ImageCache` keys on this, so it has
/// to be an integer rather than the `f32` everything else in this module carries: two boxes half a
/// pixel apart are the same texture, and keying on the float would upload one each.
fn physical_edge(logical: f32, scale: f32) -> u32 {
    let physical = logical * scale;
    if !physical.is_finite() || physical <= 1.0 {
        return 1;
    }
    physical.round() as u32
}

/// How far short of half the box a fill's corner radius stops, in logical pixels.
///
/// femtovg already limits a radius to half the box on each axis
/// (`Path::rounded_rect_varying`'s `rad.min(halfw)`), and at *exactly* that value the four straight
/// segments between the corner arcs have zero length and its fill tessellation collapses to the
/// bounding rectangle. It does not do so consistently, which is what makes an epsilon the fix
/// rather than a special case: sweeping square boxes at radius exactly half, 16, 28 and 32 filled
/// as squares while 20, 24, 36, 40, 44, 48 and 52 filled as circles. Backing off by one ULP is not
/// enough either -- 40x40 rounded and 32x32 did not.
///
/// 0.01 rather than the 0.001 that was also measured to work at every one of those sizes: ten times
/// the smallest shortfall that held, still a hundredth of a logical pixel, which is a fiftieth of a
/// physical one at 2x. Nothing can see it, and the number it is protecting against is zero.
const FILL_RADIUS_EPSILON: f32 = 0.01;

/// The background fill, rounded when the node asked for it.
///
/// The clamp is the whole content of this function and it is not defensive: see
/// [`FILL_RADIUS_EPSILON`] for the degeneracy it steers around.
///
/// A radius at or above half is the pill-and-circle case, not an edge case, which is why this is
/// load-bearing. Half the smaller side is exactly how a config spells a stadium:
/// `components/icon_button.lua` writes `side / 2` for a circle, and `theme.item_radius` lands
/// *above* half because it is scaled independently of `item_height` (18 and 34 unscaled, 17 and 32
/// at 0.93). So every pill and every circle on the dev bar drew a square ground under a correctly
/// rounded border, which is what made it read as the radius reaching only half the draw.
/// [`paint_border`] needs no such clamp: its `stroke_path` has no degeneracy at half, which is
/// precisely why the border kept its corners while the fill lost them, and pinning that asymmetry
/// is what `a_radius_of_half_the_box_fills_a_stadium_not_a_square` is for.
fn fill_rect(canvas: &mut Canvas<OpenGl>, rect: LogicalRect, radius: f32, color: Rgba) {
    let radius = radius.min(rect.width.min(rect.height) / 2.0 - FILL_RADIUS_EPSILON);
    let mut path = Path::new();
    if radius > 0.0 {
        path.rounded_rect(rect.x, rect.y, rect.width, rect.height, radius);
    } else {
        path.rect(rect.x, rect.y, rect.width, rect.height);
    }
    canvas.fill_path(&path, &Paint::color(Color::rgbaf(color.r, color.g, color.b, color.a)));
}

/// femtovg has no per-edge border primitive, so this covers exactly two cases. Uniform borders --
/// all four edges the same width and colour -- with `radius` above 0 get one `stroke_path` over
/// the rounded rect, inset by half the stroke width: femtovg strokes centred on the path, so
/// drawing directly on `rect`'s own edge would have the stroke straddle it, half inside and half
/// outside the box. Everything else -- any edge differing from another, or radius 0 -- fills each
/// edge that declares both a non-zero width and a colour as its own rectangle.
///
/// ponytail: the per-edge-rectangle fallback ignores `radius` entirely, so a config that combines
/// a radius with per-edge widths or colours gets square corners where the rounded background
/// underneath shows through. femtovg exposes no public per-corner arc primitive this could build
/// on without duplicating its internal `rounded_rect_varying` construction; the upgrade path is
/// exactly that -- four independent corner arcs plus four edge segments, mitred at each join --
/// once a real config needs a rounded per-edge border rather than this approximation.
fn paint_border(
    canvas: &mut Canvas<OpenGl>,
    rect: LogicalRect,
    radius: f32,
    colors: BorderColor,
    widths: EdgeInsets,
    scale: f32,
) {
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
        path.rounded_rect(
            box_x + inset,
            box_y + inset,
            (box_width - width).max(0.0),
            (box_height - width).max(0.0),
            radius,
        );
        let mut paint = Paint::color(Color::rgbaf(color.r, color.g, color.b, color.a));
        paint.set_line_width(width);
        canvas.stroke_path(&path, &paint);
        return;
    }

    // Corners overlap here rather than mitre -- each edge is its own filled rect spanning the
    // node's full width or height, so two adjacent non-zero edges both cover the corner they
    // share.
    paint_border_edge(
        canvas,
        colors.top,
        widths.top,
        LogicalRect { x: rect.x, y: rect.y, width: rect.width, height: widths.top },
        EdgeAxis::Horizontal,
        scale,
    );
    paint_border_edge(
        canvas,
        colors.bottom,
        widths.bottom,
        LogicalRect { x: rect.x, y: rect.y + rect.height - widths.bottom, width: rect.width, height: widths.bottom },
        EdgeAxis::Horizontal,
        scale,
    );
    paint_border_edge(
        canvas,
        colors.left,
        widths.left,
        LogicalRect { x: rect.x, y: rect.y, width: widths.left, height: rect.height },
        EdgeAxis::Vertical,
        scale,
    );
    paint_border_edge(
        canvas,
        colors.right,
        widths.right,
        LogicalRect { x: rect.x + rect.width - widths.right, y: rect.y, width: widths.right, height: rect.height },
        EdgeAxis::Vertical,
        scale,
    );
}

/// Which dimension of an edge rect is the thin one -- top/bottom edges span the node's full width
/// and are thin in y, left/right span the full height and are thin in x. `paint_border_edge` needs
/// this to know which axis to hand `snap_border_band`; inferring it from the rect's own
/// width/height would be ambiguous whenever a node's height happens to equal its border width.
enum EdgeAxis {
    Horizontal,
    Vertical,
}

/// One border edge: paints only where both a colour and a non-zero width say so
/// (`node::parse_border_color`'s doc comment -- `border_width` alone is documented § 5.2
/// behaviour, not a bug this should work around). Snaps the edge's thin axis with
/// `snap_border_band` before building the path, so a filled-edge border gets the same
/// whole-physical-pixel treatment as the uniform-radius stroke above -- the long axis is left
/// alone, since only the thin axis can straddle a pixel boundary and blur.
fn paint_border_edge(
    canvas: &mut Canvas<OpenGl>,
    color: Option<Rgba>,
    width: f32,
    edge_rect: LogicalRect,
    axis: EdgeAxis,
    scale: f32,
) {
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::ffi::c_void;

    use khronos_egl as egl;
    use mlua::Lua;

    use crate::layout::instance::SurfaceInstance;
    use crate::layout::scene::{LogicalSize, Scene};
    use crate::lua::nodes::{deserialize_lua_table, register_node_constructors};
    use crate::lua::signal;
    use crate::text::shaping::{ShapeRequest, ShapingHandle};

    const PLATFORM_SURFACELESS_MESA: egl::Enum = 0x31DD;

    /// `None` on any failure, with an `eprintln!` naming which step -- "EGL init failed, skip" is
    /// the gate a driverless CI box takes; this machine has a working Mesa/Iris (and llvmpipe
    /// under `LIBGL_ALWAYS_SOFTWARE=1`) and is expected to actually run every test below.
    ///
    /// Returns just the `Instance`: `Display`/`Surface`/`Context` are bare handle newtypes with no
    /// `Drop` impl, so they need no further Rust-side ownership once `make_current` below has
    /// bound them to this thread; only `instance` is read again, for `get_proc_address` in
    /// [`text_painter`]. Binds a pbuffer surface current before returning.
    fn init_headless_egl(width: i32, height: i32) -> Option<egl::Instance<egl::Static>> {
        let instance = egl::Instance::new(egl::Static);

        // SAFETY: `eglGetPlatformDisplay` with `EGL_PLATFORM_SURFACELESS_MESA` takes no native
        // handle -- `EGL_DEFAULT_DISPLAY` is the null sentinel the extension defines -- and the
        // attribute list is a `EGL_NONE`-terminated slice, which is the contract for this call.
        let display = match unsafe {
            instance.get_platform_display(PLATFORM_SURFACELESS_MESA, egl::DEFAULT_DISPLAY, &[egl::ATTRIB_NONE])
        } {
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
    /// `font_chain_data` source `paint_surface` uses, so this harness draws with the exact
    /// declared font chain cosmic-text shaped against (docs/adr/0043 decision 2).
    fn text_painter(
        instance: &egl::Instance<egl::Static>,
        shaping: &ShapingHandle,
        width: u32,
        height: u32,
    ) -> Option<TextPainter> {
        let font_chain = shaping.font_chain_data();
        TextPainter::new(
            |s| instance.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void),
            width,
            height,
            &font_chain,
        )
        .map_err(|e| eprintln!("EGL init failed, skip: FemtoVG init: {e}"))
        .ok()
    }

    /// Evaluates `lua_src` as one surface's tree, applies it, and returns the resolved root at
    /// `size`. Panics on any layout error -- every fixture below is a config this harness controls,
    /// so a rejection is this test's own bug, not something to assert on.
    fn resolved_surface(lua: &Lua, lua_src: &str, size: LogicalSize) -> ResolvedNode {
        register_node_constructors(lua).unwrap();
        signal::register(lua, signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua.load(lua_src).eval().unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let shaping = ShapingHandle::spawn();
        let mut scene = Scene::new();
        let instances = [SurfaceInstance {
            instance_id: "bar@TEST".to_string(),
            declared_id: "bar".to_string(),
            output: "TEST".to_string(),
            available: size,
        }];
        scene.apply(&[surface], &instances, &shaping, lua).unwrap();
        scene.surface("bar@TEST").unwrap()
    }

    // ---- display list (`build`), the seam that needs no EGL context ----

    fn text_align_of(list: &DisplayList) -> TextAlign {
        list.commands
            .iter()
            .find_map(|cmd| match &cmd.draw {
                Draw::Text { align, .. } => Some(*align),
                _ => None,
            })
            .expect("expected a text draw")
    }

    #[test]
    fn a_text_run_is_left_aligned_in_its_box_unless_it_says_otherwise() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = text { content = "hi", foreground = "#ffffffff" } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        assert_eq!(text_align_of(&build(&tree, 1.0, None)), TextAlign::Start);
    }

    #[test]
    fn a_declared_text_align_reaches_the_display_list() {
        for (declared, expected) in
            [("Center", TextAlign::Center), ("End", TextAlign::End), ("Start", TextAlign::Start)]
        {
            let lua = Lua::new();
            let src = format!(
                r##"return panel {{ id = "bar", width = 200, height = 40,
                    child = text {{ content = "hi", foreground = "#ffffffff", text_align = "{declared}" }} }}"##
            );
            let tree = resolved_surface(&lua, &src, LogicalSize { width: 200.0, height: 40.0 });
            assert_eq!(text_align_of(&build(&tree, 1.0, None)), expected, "text_align = {declared:?}");
        }
    }

    /// A masked field aligns the same way a `text` does, because both produce a `Draw::Text` and a
    /// password prompt that centres its dots is a normal thing to want.
    #[test]
    fn a_textfield_carries_its_own_alignment_too() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = textfield { width = 180, height = 24, placeholder = "password", foreground = "#ffffffff", text_align = "Center" } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        assert_eq!(text_align_of(&build(&tree, 1.0, None)), TextAlign::Center);
    }

    /// The alignment is in the list, so switching it repaints. Same argument as the opacity case:
    /// ADR-0063 skips a repaint when the list compares equal.
    #[test]
    fn changing_only_the_text_alignment_changes_the_display_list() {
        let size = LogicalSize { width: 200.0, height: 40.0 };
        let src = |align: &str| {
            format!(
                r##"return panel {{ id = "bar", width = 200, height = 40,
                    child = text {{ content = "hi", foreground = "#ffffffff", text_align = "{align}" }} }}"##
            )
        };
        let a = build(&resolved_surface(&Lua::new(), &src("Start"), size), 1.0, None);
        let b = build(&resolved_surface(&Lua::new(), &src("Center"), size), 1.0, None);
        assert_ne!(a, b);
    }

    fn box_alpha(cmd: &DrawCmd) -> f32 {
        match &cmd.draw {
            Draw::Box { background: Some(color), .. } => color.a,
            other => panic!("expected a filled box, got {other:?}"),
        }
    }

    /// The whole point of the property: one `opacity` on a container fades everything under it,
    /// rather than each descendant needing its own.
    #[test]
    fn a_parents_opacity_reaches_every_descendant() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40, opacity = 0.5,
            child = rect { width = 100, height = 20, background = "#ffffffff" } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let list = build(&tree, 1.0, None);
        assert_eq!(box_alpha(&list.commands[1]), 0.5, "the child fades with the panel it sits in");
    }

    /// Multiplied down the chain rather than replaced, so nothing inside a faded panel can come
    /// back solid.
    #[test]
    fn a_nested_opacity_multiplies_with_its_ancestors_rather_than_replacing_them() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40, opacity = 0.5,
            child = rect { width = 100, height = 20, opacity = 0.5, background = "#ffffffff" } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let list = build(&tree, 1.0, None);
        assert_eq!(box_alpha(&list.commands[1]), 0.25, "half of a half");
    }

    /// A colour that was already translucent keeps its own alpha as a factor: a config writing
    /// both meant both.
    #[test]
    fn an_opacity_multiplies_the_alpha_a_colour_already_carried() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40, opacity = 0.5,
            background = "#ffffff80" }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let list = build(&tree, 1.0, None);
        let expected = (0x80 as f32 / 255.0) * 0.5;
        assert!((box_alpha(&list.commands[0]) - expected).abs() < 1e-6);
    }

    /// ADR-0063 skips a repaint when the new list equals the last one, so a fade that did not
    /// change the list would be a change the surface never painted.
    #[test]
    fn changing_only_the_opacity_changes_the_display_list() {
        let solid = Lua::new();
        let faded = Lua::new();
        let src = |opacity: &str| {
            format!(
                r##"return panel {{ id = "bar", width = 200, height = 40, opacity = {opacity},
                    child = text {{ content = "12:00", foreground = "#ffffffff" }} }}"##
            )
        };
        let size = LogicalSize { width: 200.0, height: 40.0 };
        let a = build(&resolved_surface(&solid, &src("1.0"), size), 1.0, None);
        let b = build(&resolved_surface(&faded, &src("0.4"), size), 1.0, None);
        assert_ne!(a, b, "the alpha is in the list, not applied on the way to the canvas");
    }

    /// Blitted draws carry the alpha separately, because `Paint::image` takes it as an argument
    /// where a fill can bake it into the colour.
    #[test]
    fn an_icon_carries_the_faded_alpha_rather_than_a_tinted_colour() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40, opacity = 0.25,
            child = icon { name = "network-wireless", size = 16 } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let list = build(&tree, 1.0, None);
        let icon = list.commands.iter().find_map(|cmd| match &cmd.draw {
            Draw::Icon { alpha, .. } => Some(*alpha),
            _ => None,
        });
        assert_eq!(icon, Some(0.25));
    }

    /// Every border edge fades, and an edge with no colour stays absent rather than becoming a
    /// transparent one.
    #[test]
    fn a_border_fades_edge_by_edge_and_an_absent_edge_stays_absent() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40, opacity = 0.5,
            border_color = { top = "#ff0000ff" }, border_width = { top = 2 } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let list = build(&tree, 1.0, None);
        let Draw::Box { colors, .. } = &list.commands[0].draw else {
            panic!("expected a box");
        };
        assert_eq!(colors.top.unwrap().a, 0.5);
        assert!(colors.bottom.is_none(), "an edge the config never coloured is not faded into existence");
    }

    /// `opacity = 0` and `visible = false` are different, deliberately: a transparent node still
    /// lays out and still takes pointer events, which is what lets a fade run without the layout
    /// jumping. So it still produces a draw, at zero alpha.
    #[test]
    fn a_fully_transparent_node_still_draws_rather_than_vanishing_from_the_list() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40, opacity = 0,
            background = "#ffffffff" }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let list = build(&tree, 1.0, None);
        assert_eq!(box_alpha(&list.commands[0]), 0.0);
    }

    /// The property this whole optimisation rests on: same tree in, same list out. If this can
    /// ever fail for an unchanged scene, `paint_surface`'s skip repaints every frame anyway and
    /// the wallpaper is back to redrawing at the clock's cadence.
    #[test]
    fn the_same_tree_builds_an_equal_list_so_an_unchanged_surface_can_be_skipped() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40, background = "#112233ff",
            child = text { content = "12:00:00", foreground = "#ffffffff" } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let list = build(&tree, 1.0, None);
        assert!(!list.commands.is_empty(), "an empty list would make this pass for the wrong reason");
        assert_eq!(list, build(&tree, 1.0, None));
    }

    /// The other half, and the one that would make a skip dangerous if it failed: a changed
    /// string has to change the list, or the surface would keep showing a stale clock forever.
    #[test]
    fn changing_only_a_texts_content_changes_the_list() {
        let lua = Lua::new();
        let panel = |content: &str| {
            format!(
                r##"return panel {{ id = "bar", width = 200, height = 40,
                child = text {{ content = "{content}", foreground = "#ffffffff" }} }}"##
            )
        };
        let size = LogicalSize { width: 200.0, height: 40.0 };
        let before = build(&resolved_surface(&lua, &panel("12:00:00"), size), 1.0, None);
        let after = build(&resolved_surface(&Lua::new(), &panel("12:00:01"), size), 1.0, None);
        assert_ne!(before, after, "a new seconds digit must reach the list, or the paint gets skipped");
    }

    /// `visible = false` collapses the node and everything under it, the same rule the tree walk
    /// this replaced applied -- so an invisible subtree costs nothing to compare, not just
    /// nothing to draw.
    #[test]
    fn an_invisible_node_and_its_children_contribute_nothing() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40, background = "#112233ff",
            child = rect { visible = false, background = "#ff0000ff", width = 50, height = 20,
                   children = { text { content = "hidden", foreground = "#ffffffff" } } } }"##;
        let list = build(&resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 }), 1.0, None);
        assert!(
            !list.commands.iter().any(|c| matches!(&c.draw, Draw::Text { content, .. } if content == "hidden")),
            "an invisible node's child reached the list: {list:?}"
        );
    }

    /// Draw order is tree order, which is what makes docs/adr/0023 item 4's stacking model come
    /// out right: a child is painted after the parent it covers.
    #[test]
    fn a_parents_box_is_listed_before_its_childs() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40, background = "#112233ff",
            child = rect { background = "#445566ff", width = 50, height = 20 } }"##;
        let list = build(&resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 }), 1.0, None);
        let backgrounds: Vec<_> = list
            .commands
            .iter()
            .filter_map(|c| match &c.draw {
                Draw::Box { background: Some(color), .. } => Some(*color),
                _ => None,
            })
            .collect();
        assert_eq!(backgrounds.len(), 2, "both boxes should be listed: {list:?}");
        // #112233 then #445566: the root's own fill is listed first, the child that covers it
        // second, so replaying the list in order reproduces the stacking.
        assert_eq!(
            (backgrounds[0].b * 255.0).round() as u8,
            0x33,
            "the panel root's own background must be listed first, got {backgrounds:?}"
        );
        assert_eq!(
            (backgrounds[1].b * 255.0).round() as u8,
            0x66,
            "the child must be listed after the parent it paints over"
        );
    }

    /// A child's clip is its own box intersected with its parent's, never wider. This is the
    /// invariant that lets [`execute`] call `scissor` outright instead of rebuilding an
    /// `intersect_scissor` nest.
    #[test]
    fn a_childs_clip_never_escapes_its_parents_box() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = rect { background = "#445566ff", width = 40, height = 10,
                children = { rect { background = "#778899ff", width = 500, height = 500 } } } }"##;
        let list = build(&resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 }), 1.0, None);
        let clips: Vec<_> = list.commands.iter().map(|c| c.clip).collect();
        for pair in clips.windows(2) {
            let (outer, inner) = (pair[0], pair[1]);
            assert!(
                inner.x0 >= outer.x0 && inner.y0 >= outer.y0 && inner.x1 <= outer.x1 && inner.y1 <= outer.y1,
                "a descendant clip {inner:?} escaped its ancestor {outer:?}"
            );
        }
    }

    /// A node scrolled or positioned entirely outside its parent draws nothing, so it earns no
    /// entry -- and, more usefully, moving it around off-screen produces no list change and so no
    /// repaint.
    #[test]
    fn a_subtree_clipped_to_nothing_is_left_out_entirely() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = rect { background = "#445566ff", width = 0, height = 0,
                children = { text { content = "offscreen", foreground = "#ffffffff" } } } }"##;
        let list = build(&resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 }), 1.0, None);
        assert!(
            !list.commands.iter().any(|c| matches!(&c.draw, Draw::Text { content, .. } if content == "offscreen")),
            "a zero-area parent clips its child to nothing, so neither belongs in the list: {list:?}"
        );
    }

    // ---- textfield masking ----

    fn lock_target() -> node::SecureSubmitTarget {
        node::SecureSubmitTarget { capability: "lock".to_string(), action: "authenticate".to_string() }
    }

    /// One surface holding the dev config's own password field.
    fn password_surface(lua: &Lua) -> ResolvedNode {
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = textfield { width = "Fill", height = 28, placeholder = "password",
                mask_character = "*",
                secure_submit = { capability = "lock", action = "authenticate" } } }"##;
        resolved_surface(lua, src, LogicalSize { width: 200.0, height: 40.0 })
    }

    fn drawn_text(list: &DisplayList) -> Vec<String> {
        list.commands
            .iter()
            .filter_map(|c| match &c.draw {
                Draw::Text { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_unfocused_password_field_shows_its_placeholder() {
        let lua = Lua::new();
        let list = build(&password_surface(&lua), 1.0, None);
        assert_eq!(drawn_text(&list), vec!["password".to_string()]);
    }

    /// The fix for typing blind: four keystrokes are four glyphs on screen.
    #[test]
    fn a_focused_password_field_draws_one_mask_character_per_typed_character() {
        let lua = Lua::new();
        let target = lock_target();
        let list = build(&password_surface(&lua), 1.0, Some(&SecureField { target: &target, filled: 4 }));
        assert_eq!(drawn_text(&list), vec!["****".to_string()]);
    }

    #[test]
    fn a_focused_but_empty_password_field_still_shows_its_placeholder() {
        let lua = Lua::new();
        let target = lock_target();
        let list = build(&password_surface(&lua), 1.0, Some(&SecureField { target: &target, filled: 0 }));
        assert_eq!(drawn_text(&list), vec!["password".to_string()]);
    }

    /// Focus is a `{ capability, action }` pair, so a field addressed somewhere else must not
    /// fill just because some other field is focused on the same surface. This is the same
    /// routing rule `input::retarget_secure_submit` enforces for the bytes themselves.
    #[test]
    fn a_field_addressed_to_another_capability_does_not_draw_the_focused_fields_characters() {
        let lua = Lua::new();
        let elsewhere = node::SecureSubmitTarget { capability: "network".to_string(), action: "connect".to_string() };
        let list = build(&password_surface(&lua), 1.0, Some(&SecureField { target: &elsewhere, filled: 9 }));
        assert_eq!(
            drawn_text(&list),
            vec!["password".to_string()],
            "a PSK's length must not leak onto the lock screen's field"
        );
    }

    /// The count is all paint ever gets (see [`SecureField`]), so there is no path by which a
    /// typed character reaches the list. Asserted because a display list is cloned, compared and
    /// retained in `last_painted` -- exactly the places ADR-0005 keeps a secret out of.
    #[test]
    fn a_masked_field_draws_only_the_mask_character() {
        let lua = Lua::new();
        let target = lock_target();
        let list = build(&password_surface(&lua), 1.0, Some(&SecureField { target: &target, filled: 6 }));
        let drawn = drawn_text(&list);
        assert_eq!(drawn, vec!["******".to_string()]);
        assert!(drawn[0].chars().all(|c| c == '*'), "nothing but the mask glyph may reach the list");
    }

    /// § 5.2 item 8 makes `mask_character` optional, and a field that omits it should still look
    /// like a password field rather than draw nothing.
    #[test]
    fn a_field_without_a_mask_character_falls_back_to_a_bullet() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = textfield { width = "Fill", height = 28,
                secure_submit = { capability = "lock", action = "authenticate" } } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let target = lock_target();
        let list = build(&tree, 1.0, Some(&SecureField { target: &target, filled: 3 }));
        assert_eq!(drawn_text(&list), vec!["\u{2022}\u{2022}\u{2022}".to_string()]);
    }

    /// The property this feature needs from the display list: typing has to change it, or
    /// `paint_surface` skips the repaint and the dots never appear.
    #[test]
    fn each_typed_character_changes_the_list_so_the_repaint_is_not_skipped() {
        let lua = Lua::new();
        let tree = password_surface(&lua);
        let target = lock_target();
        let three = build(&tree, 1.0, Some(&SecureField { target: &target, filled: 3 }));
        let four = build(&tree, 1.0, Some(&SecureField { target: &target, filled: 4 }));
        assert_ne!(three, four);
    }

    /// `#RRGGBBAA` at logical `(x, y)` from `canvas.screenshot()` -- femtovg's own `screenshot`
    /// already does the GL readback and the row flip, so this harness needs no raw `glReadPixels`.
    /// `scale` here is always `1.0`, so logical and physical pixel coordinates coincide.
    fn pixel_at(canvas: &mut Canvas<OpenGl>, x: usize, y: usize) -> (u8, u8, u8, u8) {
        let image = canvas.screenshot().expect("screenshot reads back the pbuffer's own framebuffer");
        let px = image[(x, y)];
        (px.r, px.g, px.b, px.a)
    }

    /// A second, self-contained EGL harness returning the pieces [`init_headless_egl`] hides, so
    /// one context can be made current against two different draw surfaces. Only
    /// [`one_canvas_draws_correctly_across_two_surfaces_sharing_one_context`] needs this.
    #[allow(clippy::type_complexity)]
    fn init_headless_egl_two_surfaces(
        width: i32,
        height: i32,
    ) -> Option<(egl::Instance<egl::Static>, egl::Display, egl::Context, egl::Surface, egl::Surface)> {
        let instance = egl::Instance::new(egl::Static);

        // SAFETY: `eglGetPlatformDisplay` with `EGL_PLATFORM_SURFACELESS_MESA` takes no native
        // handle -- `EGL_DEFAULT_DISPLAY` is the null sentinel the extension defines -- and the
        // attribute list is a `EGL_NONE`-terminated slice, which is the contract for this call.
        let display = match unsafe {
            instance.get_platform_display(PLATFORM_SURFACELESS_MESA, egl::DEFAULT_DISPLAY, &[egl::ATTRIB_NONE])
        } {
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
        let mut surfaces = Vec::new();
        for _ in 0..2 {
            match instance.create_pbuffer_surface(display, config, &pbuffer_attribs) {
                Ok(s) => surfaces.push(s),
                Err(e) => {
                    eprintln!("EGL init failed, skip: eglCreatePbufferSurface: {e}");
                    return None;
                }
            }
        }
        let context_attribs = [egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE];
        let context = match instance.create_context(display, config, None, &context_attribs) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("EGL init failed, skip: eglCreateContext: {e}");
                return None;
            }
        };
        if let Err(e) = instance.make_current(display, Some(surfaces[0]), Some(surfaces[0]), Some(context)) {
            eprintln!("EGL init failed, skip: eglMakeCurrent: {e}");
            return None;
        }
        Some((instance, display, context, surfaces[0], surfaces[1]))
    }

    #[test]
    fn one_canvas_draws_correctly_across_two_surfaces_sharing_one_context() {
        let Some((instance, display, context, first, second)) = init_headless_egl_two_surfaces(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let red = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, child = rect { width = "Fill", height = "Fill", background = "#FF0000FF" } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        painter.resize(64, 64);
        paint_tree(&mut painter, &mut ImageCache::new(), &red, 1.0);
        assert_eq!(pixel_at(painter.canvas_mut(), 32, 32), (255, 0, 0, 255));

        instance.make_current(display, Some(second), Some(second), Some(context)).expect("switching the draw surface");
        let green = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 32, height = 32, child = rect { width = "Fill", height = "Fill", background = "#00FF00FF" } }"##,
            LogicalSize { width: 32.0, height: 32.0 },
        );
        painter.resize(32, 32);
        paint_tree(&mut painter, &mut ImageCache::new(), &green, 1.0);
        assert_eq!(
            pixel_at(painter.canvas_mut(), 16, 16),
            (0, 255, 0, 255),
            "the shared canvas must still draw correctly after eglMakeCurrent moved it to another surface"
        );

        instance.make_current(display, Some(first), Some(first), Some(context)).expect("switching back");
        painter.resize(64, 64);
        assert_eq!(
            pixel_at(painter.canvas_mut(), 32, 32),
            (255, 0, 0, 255),
            "the first surface's own framebuffer must be untouched by what was drawn into the second"
        );
    }

    #[test]
    fn a_background_fills_the_surface_with_the_exact_colour() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, child = rect { width = "Fill", height = "Fill", background = "#FF0000FF" } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 32, 32), (255, 0, 0, 255));
    }

    #[test]
    fn a_window_or_popup_root_paints_its_own_box_exactly_as_a_panel_root_does() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        for (kind, colour, expected) in
            [("window", "#FF0000FF", (255, 0, 0, 255)), ("popup", "#0000FFFF", (0, 0, 255, 255))]
        {
            let lua = Lua::new();
            let root = resolved_surface(
                &lua,
                &format!(r#"return {kind} {{ id = "bar", width = 64, height = 64, background = "{colour}" }}"#),
                LogicalSize { width: 64.0, height: 64.0 },
            );
            paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
            assert_eq!(
                pixel_at(painter.canvas_mut(), 32, 32),
                expected,
                "a `{kind}` root must paint its own box like a `panel` root"
            );
        }
    }

    #[test]
    fn a_lock_root_paints_its_own_background_over_the_whole_output_it_covers() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        let lua = Lua::new();
        let root = resolved_surface(
            &lua,
            r##"return lock { id = "bar", background = "#00FF00FF" }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
        assert_eq!(pixel_at(painter.canvas_mut(), 32, 32), (0, 255, 0, 255));
        assert_eq!(
            pixel_at(painter.canvas_mut(), 1, 1),
            (0, 255, 0, 255),
            "the fill reaches the corner of the output the surface covers"
        );
    }

    #[test]
    fn a_later_child_paints_over_its_parent_at_the_overlap_and_the_parent_shows_outside_it() {
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
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 5, 5), (0, 255, 0, 255));
        assert_eq!(pixel_at(painter.canvas_mut(), 40, 40), (0, 0, 255, 255));
    }

    #[test]
    fn a_childs_padded_offset_position_is_honoured_across_two_levels_of_nesting() {
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
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 10, 10), (0, 0, 255, 255));
        assert_eq!(pixel_at(painter.canvas_mut(), 27, 27), (0, 255, 0, 255));
    }

    #[test]
    fn a_per_edge_border_paints_only_the_edge_that_declared_both_a_colour_and_a_width() {
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
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 20, 1), (255, 255, 255, 255));
        assert_eq!(pixel_at(painter.canvas_mut(), 20, 38), (0, 0, 0, 255));
    }

    #[test]
    fn text_foreground_colour_puts_non_background_pixels_inside_its_rect() {
        let Some(instance) = init_headless_egl(120, 40) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 120, 40) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 120, height = 40, child = rect { width = "Fill", height = "Fill", background = "#000000FF", children = {
                text { content = "Oblisk", font_size = 24, foreground = "#00FF00FF" },
            } } }"##,
            LogicalSize { width: 120.0, height: 40.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

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
        assert!(
            lit_pixels > 0,
            "text with a foreground colour must paint at least one non-background pixel inside its rect"
        );
    }

    /// The bar's own shape, and the bug it hid. `components/icon_button.lua` asks for
    /// `radius = side / 2` because that is what a circle is, and `theme.item_radius` sits above half
    /// the item height because the two tokens scale independently. Both reached femtovg's own
    /// half-the-box clamp, where its fill tessellation collapses to the bounding rectangle, so every
    /// pill and circle on the bar painted a square ground under a round border.
    ///
    /// 32x32 at radius 16 is the live case, taken off the dev bar's own display list. 40x40 at 20 is
    /// here because it *did* round before the fix and 32x32 did not, which is the measurement that
    /// showed the degeneracy is erratic by size rather than a clean threshold, and so that a future
    /// edit cannot satisfy this test by tightening the epsilon back to one ULP.
    ///
    /// Both halves of each case are asserted, because the asymmetry is the tell: the corner must
    /// show the parent through it (the fill is round) *and* the mid-edge must still be border (the
    /// stroke was always round). A fix that squared the border to match the fill would satisfy one
    /// and not the other.
    #[test]
    fn a_radius_of_half_the_box_fills_a_stadium_not_a_square() {
        let Some(instance) = init_headless_egl(64, 64) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };

        // side, radius. The third is the over-asked radius `theme.item_radius` produces against
        // `theme.item_height`, which must clamp to the same stadium rather than square off.
        for (side, radius) in [(32.0_f32, 16.0_f32), (40.0, 20.0), (32.0, 17.0)] {
            let src = format!(
                r##"return panel {{ id = "bar", width = 64, height = 64, background = "#FF0000FF", padding = {{ top = 4, left = 4 }}, child = rect {{
                    width = {side}, height = {side}, background = "#000000FF", radius = {radius},
                    border_width = 2, border_color = "#FFFFFFFF",
                }} }}"##
            );
            let root = resolved_surface(&lua, &src, LogicalSize { width: 64.0, height: 64.0 });
            paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

            let case = format!("{side}x{side} radius {radius}");
            // Well outside the inscribed circle: the box corner is `side/2 * (sqrt(2) - 1)` clear of
            // it, roughly 6px at 32 and 8px at 40, so this is not an antialiasing read.
            assert_eq!(
                pixel_at(painter.canvas_mut(), 5, 5),
                (255, 0, 0, 255),
                "{case}: the corner of a stadium is outside it, so the panel behind must show through"
            );
            assert_eq!(
                pixel_at(painter.canvas_mut(), 4 + side as usize / 2, 4 + side as usize / 2),
                (0, 0, 0, 255),
                "{case}: and the middle is still filled"
            );
            assert_eq!(
                pixel_at(painter.canvas_mut(), 5, 4 + side as usize / 2),
                (255, 255, 255, 255),
                "{case}: the border was always round here and must stay so"
            );
        }
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

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 64, background = "#FF0000FF", padding = { top = 10, left = 10 }, child = rect {
                width = 40, height = 40, background = "#000000FF",
                radius = 8, border_width = 4, border_color = "#FFFFFFFF",
            } }"##,
            LogicalSize { width: 64.0, height: 64.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 12, 30), (255, 255, 255, 255));
        assert_eq!(pixel_at(painter.canvas_mut(), 16, 30), (0, 0, 0, 255));
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
    /// (`ShapingHandle::font_chain_data`), so this compares cosmic-text's `shape()` against
    /// femtovg's own `measure_text` for the identical string at the identical size and asserts
    /// they land within 2% -- not exact equality, since the two shapers round glyph advances
    /// slightly differently even reading the same font file.
    ///
    /// What this actually covers, stated plainly rather than implied: it catches paint and
    /// measurement loading two *different font sets* -- confirmed real by temporarily having
    /// `TextPainter` load `font_chain_data()[1..]` (dropping the chain's first entry) instead
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

        let shaped = shaping.shape(ShapeRequest {
            text: TEXT.into(),
            font_size: FONT_SIZE,
            line_height: FONT_SIZE * 1.2,
            max_width: None,
        });

        let mut paint = Paint::color(Color::black());
        paint.set_font(painter.fonts());
        paint.set_font_size(FONT_SIZE);
        let metrics = painter
            .canvas_mut()
            .measure_text(0.0, 0.0, TEXT, &paint)
            .expect("measure_text should succeed with the chain fonts loaded");
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

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 200, height = 50, background = "#0000FFFF", child = rect {
                width = 40, height = 50, background = "#000000FF", children = {
                    text { content = "Oblisk Shell Renderer Overflow", font_size = 24, foreground = "#FFFFFFFF" },
                } } }"##,
            LogicalSize { width: 200.0, height: 50.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

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
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 15, 9), (255, 0, 0, 255));
        assert_eq!(pixel_at(painter.canvas_mut(), 15, 10), (255, 255, 255, 255));
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
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 15, 30), (255, 0, 0, 255));
        for y in 31..35usize {
            assert_eq!(pixel_at(painter.canvas_mut(), 15, y), (255, 255, 255, 255), "row {y} is not fully white");
        }
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
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(
            pixel_at(painter.canvas_mut(), 9, 30),
            (255, 0, 0, 255),
            "column 9 should be the surface's red, outside the border"
        );
        for x in 10..14usize {
            assert_eq!(pixel_at(painter.canvas_mut(), x, 30), (255, 255, 255, 255), "column {x} is not fully white");
        }
        assert_eq!(
            pixel_at(painter.canvas_mut(), 14, 30),
            (0, 0, 0, 255),
            "column 14 should be the rect's own black fill, past the border"
        );
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

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 80, height = 80, background = "#FF00FFFF", padding = { top = 10, left = 10 }, child = rect {
                width = 30, height = 30, background = "#000000FF", children = {
                    rect { background = "#00FF00FF", width = 60, height = 60 },
                } } }"##,
            LogicalSize { width: 80.0, height: 80.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        assert_eq!(pixel_at(painter.canvas_mut(), 15, 15), (0, 255, 0, 255));
        assert_eq!(pixel_at(painter.canvas_mut(), 50, 50), (255, 0, 255, 255));
    }
}
