//! Draws a resolved layout tree onto a shared femtovg canvas.
//!
//! Split in two since the list existed: [`build`] walks a `layout::scene::ResolvedNode` tree into
//! a [`DisplayList`] of plain Rust data, and [`execute`] turns that list into femtovg draw calls on
//! `text::atlas::TextPainter`'s canvas (the one `draw_text` draws glyphs on). This lets
//! `wayland::App::paint_surface` skip the draw plus `eglSwapBuffers` when a frame's list equals the
//! last, and lets [`build`] be tested without an EGL context. Measured on an idle bar with a clock:
//! a 1920x1200 wallpaper went from repainting twice a second to never, niri's CPU fell about a
//! third, since a full-surface commit recomposites the whole screen behind it.
//!
//! Nothing here parses: `node::paint_style` did that while `Scene::apply` resolved the node, so
//! [`build_node`] reads a typed `node::PaintStyle` and holds no `mlua::Value`. Draws in tree order
//! (parent then children), matching the stacking model ADR-0023 describes. An invisible node
//! (`visible == false`) and its subtree draw nothing, the same collapse `layout::scene` picked for
//! row/column space reservation.
//!
//! `ResolvedNode.rect` is parent-relative, so [`build_node`] accumulates an absolute origin as it
//! descends instead of trusting `rect.x`/`rect.y` as already-absolute.

use std::f32::consts::{FRAC_PI_2, PI};

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, Color, ImageFlags, ImageId, Paint, Path, PixelFormat, RenderTarget, Solidity};

use crate::image::{self, Fit, ImageCache};
use crate::layout::node::{self, BorderColor, ClipShape, EdgeInsets, PaintStyle, Rgba, TextAlign};
use crate::layout::scene::ResolvedNode;
use crate::text::atlas::TextPainter;
use crate::text::snap::{LogicalRect, PhysicalRect, snap_border_band, snap_to_physical};

/// What one node actually draws, with every paint property already parsed. The four variants are
/// the kind arms that draw anything; a `textfield` or unrecognised kind contributes no [`DrawCmd`].
///
/// Plain Rust data on purpose: `ResolvedNode` properties are a `HashMap<String, mlua::Value>`, and
/// mlua compares tables by identity, so deriving `PartialEq` there would give a signal's resolved
/// table a fresh unequal value every pass (the trap `Signal::set_changed` documents for hover
/// rects). Nothing here holds a Lua value, so equality means what it says.
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
    /// The theme *name*, not the resolved path: [`execute`] resolves it via
    /// `image::icons::resolve`, keeping the filesystem hit out of [`build`]. `alpha`, not a tinted
    /// colour: an icon is blitted, and `femtovg`'s `Paint::image` takes alpha as its last argument,
    /// where a `Box` or `Text` bakes it into `Rgba`.
    Icon {
        name: String,
        px: u32,
        alpha: f32,
        /// What a `currentColor` fill in the resolved SVG resolves to (ADR-0072). `ImageCache` keys
        /// on it, so the same file tinted two ways is two textures.
        color: Option<Rgba>,
    },
    Image {
        source: String,
        fit: Fit,
        px: u32,
        alpha: f32,
    },
    /// A whole subtree drawn through the arc of the node that declared `clip = "Rounded"`, rather
    /// than its bounding rectangle. `radius` is that node's own; the carrying [`DrawCmd`]'s `rect`
    /// is the box the arc is built on. The one recursive variant: every other clip here is
    /// axis-aligned and flattened into one `clip` per [`DrawCmd`], but a rounded shape does not
    /// intersect into a rectangle, so its subtree stays grouped for [`execute`] to mask as a whole.
    /// `Vec<DrawCmd>`, not `DisplayList`: that is one surface's finished output.
    Clipped {
        radius: f32,
        commands: Vec<DrawCmd>,
    },
}

/// One drawable node: what, where, and the clip it draws under.
///
/// `clip` is the intersection of this node's snapped box with every ancestor's, computed once
/// during [`build`] rather than rebuilt from a save/restore scissor nest at draw time. The two are
/// equivalent: every clip here is axis-aligned, intersection is associative, and this crate applies
/// no canvas transform (`TextPainter::resize` only calls `set_size(w, h, 1.0)`).
#[derive(Debug, Clone, PartialEq)]
pub struct DrawCmd {
    pub rect: LogicalRect,
    pub clip: PhysicalRect,
    pub draw: Draw,
}

/// Everything one surface draws, in draw order (parent before child, earlier sibling before
/// later), flattened out of the tree. Exists to be compared, the module doc's skipped-repaint
/// trick. Before it, `repaint_mapped_surfaces` repainted every mapped surface on every re-resolve,
/// since ADR-0044 decision 2's single dirty flag could not say which surface changed; this list
/// says *what*.
///
/// Float equality, usually wrong, is right here: both sides come from the same parsers over the
/// same property values, so an unchanged input is bit-identical, not merely close. `NaN` compares
/// unequal to itself and so repaints forever, the safe failure: too many frames, never a stale one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DisplayList {
    pub commands: Vec<DrawCmd>,
}

/// A clip that excludes nothing, which is what the canvas starts with before any scissor is
/// pushed. Intersecting against it is the identity, so [`build`] can treat the root like every
/// other node instead of special-casing it.
const UNCLIPPED: PhysicalRect = PhysicalRect { x0: i32::MIN, y0: i32::MIN, x1: i32::MAX, y1: i32::MAX };

/// Overlap of two snapped boxes. Empty (`x1 <= x0` or `y1 <= y0`) means nothing can draw, which
/// [`build_node`] treats as a whole-subtree skip: a child's clip is this intersected further, so it
/// can only be empty too.
fn intersect(a: PhysicalRect, b: PhysicalRect) -> PhysicalRect {
    PhysicalRect { x0: a.x0.max(b.x0), y0: a.y0.max(b.y0), x1: a.x1.min(b.x1), y1: a.y1.min(b.y1) }
}

fn is_empty(clip: PhysicalRect) -> bool {
    clip.x1 <= clip.x0 || clip.y1 <= clip.y0
}

/// The focused `secure_submit` field's state, as much of it as paint is allowed to know.
///
/// A count, never the bytes. `shared::SecureBuffer` has one sanctioned read (`expose_secret`, into
/// an outgoing IPC envelope), deliberately not this one: ADR-0005 keeps a typed secret out of the
/// Lua VM, and a `Draw::Text` this process clones and retains in `last_painted` is just as wrong.
///
/// `target`, not a node id: `wayland::input::FocusedField` names a surface and a
/// `{ capability, action }` pair, and the node that declared that pair is the focused one.
pub struct SecureField<'a> {
    pub target: &'a node::SecureSubmitTarget,
    /// `shared::SecureBuffer::char_count`, so one glyph is drawn per keystroke.
    pub filled: usize,
}

/// Flattens `root` into the list of draws it would produce, touching no canvas and no GL context.
///
/// Pure, so the whole paint stage is testable without EGL: every test below stands up a headless
/// pbuffer and reads pixels back, and none of them can say "these two trees paint the same".
pub fn build(root: &ResolvedNode, scale: f32, focus: Option<&SecureField>) -> DisplayList {
    let mut commands = Vec::new();
    build_node(root, 0.0, 0.0, scale, UNCLIPPED, 1.0, focus, &mut commands);
    DisplayList { commands }
}

/// One node, then its children (tree order, see this module's doc comment). `origin_x`/`origin_y`
/// is the absolute position of this node's parent's content box, added to `node.rect.x`/`.y`
/// (parent-relative) to get this node's absolute rect, the next recursion level's origin.
// Eight parameters, the same answer `layout::scene`'s own walks give: three (`origin`, `clip`,
// `inherited_opacity`) are what this recursion accumulates, the rest are invariants it carries, so
// a struct would just bag the same eight fields through the same one caller.
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

    // Clipped to this box, snapped like `draw_text` snaps its glyph origin, and intersected with
    // the ancestors' clip rather than replacing it, so a child can only shrink the region.
    //
    // A `text` that asked to wrap arrives here already broken into lines by `layout::scene`'s
    // `fit_text_to_box`, so this clip is a backstop rather than the thing deciding what shows. One
    // that did not is a single run, and the clip cuts it at the box edge as it always has.
    //
    // Always rectangular, `radius` or not: a node that wants its children cut by its arc says
    // `clip = "Rounded"` and gets a `Draw::Clipped` group below. femtovg's
    // `intersect_rounded_scissor` is not the alternative it looks like ([`draw_clipped`] measures
    // what it does to a pill with a part-width child).
    //
    // ponytail: `layout::hit` intersects the same rectangles but knows nothing about the arc, so a
    // pill's corner is outside its fill yet still takes a click (four pixels on a 34px control).
    // Upgrade path: hit testing should share this walk instead of a second copy of the rule.
    let clip = intersect(clip, snap_to_physical(rect, scale));
    // Same as the save/`intersect_scissor`/restore walk this replaced, which recursed into
    // fully-clipped children and had every draw discarded by the scissor.
    if is_empty(clip) {
        return;
    }

    // `node.kind` is not consulted: `node::paint_style` decided while `Scene::apply` resolved this
    // node. An unrecognised kind draws nothing, deliberately: on a lock surface that silence is a
    // transparent buffer over a locked session, the black screen ADR-0052 decision 3 refuses a lock
    // to avoid, reached another way. Opacity multiplies down the tree the same way `clip`
    // intersects down it, baked in here rather than in `execute` since ADR-0063 skips a repaint on
    // an unchanged list, and a fade outside the list would go unnoticed.
    let opacity = inherited_opacity * node.opacity;
    let draw = node.paint.as_ref().and_then(|style| draw_for(style, rect, scale, opacity, focus));

    let Some(radius) = rounded_clip(node) else {
        if let Some(draw) = draw {
            out.push(DrawCmd { rect, clip, draw });
        }
        for child in &node.children {
            build_node(child, x, y, scale, clip, opacity, focus, out);
        }
        return;
    };

    // `clip = "Rounded"`: fill, then the subtree masked by the arc, then the border on top, QML's
    // own order. Painting the border first would have it covered wherever a child reaches the arc,
    // exactly where a pill's filled ground reaches its left cap.
    let (fill, border) = split_fill_and_border(draw);
    if let Some(fill) = fill {
        out.push(DrawCmd { rect, clip, draw: fill });
    }

    let mut inner = Vec::new();
    for child in &node.children {
        build_node(child, x, y, scale, clip, opacity, focus, &mut inner);
    }
    // No children, no offscreen pass. A leaf that asked for a rounded clip has nothing to clip, and
    // the empty group would still cost `execute` a render target and a composite.
    if !inner.is_empty() {
        out.push(DrawCmd { rect, clip, draw: Draw::Clipped { radius, commands: inner } });
    }

    if let Some(border) = border {
        out.push(DrawCmd { rect, clip, draw: border });
    }
}

/// This node's radius when it asked for its children to be cut by it, `None` otherwise. A zero
/// radius is `None` too: the rounded shape of a square-cornered box *is* its rectangle, which the
/// flattened `clip` already gives for free.
fn rounded_clip(node: &ResolvedNode) -> Option<f32> {
    match node.paint {
        Some(PaintStyle::Box { clip: ClipShape::Rounded, radius, .. }) if radius > 0.0 => Some(radius),
        _ => None,
    }
}

/// One `Draw::Box` as the fill alone and the border alone, so [`build_node`] can put a
/// [`Draw::Clipped`] between them. Either half is `None` when it would paint nothing.
///
/// Anything that is not a `Draw::Box` comes back whole in the first slot: [`rounded_clip`] answers
/// only for `PaintStyle::Box`, so that arm is unreachable, and returning it costs less than a
/// panic.
fn split_fill_and_border(draw: Option<Draw>) -> (Option<Draw>, Option<Draw>) {
    let Some(Draw::Box { background, radius, colors, widths }) = draw else {
        return (draw, None);
    };
    let fill = background.map(|color| Draw::Box {
        background: Some(color),
        radius,
        colors: BorderColor::default(),
        widths: EdgeInsets::default(),
    });
    let border = (widths != EdgeInsets::default()).then_some(Draw::Box { background: None, radius, colors, widths });
    (fill, border)
}

/// Draws `root` and its whole subtree onto `painter`'s canvas, then flushes once. `scale` is the
/// physical/logical pixel ratio `text::snap::snap_to_physical` and `TextPainter::draw_text` take
/// everywhere else in this crate: every call site in `wayland::mod` hardcodes `1.0` today.
///
/// `crate::wayland::App::paint_surface` is the production caller: `socket.rs`'s `RendererClient`
/// keys a `Scene` by the `id` a config writes, and `wayland::App` keys a `wl_surface` the same way,
/// since ADR-0038 decision 1 deleted the fixed Rust-owned role enum that kept the two id spaces
/// from overlapping.
///
/// Test-only: that caller paints through [`build`] and [`execute`] separately to compare the list
/// between the two, and every pixel test below is written as "paint this tree and read the
/// framebuffer".
#[cfg(test)]
pub fn paint_tree(painter: &mut TextPainter, images: &mut ImageCache, root: &ResolvedNode, scale: f32) {
    execute(painter, images, &build(root, scale, None), scale);
}

/// Draws an already-built list. Split from [`build`] so the canvas half holds no geometry and the
/// geometry half holds no canvas: what makes a list comparable, and [`build`] testable without an
/// EGL context.
pub fn execute(painter: &mut TextPainter, images: &mut ImageCache, list: &DisplayList, scale: f32) {
    // Before the draws, never during: the previous flush has happened and this one has recorded
    // nothing yet, the only point deleting a texture cannot pull it from under a queued draw call
    // (`ImageCache::release_evicted`).
    images.release_evicted(painter.canvas_mut());
    let mut scratch = Vec::new();
    run(painter, images, &list.commands, scale, RenderTarget::Screen, &mut scratch);
    painter.canvas_mut().reset_scissor();
    painter.canvas_mut().flush();
    // After the flush, never before: femtovg executes queued draw calls there, so an image deleted
    // earlier is pulled from under one. `release_evicted` follows this rule from the other end, the
    // same femtovg's own `release_shadow_images` follows for its drop shadow's offscreen images.
    for id in scratch {
        painter.canvas_mut().delete_image(id);
    }
}

/// One run of commands against one render target. Recursive because [`Draw::Clipped`] is.
///
/// `target` is what this run draws into, so a nested [`Draw::Clipped`] can restore it rather than
/// assume the screen: femtovg keeps the current target private, and restoring the wrong one sends
/// a doubly-nested subtree to the framebuffer instead of its parent's image. `scratch` collects
/// offscreen images for [`execute`] to free once it has flushed.
fn run(
    painter: &mut TextPainter,
    images: &mut ImageCache,
    commands: &[DrawCmd],
    scale: f32,
    target: RenderTarget,
    scratch: &mut Vec<ImageId>,
) {
    for command in commands {
        // `scissor`, not `intersect_scissor`: the intersection with every ancestor's box is already
        // in `command.clip` (see [`DrawCmd`]), so each draw sets the finished clip outright instead
        // of rebuilding it through a save/restore nest.
        let clip = command.clip;
        painter.canvas_mut().scissor(
            clip.x0 as f32,
            clip.y0 as f32,
            (clip.x1 - clip.x0) as f32,
            (clip.y1 - clip.y0) as f32,
        );
        let rect = command.rect;
        match &command.draw {
            Draw::Box { background, radius, colors, widths } => {
                // `None` skips the fill entirely; `Some` with alpha 0 still paints a transparent
                // rect, a distinction `parse_background`'s own doc comment calls deliberate.
                if let Some(color) = background {
                    fill_rect(painter.canvas_mut(), rect, *radius, *color);
                }
                paint_border(painter.canvas_mut(), rect, *radius, *colors, *widths, scale);
            }
            Draw::Text { content, font_size, color, align } => {
                painter.draw_text(content, rect, *font_size, scale, *color, *align)
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
            Draw::Clipped { radius, commands } => {
                draw_clipped(painter, images, rect, clip, *radius, commands, scale, target, scratch)
            }
        }
    }
}

/// Draws `commands` into an offscreen image the size of `clip`, then fills the node's own rounded
/// path with that image: the mask is the path, so its antialiased edge *is* the clip's edge.
///
/// **Why not femtovg's `intersect_rounded_scissor`.** femtovg 0.26 carries one scissor, a single
/// rounded rectangle, so "this rect and that arc" has nowhere to live: given a rounded pill and a
/// child covering its left 30px, the intersection re-rounds the child's own 30px box, turning a
/// fill that should be flat on its right edge into a lozenge. Measured on an 80x32 pill at radius
/// 16 with a 30px child: the scissor route leaks the ground through at 8% where the pill's top edge
/// is straight, this route does not; `dev-config`'s battery indicator worked around the same
/// lozenge by giving its fill the pill's radius, so the scissor would only move the bug.
///
/// QML's answer too, shape for shape: Quickshell's `ClippingRectangle` renders to a
/// `ShaderEffectSource` and composites through a mask texture, spending two offscreen targets since
/// the mask must be a texture for its fragment shader to sample. femtovg fills a path with an image
/// paint directly, so the path is the mask and one target does it.
///
/// ponytail: one image allocated and freed per clipping node per repaint. Upgrade path: a pool
/// keyed by size next to `ImageCache`, once a config repaints a rounded clip at pointer rate.
// Nine parameters, the same answer [`build_node`] gives for its eight: four are one command taken
// apart, the rest are what [`run`] carries. Passing the `DrawCmd` whole would trade them for a
// re-match on a variant the caller already matched.
#[allow(clippy::too_many_arguments)]
fn draw_clipped(
    painter: &mut TextPainter,
    images: &mut ImageCache,
    rect: LogicalRect,
    clip: PhysicalRect,
    radius: f32,
    commands: &[DrawCmd],
    scale: f32,
    target: RenderTarget,
    scratch: &mut Vec<ImageId>,
) {
    let (width, height) = ((clip.x1 - clip.x0) as usize, (clip.y1 - clip.y0) as usize);
    // `PREMULTIPLIED`: an offscreen target stores premultiplied results, and without it the
    // composite premultiplies twice, darkening every partially transparent texel. `FLIP_Y`: a GL
    // framebuffer object puts canvas y = 0 on the *last* texture row. Both flags are femtovg's own,
    // from its drop-shadow pass (femtovg 0.26.0 `src/lib.rs`, `PREMULTIPLIED | FLIP_Y`).
    let flags = ImageFlags::PREMULTIPLIED | ImageFlags::FLIP_Y;
    let Ok(image) = painter.canvas_mut().create_image_empty(width, height, PixelFormat::Rgba8, flags) else {
        // Out of texture memory: draw the subtree unmasked rather than dropping it. A
        // square-cornered child is what every node did before this existed; an empty pill is worse.
        run(painter, images, commands, scale, target, scratch);
        return;
    };
    scratch.push(image);

    let canvas = painter.canvas_mut();
    canvas.save();
    canvas.set_render_target(RenderTarget::Image(image));
    canvas.clear_rect(0, 0, width as u32, height as u32, Color::rgbaf(0.0, 0.0, 0.0, 0.0));
    // Set, not accumulated via `translate`: a nested clipping node would otherwise compose both
    // offsets and land its subtree at their sum. Every `scissor` the inner run sets is transformed
    // by this too, so the commands' absolute coordinates map into the image with no extra math.
    canvas.reset_transform();
    canvas.translate(-clip.x0 as f32, -clip.y0 as f32);
    run(painter, images, commands, scale, RenderTarget::Image(image), scratch);

    let canvas = painter.canvas_mut();
    canvas.restore();
    canvas.set_render_target(target);
    let path = box_path(rect, radius);
    let paint = Paint::image(image, clip.x0 as f32, clip.y0 as f32, width as f32, height as f32, 0.0, 1.0);
    canvas.fill_path(&path, &paint);
}

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

/// One node's parsed paint properties as the draw they produce, or `None` when they produce none.
///
/// `scale` and `focus` are why this lives here rather than in `node::paint_style`: an `icon`/
/// `image` needs the physical pixel count its resolved rect works out to, and a `textfield` needs
/// to know whether it holds the keyboard. Nothing here can fail: a malformed paint property never
/// reaches this function, since `Scene::apply` refused the tree that carried it (see
/// `node::paint_style`'s module doc comment).
fn draw_for(
    style: &PaintStyle,
    rect: LogicalRect,
    scale: f32,
    opacity: f32,
    focus: Option<&SecureField>,
) -> Option<Draw> {
    match style {
        // The shared paint of `rect`/`row`/`column`/`button` and all four surface roles: background
        // fill, then borders (`oblisk-idl-api-specs.md` § 5.2 item 1). `clip` is not read here: it
        // decides what this node's *children* are cut to, `build_node`'s question, not this one's.
        PaintStyle::Box { background, radius, colors, widths, clip: _ } => Some(Draw::Box {
            background: background.map(|color| fade(color, opacity)),
            radius: *radius,
            colors: fade_border(*colors, opacity),
            widths: *widths,
        }),

        // `text` (§ 5.2 item 4): `content` through `TextPainter`, at `rect`, coloured by
        // `foreground`. `elide`, `wrap` and `max_lines` are absent on purpose: `Scene::apply`
        // already rewrote `content` to the string that fits -- ellipsized, or line-broken with
        // `\n` -- in the only place the box width and the shaping worker are both in reach.
        //
        // ponytail: a `Content`-sized `text` box comes from cosmic-text's measurement
        // (`layout::scene`'s measure callback), so if femtovg ever renders wider than cosmic-text
        // measured, this draw's clip shaves the overrun off the right edge. Verified on the current
        // chain (single-face Noto Sans): `measure_text` agreed with cosmic-text's `shape()` to
        // within 0.0001px on a 53-character, 32px string, last lit pixel 3-4 physical pixels inside
        // the measured edge. No shaving observed today, but the clip is the safe direction.
        PaintStyle::Text { content, font_size, color, align, elide: _, wrap: _, max_lines: _ } => Some(Draw::Text {
            content: content.clone(),
            font_size: *font_size,
            color: fade(*color, opacity),
            align: *align,
        }),

        // `icon` (§ 5.2 item 5): the theme name, resolved to a file by [`execute`] (ADR-0054).
        // `Contain`, not `Cover`, with the *shorter* edge as the resolved size: `size` is § 5.2's
        // "bounding box diameter", so an icon in a non-square box sits inside it whole rather than
        // cropped to fill it.
        PaintStyle::Icon { name, color } => Some(Draw::Icon {
            name: name.clone(),
            px: physical_edge(rect.width.min(rect.height), scale),
            alpha: opacity,
            color: *color,
        }),

        // `image` (ADR-0054 decision 3): the file at `source`, fitted by `fit`. An empty `source`
        // is the absent-key default, so it draws nothing rather than reaching the cache with a path
        // of "". The *longer* edge, unlike an icon's: `Cover` scales the image up until it covers
        // the box, so rasterizing against the shorter edge would upload below the resolution `fit`
        // is about to scale past.
        PaintStyle::Image { source, fit } => (!source.is_empty()).then(|| Draw::Image {
            source: source.clone(),
            fit: *fit,
            px: physical_edge(rect.width.max(rect.height), scale),
            alpha: opacity,
        }),

        // `textfield` (§ 5.2 item 8): the placeholder while empty, one `mask_character` per typed
        // character once it is not. Typing blind is worse than cosmetic: `pam_unix` answers a wrong
        // password with a two second `pam_fail_delay`, and `pam_faillock` locks the account after
        // three, so a typo is indistinguishable from a slow unlock and costs a ten-minute lockout.
        //
        // Only the focused field fills. An unfocused one shows its placeholder:
        // `input::retarget_secure_submit` zeroizes the buffer whenever focus moves, so no typed
        // state survives anywhere else to represent.
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

/// The shared half of [`Draw::Icon`] and [`Draw::Image`]: cache lookup, then one `fill_path` over
/// the *fitted* rect, not the node's box: femtovg clamps to the edge outside a paint's extent
/// unless `REPEAT_X`/`REPEAT_Y` are set, so filling the whole box with a `Contain` paint would
/// smear the image's outermost pixel row across the letterbox. `Cover`'s fitted rect is larger than
/// the box, and `run`'s scissor crops it.
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

/// The path a box with `radius` asks for: a rectangle, a rounded rectangle, or a stadium.
///
/// The third case is why this is a function, not one `rounded_rect` call. femtovg limits a radius
/// to half the box on each axis (`Path::rounded_rect_varying`'s `rad.min(halfw)`), and around that
/// value a rounded rect fails two ways in two adjacent bands. At exactly half, the four straight
/// segments between the corner arcs go to zero length and the fill collapses to the bounding
/// rectangle. Just short of half, the segments return but the tessellator flags a bevel join at
/// each one, and the half-pixel inset it applies to the fill fan (`path::cache`'s `woff`, and the
/// `TODO: woff = 0.0 produces no artifaacts` beside it) folds the fan back on itself there: an
/// opaque fill hides the fold, a translucent one blends every folded sliver twice, a one-pixel
/// chord at 1.6x alpha on the dev bar's 42%-alpha controls.
///
/// Filling square boxes from 24 to 43.5 logical pixels at two sub-pixel offsets, 80 geometries per
/// row:
///
/// | radius below half | filled square | interior seams |
/// | ----------------- | ------------- | -------------- |
/// | 0 (exactly half)  | 80            | 0              |
/// | 0.0001 to 0.01 px | 0             | 29             |
/// | 0.05 px and more  | 0             | 0              |
///
/// A shortfall clears both bands, and a shipped `FILL_RADIUS_EPSILON` of 0.01 sat in the second
/// one. Qt's own clamp, `qMin(w, h) * 0.4999f` (`qsgbasicinternalrectanglenode.cpp`), lands in the
/// bad band at every size here, and backing off further is still a constant tuned against one
/// tessellator, so this builds the shape instead.
///
/// Half the smaller side is how a config spells a pill, not an edge case:
/// `components/icon_button.lua` writes `side / 2` for a circle, and `theme.item_radius` lands
/// above half since it scales independently of `item_height`. So a square box is built as a circle
/// (femtovg's own `circle`, four beziers, no straight segments), anything else as two semicircular
/// caps joined by two segments of length `|width - height|`, above zero by construction here. Both
/// wind like `rounded_rect` (left, bottom, right, top), since the fill fan's inset direction comes
/// from the contour's winding.
///
/// A box whose sides differ by a hair still leaves a hair-length segment and can still bevel.
/// Nothing produces one: a config asks for a circle (one expression sets both sides equal) or a
/// pill (they differ by the whole run of the content).
fn box_path(rect: LogicalRect, radius: f32) -> Path {
    let LogicalRect { x, y, width: w, height: h } = rect;
    let mut path = Path::new();

    if radius <= 0.0 || w <= 0.0 || h <= 0.0 {
        path.rect(x, y, w, h);
    } else if radius < w.min(h) / 2.0 {
        path.rounded_rect(x, y, w, h, radius);
    } else if w == h {
        path.circle(x + w / 2.0, y + h / 2.0, w / 2.0);
    } else if w > h {
        let r = h / 2.0;
        let (cy, right) = (y + r, x + w - r);
        // Top of the left cap, round the left to its bottom; the bottom edge; the right cap, round
        // to its top; `close` walks the top edge back. `Solidity::Solid` sweeps by *decreasing*
        // angle, which with y pointing down is the left-bottom-right-top direction wanted here.
        path.arc(x + r, cy, r, 3.0 * FRAC_PI_2, FRAC_PI_2, Solidity::Solid);
        path.arc(right, cy, r, FRAC_PI_2, -FRAC_PI_2, Solidity::Solid);
        path.close();
    } else {
        let r = w / 2.0;
        let (cx, bottom) = (x + r, y + h - r);
        path.arc(cx, y + r, r, 0.0, -PI, Solidity::Solid);
        path.arc(cx, bottom, r, PI, 0.0, Solidity::Solid);
        path.close();
    }

    path
}

/// The background fill, rounded when the node asked for it. See [`box_path`] for why a radius at
/// half the box is its own shape rather than a `rounded_rect` argument.
fn fill_rect(canvas: &mut Canvas<OpenGl>, rect: LogicalRect, radius: f32, color: Rgba) {
    let path = box_path(rect, radius);
    canvas.fill_path(&path, &Paint::color(Color::rgbaf(color.r, color.g, color.b, color.a)));
}

/// femtovg has no per-edge border primitive, so this covers exactly two cases. Uniform borders
/// (all four edges same width and colour) with `radius` above 0 get one `stroke_path` over the
/// rounded rect, inset by half the stroke width: femtovg strokes centred on the path, so drawing on
/// `rect`'s own edge would straddle it, half inside and half outside. Everything else, any edge
/// differing or radius 0, fills each edge that declares both a non-zero width and a colour as its
/// own rectangle.
///
/// ponytail: the per-edge-rectangle fallback ignores `radius`, giving square corners where a
/// rounded background shows through. Upgrade path: four corner arcs plus four mitred edge
/// segments, once a real config needs a rounded per-edge border.
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
        // `snap_border_band` rounds a box's own two edges to nearest, so it snaps the node's span
        // on each axis, not only a hairline's thickness. The stroke's thickness is snapped the same
        // way (band-of-one starting at `rect.x`, only the thickness half kept), so with integer box
        // edges and an integer thickness the centerline lands on an integer for an even width and a
        // half-integer for an odd one, the parity femtovg actually rasterizes (this module's
        // `snap_border_band` doc comment).
        let (box_x, box_width) = snap_border_band(rect.x, rect.width, scale);
        let (box_y, box_height) = snap_border_band(rect.y, rect.height, scale);
        let (_, width) = snap_border_band(rect.x, widths.top, scale);
        let inset = width / 2.0;
        let path = box_path(
            LogicalRect {
                x: box_x + inset,
                y: box_y + inset,
                width: (box_width - width).max(0.0),
                height: (box_height - width).max(0.0),
            },
            radius,
        );
        let mut paint = Paint::color(Color::rgbaf(color.r, color.g, color.b, color.a));
        paint.set_line_width(width);
        canvas.stroke_path(&path, &paint);
        return;
    }

    // Corners overlap here rather than mitre: each edge is its own filled rect spanning the node's
    // full width or height, so two adjacent non-zero edges both cover the corner they share.
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

/// Which dimension of an edge rect is the thin one: top/bottom are thin in y, left/right in x.
/// `paint_border_edge` needs this to know which axis to hand `snap_border_band`; inferring it from
/// the rect's own width/height would be ambiguous whenever a node's height equals its border width.
enum EdgeAxis {
    Horizontal,
    Vertical,
}

/// One border edge: paints only where both a colour and a non-zero width say so
/// (`node::parse_border_color`'s doc comment: `border_width` alone is documented § 5.2 behaviour,
/// not a bug). Snaps the edge's thin axis with `snap_border_band` first, the same whole-physical-
/// pixel treatment as the uniform-radius stroke above; the long axis is left alone, since only the
/// thin axis can straddle a pixel boundary and blur.
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
    /// declared font chain cosmic-text shaped against (ADR-0043 decision 2).
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

    /// Draw order is tree order, which is what makes ADR-0023's stacking model come
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
                    "a glyph pixel at ({x}, {y}) is {:?}, which is not the green `foreground` asked for -- white here means `foreground` never reached `draw_text`",
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
    /// edit cannot satisfy this test at one size and call it fixed. Both are the stadium branch of
    /// [`box_path`] now, and this is the half of the pair that catches a radius left at exactly
    /// half; a radius shaved just short of it passes here and folds its fill fan over instead,
    /// which is what [`a_translucent_ground_at_half_radius_blends_once_not_twice`] is for.
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

    /// A translucent ground at radius half the box must blend exactly once everywhere inside it.
    ///
    /// The shape being right is not enough, and that is the point of this test sitting beside
    /// [`a_radius_of_half_the_box_fills_a_stadium_not_a_square`]: `rounded_rect` at exactly half
    /// draws a correct outline and then folds its fill fan over itself at each of the four
    /// collapsed straight segments, so a translucent ground gets a second helping of itself along
    /// a one-pixel chord out of each cap. Opaque fills hide it. Every control on the dev bar is at
    /// 42% alpha, so none of them did.
    ///
    /// 33x33 at offset 20 rather than a round 32, because the fold is erratic in the size: a sweep
    /// of square boxes from 24 to 43.5 at radius exactly half found seams at 30 of the 80
    /// geometries tried, and 32x32 at an integer offset was one of the clean ones. This case is one
    /// of the dirty ones, so it fails against a plain `rounded_rect`.
    #[test]
    fn a_translucent_ground_at_half_radius_blends_once_not_twice() {
        let Some(instance) = init_headless_egl(96, 96) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 96) else { return };

        // Black at 42% over the panel's red: one blend is 255 * 0.58, two is 255 * 0.58^2 = 86.
        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 96, height = 96, background = "#FF0000FF", padding = { top = 20, left = 20 }, child = rect {
                width = 33, height = 33, background = "#0000006B", radius = 16.5,
            } }"##,
            LogicalSize { width: 96.0, height: 96.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
        let image = painter.canvas_mut().screenshot().expect("screenshot reads back the pbuffer's own framebuffer");

        // The inscribed disc less four pixels, which clears the antialiased rim on every side.
        let centre = 20.0 + 33.0 / 2.0;
        let radius = 33.0 / 2.0 - 4.0;
        let mut doubled = Vec::new();
        for y in 0..96usize {
            for x in 0..96usize {
                let (dx, dy) = (x as f32 + 0.5 - centre, y as f32 + 0.5 - centre);
                if dx * dx + dy * dy > radius * radius {
                    continue;
                }
                let pixel = image[(x, y)];
                if (pixel.r, pixel.g, pixel.b) != (148, 0, 0) {
                    doubled.push((x, y, pixel.r));
                }
            }
        }
        assert!(doubled.is_empty(), "the ground blended twice at {doubled:?}");

        // And the shape is still a circle, so nothing above can be satisfied by drawing less.
        assert_eq!(
            pixel_at(painter.canvas_mut(), 21, 21),
            (255, 0, 0, 255),
            "the corner of a circle is outside it, so the panel behind must show through"
        );
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

    /// The regression test for ADR-0043 decision 2:
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
            line_height: crate::text::shaping::line_height(FONT_SIZE),
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

    /// A `text` node's content
    /// wider than the box layout gave it must stop at that box's edge, not paint over whatever
    /// sits to its right. The live MPRIS-title-through-two-cells bug the doc comment
    /// describes.
    ///
    /// Proved this test is real, not just a green test: with `run`'s `save`/
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

    /// Regression test: a 1px border at a fractional
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
    /// its glyphs. A `row` whose children overflow is the same defect as an overflowing `text`,
    /// not a separate case. This is the
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

    // ---- `clip = "Rounded"` ----

    /// The shape `dev-config`'s battery indicator is: a pill with a child filling its left third.
    /// Without the rounded clip that child is a square-cornered block poking out of the left cap,
    /// which is why the config gave it the pill's own radius and got a lozenge instead.
    #[test]
    fn a_rounded_clip_cuts_a_child_by_the_parents_arc() {
        let Some(instance) = init_headless_egl(96, 48) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 48) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 96, height = 48, background = "#FF0000FF",
                padding = { top = 8, left = 8 }, child = rect {
                    width = 80, height = 32, radius = 16, clip = "Rounded",
                    children = { rect { width = 30, height = "Fill", background = "#0000FFFF" } },
                } }"##,
            LogicalSize { width: 96.0, height: 48.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);

        // The pill spans (8, 8) to (88, 40) with radius 16, so its corner arcs are centred at
        // (24, 24) and the child covers x in [8, 38).
        let canvas = painter.canvas_mut();
        assert_eq!(
            pixel_at(canvas, 10, 10),
            (255, 0, 0, 255),
            "the child's own top-left corner is outside the pill's arc, so the panel shows through"
        );
        assert_eq!(pixel_at(canvas, 10, 24), (0, 0, 255, 255), "at the pill's waist the child reaches its edge");
        assert_eq!(
            pixel_at(canvas, 30, 10),
            (0, 0, 255, 255),
            "past the arc the pill's top edge is straight, so nothing may round the child there"
        );
        assert_eq!(pixel_at(canvas, 50, 24), (255, 0, 0, 255), "the child ends at x = 38 and nothing extends it");
    }

    /// The default is unchanged and costs nothing: no `clip` means square corners, which is what
    /// every node did before this property existed.
    #[test]
    fn without_the_property_a_radius_still_clips_square() {
        let Some(instance) = init_headless_egl(96, 48) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 48) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 96, height = 48, background = "#FF0000FF",
                padding = { top = 8, left = 8 }, child = rect {
                    width = 80, height = 32, radius = 16,
                    children = { rect { width = 30, height = "Fill", background = "#0000FFFF" } },
                } }"##,
            LogicalSize { width: 96.0, height: 48.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
        assert_eq!(pixel_at(painter.canvas_mut(), 10, 10), (0, 0, 255, 255));
    }

    /// A child that would escape the parent entirely is still bound by the rectangle, so the
    /// rounded pass narrows the clip rather than replacing it.
    #[test]
    fn a_rounded_clip_still_holds_a_child_to_the_parents_box() {
        let Some(instance) = init_headless_egl(96, 48) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 48) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 96, height = 48, background = "#FF0000FF",
                padding = { top = 8, left = 8 }, child = rect {
                    width = 40, height = 32, radius = 16, clip = "Rounded",
                    children = { rect { width = 90, height = 90, background = "#0000FFFF" } },
                } }"##,
            LogicalSize { width: 96.0, height: 48.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
        let canvas = painter.canvas_mut();
        assert_eq!(pixel_at(canvas, 60, 24), (255, 0, 0, 255), "the pill ends at x = 48");
        assert_eq!(pixel_at(canvas, 24, 44), (255, 0, 0, 255), "and at y = 40");
        assert_eq!(pixel_at(canvas, 24, 24), (0, 0, 255, 255));
    }

    /// One rounded clip inside another. The inner pass has to put the render target back to its
    /// parent's image rather than to the screen, and this is the assertion that catches it: were
    /// the inner subtree sent to the framebuffer it would paint unmasked and outside the outer arc.
    #[test]
    fn a_rounded_clip_nests_inside_another_one() {
        let Some(instance) = init_headless_egl(96, 96) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 96) else { return };

        // A 64x64 circle at (16, 16), holding a 64x64 child that is itself a rounded clip holding a
        // square block covering the whole box. Both arcs have to survive.
        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 96, height = 96, background = "#FF0000FF",
                padding = { top = 16, left = 16 }, child = rect {
                    width = 64, height = 64, radius = 32, clip = "Rounded",
                    children = { rect {
                        width = 64, height = 64, radius = 32, clip = "Rounded",
                        children = { rect { width = 64, height = 64, background = "#0000FFFF" } },
                    } },
                } }"##,
            LogicalSize { width: 96.0, height: 96.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
        let canvas = painter.canvas_mut();
        assert_eq!(pixel_at(canvas, 48, 48), (0, 0, 255, 255), "the middle of the disc");
        assert_eq!(pixel_at(canvas, 20, 20), (255, 0, 0, 255), "the corner of the box is outside the disc");
    }

    /// A translucent child blends with what is behind it once, not twice. The offscreen pass is
    /// where this can go wrong: femtovg stores premultiplied results in a render target, and
    /// compositing without `PREMULTIPLIED` multiplies the alpha in a second time.
    #[test]
    fn a_translucent_child_under_a_rounded_clip_blends_once() {
        let Some(instance) = init_headless_egl(96, 48) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 48) else { return };

        // Blue at 50% over the panel's red: one blend is 128 of each, two would be 64 blue.
        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 96, height = 48, background = "#FF0000FF",
                padding = { top = 8, left = 8 }, child = rect {
                    width = 80, height = 32, radius = 16, clip = "Rounded",
                    children = { rect { width = 30, height = "Fill", background = "#0000FF80" } },
                } }"##,
            LogicalSize { width: 96.0, height: 48.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
        let (r, _, b, _) = pixel_at(painter.canvas_mut(), 20, 24);
        assert!((126..=129).contains(&r) && (126..=129).contains(&b), "blended to ({r}, _, {b}, _), expected ~128");
    }

    /// The border paints over the clipped subtree, the way QML's `ClippingRectangle` does. A fill
    /// reaching the arc otherwise covers the border exactly where the arc is, which is the half of
    /// a pill's outline most worth seeing.
    #[test]
    fn a_rounded_clips_border_paints_over_its_children() {
        let Some(instance) = init_headless_egl(96, 48) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 48) else { return };

        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 96, height = 48, background = "#FF0000FF",
                padding = { top = 8, left = 8 }, child = rect {
                    width = 80, height = 32, radius = 16, clip = "Rounded",
                    border_width = 4, border_color = "#00FF00FF",
                    children = { rect { width = 30, height = "Fill", background = "#0000FFFF" } },
                } }"##,
            LogicalSize { width: 96.0, height: 48.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
        assert_eq!(
            pixel_at(painter.canvas_mut(), 10, 24),
            (0, 255, 0, 255),
            "the left cap's border is where the child reaches, so it has to be on top"
        );
    }

    /// A leaf that asks for a rounded clip has nothing to clip, so it buys no offscreen pass.
    #[test]
    fn a_childless_rounded_clip_builds_no_group() {
        let lua = Lua::new();
        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 96, height = 48, child = rect {
                width = 80, height = 32, radius = 16, clip = "Rounded", background = "#0000FFFF",
            } }"##,
            LogicalSize { width: 96.0, height: 48.0 },
        );
        let list = build(&root, 1.0, None);
        assert!(!list.commands.iter().any(|cmd| matches!(cmd.draw, Draw::Clipped { .. })));
    }

    /// The property is in the display list, so turning it on repaints: ADR-0063 skips the frame
    /// when the list compares equal, and a clip that changed shape without changing the list would
    /// never be drawn.
    #[test]
    fn changing_only_the_clip_changes_the_display_list() {
        let size = LogicalSize { width: 96.0, height: 48.0 };
        let src = |clip: &str| {
            format!(
                r##"return panel {{ id = "bar", width = 96, height = 48, child = rect {{
                    width = 80, height = 32, radius = 16, clip = "{clip}",
                    children = {{ rect {{ width = 30, height = "Fill", background = "#0000FFFF" }} }},
                }} }}"##
            )
        };
        let boxed = build(&resolved_surface(&Lua::new(), &src("Box"), size), 1.0, None);
        let rounded = build(&resolved_surface(&Lua::new(), &src("Rounded"), size), 1.0, None);
        assert_ne!(boxed, rounded);
    }

    /// The group's own clip can be narrower than the node's box, when an ancestor cuts it. The
    /// offscreen image is sized to that clip while the mask path is drawn on the full box, so this
    /// is where the two coordinate systems have to agree.
    #[test]
    fn a_rounded_clip_hanging_off_its_parent_still_lands_where_it_belongs() {
        let Some(instance) = init_headless_egl(96, 48) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 96, 48) else { return };

        // A 64-wide pill starting 40px into a 60px-wide parent, so its right 44px are cut away by
        // the parent's box and the group's clip runs (48, 8) to (108, 40) intersected to x < 68.
        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 96, height = 48, background = "#FF0000FF",
                padding = { top = 8, left = 8 }, child = rect {
                    width = 60, height = 32, children = { rect {
                        margin = { left = 40 }, width = 64, height = 32, radius = 16, clip = "Rounded",
                        children = { rect { width = 64, height = "Fill", background = "#0000FFFF" } },
                    } },
                } }"##,
            LogicalSize { width: 96.0, height: 48.0 },
        );
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
        let canvas = painter.canvas_mut();
        assert_eq!(pixel_at(canvas, 60, 24), (0, 0, 255, 255), "inside the pill and inside the parent");
        assert_eq!(pixel_at(canvas, 50, 10), (255, 0, 0, 255), "the pill's left cap still rounds");
        assert_eq!(pixel_at(canvas, 72, 24), (255, 0, 0, 255), "and the parent's box still ends at x = 68");
    }
}
