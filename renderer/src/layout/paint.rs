//! Draws a resolved layout tree onto a shared femtovg canvas.
//!
//! [`build`] turns a resolved tree into plain Rust [`DisplayList`] data; [`execute`] sends it to
//! femtovg. `paint_surface` skips drawing and `eglSwapBuffers` when the list is unchanged, while
//! `build` stays testable without EGL. A full-surface commit recomposites the whole screen behind
//! it, so unchanged lists skip that cost. On an idle bar with a clock, a 1920x1200 wallpaper went
//! from repainting twice a second to never and niri CPU fell about a third.
//!
//! `node::paint_style` parses during `Scene::apply`; [`build_node`] reads typed data only. Drawing
//! is parent-then-child tree order (ADR-0023), and invisible subtrees draw nothing.
//!
//! `ResolvedNode.rect` is parent-relative, so [`build_node`] accumulates an absolute origin as it
//! descends instead of trusting `rect.x`/`rect.y` as already-absolute.

use std::f32::consts::{FRAC_PI_2, PI};

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, Color, ImageFlags, ImageId, Paint, Path, PixelFormat, RenderTarget, Solidity};

use crate::image::{self, Fit, ImageCache, Load};
use crate::layout::image_shader;
use crate::layout::node::{self, BorderColor, ClipShape, EdgeInsets, PaintStyle, Rgba, StyleRun, TextAlign};
use crate::layout::scene::{NodeId, ResolvedNode};
use crate::text::atlas::{TextDraw, TextPainter};
use crate::text::snap::{LogicalRect, PhysicalRect, snap_border_band, snap_to_physical};

/// Typed draw data. `textfield` and unrecognised kinds contribute no [`DrawCmd`]. No Lua values are
/// kept: mlua table identity would make a signal-resolved table unequal every pass.
#[derive(Debug, Clone, PartialEq)]
pub enum Draw {
    /// Box fill, then border, for containers and all surface roles.
    Box { background: Option<Rgba>, radius: f32, colors: BorderColor, widths: EdgeInsets },
    Text {
        content: String,
        /// Byte ranges drawn in another face, underlined, or recoloured (ADR-0104).
        runs: Vec<StyleRun>,
        font_size: f32,
        /// The family this was measured and drawn in, or `None` for the declared chain
        /// (ADR-0144).
        font: Option<std::sync::Arc<str>>,
        color: Rgba,
        align: TextAlign,
        /// Center a `textfield` line; ordinary text starts at the top of its content box.
        centered: bool,
    },
    /// Theme name, resolved in [`execute`]. Icons carry alpha separately because `Paint::image`
    /// takes it as an argument.
    Icon {
        name: String,
        px: u32,
        alpha: f32,
        /// `currentColor` tint (ADR-0072); `ImageCache` keys on it.
        color: Option<Rgba>,
    },
    /// Node box in physical pixels, used as the `ImageCache` key; the cache downscales a raster to
    /// cover it (ADR-0122).
    /// `retained` is the source this node last had a texture for, carried when `retain` is set
    /// and `source` has not caught up to it yet (ADR-0180); [`run`] draws it if `source` has no
    /// texture. Present only while the two differ, so a settled node's list stops changing.
    Image {
        /// Which retained node this is, so [`execute`] can report back the source it actually drew
        /// (ADR-0183). Readiness is not knowable anywhere else: only the draw has the exact cache
        /// key, and only it can tell a decode that landed from one that failed or was never asked.
        node: NodeId,
        source: String,
        fit: Fit,
        box_px: (u32, u32),
        alpha: f32,
        load: Load,
        retained: Option<String>,
        /// Mid-cross-dissolve (ADR-0181), the alpha `source` is drawn at over `retained`. `None`
        /// when the node is showing one picture, which is when `retained` is a gap cover rather
        /// than the source being crossed away from.
        dissolve: Option<f32>,
        /// The config shader this cross is drawn with and the `params` it is given (ADR-0184).
        /// `None` is the built-in dissolve, and so is a shader that would not build.
        shader: Option<(std::path::PathBuf, Vec<(String, f32)>)>,
    },
    /// A subtree masked by the declaring node's rounded arc. Rectangular clips flatten into each
    /// command; rounded clips stay grouped for [`execute`].
    Clipped { radius: f32, commands: Vec<DrawCmd> },
    /// The subtree of a node with a `scale`/`rotate`/`translate` (ADR-0149), drawn under its
    /// affine. Coordinates inside are the untransformed absolute ones.
    Transformed { matrix: node::Affine, commands: Vec<DrawCmd> },
}

/// One drawable node: what, where, and its precomputed ancestor clip. Intersections are axis
/// aligned and associative, so [`execute`] can set one scissor instead of rebuilding a nest.
#[derive(Debug, Clone, PartialEq)]
pub struct DrawCmd {
    pub rect: LogicalRect,
    pub clip: PhysicalRect,
    pub draw: Draw,
}

/// One surface's draw order, flattened for equality. Before this, the single dirty flag repainted
/// every mapped surface on every re-resolve (ADR-0044 decision 2). Float equality is safe because
/// identical inputs produce identical bits; `NaN` repaints forever rather than leaving stale
/// pixels.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DisplayList {
    pub commands: Vec<DrawCmd>,
}

impl DisplayList {
    /// Whether any `image` in this list draws one of `files` (ADR-0122). A background decode
    /// landing changes no list, since a list names the file and not the texture, so this is how
    /// `wayland::App` tells which surfaces' skipped repaint is now stale.
    pub fn draws_any_of(&self, files: &[std::path::PathBuf]) -> bool {
        fn walk(commands: &[DrawCmd], files: &[std::path::PathBuf]) -> bool {
            commands.iter().any(|command| match &command.draw {
                Draw::Image { source, retained, .. } => files.iter().any(|file| {
                    file.as_os_str() == source.as_str()
                        || retained.as_ref().is_some_and(|cover| file.as_os_str() == cover.as_str())
                }),
                Draw::Clipped { commands, .. } | Draw::Transformed { commands, .. } => walk(commands, files),
                _ => false,
            })
        }
        walk(&self.commands, files)
    }

    /// Images as `(path, box)` cache keys for `ImageCache::trim` pins (ADR-0123). A mapped surface
    /// still shows what it last painted, so that entry must not be evicted underneath it.
    pub fn drawn_images(&self, out: &mut Vec<(std::path::PathBuf, (u32, u32))>) {
        fn walk(commands: &[DrawCmd], out: &mut Vec<(std::path::PathBuf, (u32, u32))>) {
            for command in commands {
                match &command.draw {
                    Draw::Image { source, box_px, retained, .. } => {
                        // The box the *entry* is under, not the box it is drawn into: a vector's
                        // key is squared, and a pin that names the drawn box misses it (ADR-0183).
                        // Both endpoints are pinned, or `trim` frees the very texture covering the
                        // gap and the node blinks after all (ADR-0180).
                        for path in std::iter::once(source).chain(retained.iter()) {
                            let path = std::path::PathBuf::from(path);
                            let key_box = image::cache_box(&path, *box_px);
                            out.push((path, key_box));
                        }
                    }
                    Draw::Clipped { commands, .. } | Draw::Transformed { commands, .. } => walk(commands, out),
                    _ => {}
                }
            }
        }
        walk(&self.commands, out)
    }
}

/// Identity clip before any scissor is pushed.
const UNCLIPPED: PhysicalRect = PhysicalRect { x0: i32::MIN, y0: i32::MIN, x1: i32::MAX, y1: i32::MAX };

/// Overlap of two snapped boxes. Empty (`x1 <= x0` or `y1 <= y0`) skips the whole subtree.
fn intersect(a: PhysicalRect, b: PhysicalRect) -> PhysicalRect {
    PhysicalRect { x0: a.x0.max(b.x0), y0: a.y0.max(b.y0), x1: a.x1.min(b.x1), y1: a.y1.min(b.y1) }
}

fn is_empty(clip: PhysicalRect) -> bool {
    clip.x1 <= clip.x0 || clip.y1 <= clip.y0
}

/// Focused field and draw-safe content. Masked fields carry only destination and character count,
/// never secret bytes (`shared::SecureBuffer::expose_secret`, ADR-0005). Plain fields use `NodeId`,
/// not a box: when a notification moved a field, box-keyed focus lost its caret while keystrokes
/// continued landing in the buffer; a replacement field could also inherit its text (ADR-0099).
pub enum FieldFocus<'a> {
    Masked {
        target: &'a node::SecureSubmitTarget,
        /// `shared::SecureBuffer::char_count`.
        filled: usize,
    },
    Plain {
        /// Focused node, stable across movement, resizing, and inserted siblings.
        id: NodeId,
        text: &'a str,
        caret: bool,
    },
}

/// Flattens `root` without touching a canvas or GL context.
pub fn build(root: &ResolvedNode, scale: f32, focus: Option<&FieldFocus>) -> DisplayList {
    let mut commands = Vec::new();
    build_node(root, 0.0, 0.0, scale, UNCLIPPED, 1.0, focus, &mut commands);
    DisplayList { commands }
}

/// One node, then its children in tree order. Origins accumulate parent-relative rects to an
/// absolute position.
// ponytail: keep the eight scalar/context arguments; a wrapper would only bag them for one caller.
#[allow(clippy::too_many_arguments)]
fn build_node(
    node: &ResolvedNode,
    origin_x: f32,
    origin_y: f32,
    scale: f32,
    clip: PhysicalRect,
    inherited_opacity: f32,
    focus: Option<&FieldFocus>,
    out: &mut Vec<DrawCmd>,
) {
    if !node.visible {
        return;
    }

    let x = origin_x + node.rect.x;
    let y = origin_y + node.rect.y;
    let rect = LogicalRect { x, y, width: node.rect.width, height: node.rect.height };

    // Snap this box and intersect it with ancestor clips. Wrapped text is already rewritten by
    // `fit_text_to_box`; this remains a backstop for unwrapped overflow. Clips stay rectangular
    // here; `clip = "Rounded"` creates a grouped mask below.
    //
    // ponytail: `layout::hit` intersects the same rectangles but knows nothing about the arc, so a
    // pill's corner is outside its fill yet still takes a click (four pixels on a 34px control).
    // Upgrade path: hit testing should share this walk instead of a second copy of the rule.
    let clip = intersect(clip, snap_to_physical(rect, scale));
    // Fully clipped children cannot draw.
    if is_empty(clip) {
        return;
    }

    // `node::paint_style` already decided the draw. An unrecognised kind stays transparent, which
    // avoids the passwordless black lock screen ADR-0052 decision 3 rejects. Opacity is baked into
    // the list because ADR-0063 skips unchanged lists; applying it in `execute` would be invisible.
    let opacity = inherited_opacity * node.opacity;
    let draw = draw_for(node, rect, scale, opacity, focus);

    // A transformed node paints itself and its subtree as one group under its matrix
    // (ADR-0149), so the group is built into `out` and lifted out of it afterwards. Coordinates
    // inside stay the untransformed absolute ones this walk computes; the matrix is about the
    // node's absolute origin, so the canvas maps them at draw time. Scissors inside follow the
    // matrix too, femtovg's own rule, which is right for the node's own box.
    // ponytail: an ancestor's clip is carried along as well, so a scaled child overflowing its
    // parent is cut by the parent's box scaled with it, not the box itself. Upgrade path: set the
    // parent clip once outside the group and `intersect_scissor` inside.
    let start = out.len();
    let (x, y) = (rect.x, rect.y);
    match rounded_clip(node) {
        None => {
            if let Some(draw) = draw {
                out.push(DrawCmd { rect, clip, draw });
            }
            for child in &node.children {
                build_node(child, x, y, scale, clip, opacity, focus, out);
            }
        }
        // Rounded order matches QML: fill, masked subtree, border. A child reaching the arc would
        // cover a border painted first.
        Some(radius) => {
            let (fill, border) = split_fill_and_border(draw);
            if let Some(fill) = fill {
                out.push(DrawCmd { rect, clip, draw: fill });
            }
            let mut inner = Vec::new();
            for child in &node.children {
                build_node(child, x, y, scale, clip, opacity, focus, &mut inner);
            }
            // A leaf has nothing to clip, so avoid the render target and composite.
            if !inner.is_empty() {
                out.push(DrawCmd { rect, clip, draw: Draw::Clipped { radius, commands: inner } });
            }
            if let Some(border) = border {
                out.push(DrawCmd { rect, clip, draw: border });
            }
        }
    }
    if !node.transform.is_identity() {
        let matrix = node.transform.matrix(rect);
        let commands: Vec<DrawCmd> = out.drain(start..).collect();
        out.push(DrawCmd { rect, clip, draw: Draw::Transformed { matrix, commands } });
    }
}

/// The positive radius of a node whose children use a rounded clip.
fn rounded_clip(node: &ResolvedNode) -> Option<f32> {
    match node.paint {
        Some(PaintStyle::Box { clip: ClipShape::Rounded, radius, .. }) if radius > 0.0 => Some(radius),
        _ => None,
    }
}

/// Splits a box into fill and border so [`build_node`] can mask children between them. Other draws
/// stay whole; [`rounded_clip`] only returns for boxes.
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

/// Test helper that paints a tree through [`build`] and [`execute`] at the caller's scale.
/// The production caller is `wayland::App::paint_surface`: `socket.rs` keys a `Scene` by the `id` a
/// config writes and `wayland::App` keys a `wl_surface` the same way, since ADR-0038 decision 1
/// deleted the fixed Rust-owned role enum that had kept the two id spaces from overlapping.
#[cfg(test)]
pub fn paint_tree(painter: &mut TextPainter, images: &mut ImageCache, root: &ResolvedNode, scale: f32) {
    // No GL context reaches this harness, so a config shader falls back to the dissolve.
    execute(painter, images, &build(root, scale, None), scale, (0.0, 0.0), None);
}

/// Executes an already-built list; keeping canvas work separate makes the list comparable and
/// [`build`] EGL-free.
/// One `image` node's source that this paint had a texture for. `layout::scene` moves the node onto
/// it, which is how a `retain` cover ends and a `transition` begins (ADR-0183).
///
/// Reported from the draw rather than inferred from `ImageCache::poll`, because only the draw asks
/// the cache with the node's exact key. The landing cue this replaces named a path: it fired for a
/// decode that had *failed*, never fired at all when the source was already cached, and could not
/// tell a thumbnail's landing from the full-size image's.
#[derive(Debug, Clone, PartialEq)]
pub struct DrawnImage {
    pub node: NodeId,
    pub source: String,
}

/// The GL context and the compiled config shaders, for the runs that need them (ADR-0184). Absent
/// wherever there is no context to draw with -- a test harness, a paint before a surface has bound
/// one -- and every cross then falls back to the built-in dissolve.
pub struct Shaders<'a> {
    pub gl: &'a glow::Context,
    pub stage: &'a mut image_shader::ShaderStage,
}

/// Everything a paint walk carries besides the commands and the target it is drawing into: the
/// cache it asks for textures, what it has produced so far, and the shaders it may use. Bundled
/// because these are one context threaded whole through a recursion, and passing them apart made
/// `run` and `draw_clipped` a wall of positional arguments.
struct Walk<'a, 'g> {
    images: &'a mut ImageCache,
    scale: f32,
    /// Offscreen targets held until [`execute`] flushes.
    scratch: Vec<ImageId>,
    drawn: Vec<DrawnImage>,
    shaders: Option<Shaders<'g>>,
}

/// The framebuffer a walk is drawing into and the transform in force there. Both are the screen's
/// until `draw_clipped` opens an offscreen or `Draw::Transformed` sets a matrix, and both are
/// things a shader quad needs that femtovg's own draws get from the canvas (ADR-0184).
#[derive(Clone, Copy)]
struct Frame {
    /// Size of the target, and where its top-left sits in surface coordinates.
    size: (f32, f32),
    origin: (f32, f32),
    transform: Option<node::Affine>,
}

pub fn execute(
    painter: &mut TextPainter,
    images: &mut ImageCache,
    list: &DisplayList,
    scale: f32,
    target_size: (f32, f32),
    shaders: Option<Shaders<'_>>,
) -> Vec<DrawnImage> {
    // Before recording draws, after the previous flush: evicted textures cannot be queued draws.
    images.release_evicted(painter.canvas_mut());
    // Upload before any draw names the texture.
    images.upload_landed(painter.canvas_mut());
    let mut walk = Walk { images, scale, scratch: Vec::new(), drawn: Vec::new(), shaders };
    let frame = Frame { size: target_size, origin: (0.0, 0.0), transform: None };
    run(painter, &mut walk, &list.commands, RenderTarget::Screen, frame);
    painter.canvas_mut().reset_scissor();
    painter.canvas_mut().flush();
    // Delete scratch targets only after flush; femtovg still executes queued calls at flush, as
    // `release_shadow_images` does for drop-shadow targets.
    for id in std::mem::take(&mut walk.scratch) {
        painter.canvas_mut().delete_image(id);
    }
    walk.drawn
}

/// Runs commands against `target`, recursively restoring parent images for nested clips. `scratch`
/// holds offscreen images until [`execute`] flushes.
fn run(painter: &mut TextPainter, walk: &mut Walk<'_, '_>, commands: &[DrawCmd], target: RenderTarget, frame: Frame) {
    let scale = walk.scale;
    for command in commands {
        // `command.clip` already contains every ancestor intersection, so set the final scissor.
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
                // `None` skips the fill; alpha 0 remains an explicit transparent rect.
                if let Some(color) = background {
                    fill_rect(painter.canvas_mut(), rect, *radius, *color);
                }
                paint_border(painter.canvas_mut(), rect, *radius, *colors, *widths, scale);
            }
            Draw::Text { content, runs, font_size, font, color, align, centered } => {
                let mut rect = rect;
                if *centered {
                    rect.y += ((rect.height - crate::text::shaping::line_height(*font_size)) / 2.0).max(0.0);
                }
                painter.draw_text(
                    TextDraw {
                        text: content,
                        runs,
                        font_size: *font_size,
                        font: font.as_deref(),
                        color: *color,
                        align: *align,
                    },
                    rect,
                    scale,
                )
            }
            Draw::Icon { name, px, alpha, color } => {
                // `freedesktop-icons` uses `u16`; themes have no directory above 512.
                if let Some(path) = image::icons::resolve(name, (*px).min(512) as u16) {
                    let draw = FileDraw {
                        fit: Fit::Contain,
                        rect,
                        box_px: (*px, *px),
                        alpha: *alpha,
                        tint: *color,
                        load: Load::Inline,
                    };
                    let _ = draw_file(painter.canvas_mut(), walk.images, &path, draw);
                }
            }
            Draw::Image { node, source, fit, box_px, alpha, load, retained, dissolve, shader } => {
                let draw = FileDraw { fit: *fit, rect, box_px: *box_px, alpha: *alpha, tint: None, load: *load };
                let under = retained.as_deref().map(std::path::Path::new);
                match dissolve {
                    Some(progress) => {
                        // Asked for whether or not a pixel of it is visible yet: the answer is
                        // about the texture, and it is what starts the run (ADR-0183).
                        let to = file_texture(painter.canvas_mut(), walk.images, std::path::Path::new(source), draw);
                        if to.is_some() {
                            walk.drawn.push(DrawnImage { node: *node, source: source.clone() });
                        }
                        let from = under.and_then(|under| file_texture(painter.canvas_mut(), walk.images, under, draw));

                        // The stage takes the whole cross, both endpoints at once, which is the
                        // only way an effect can be anything but a fade (ADR-0184) -- and, with no
                        // effect named, the only way a fade composes exactly (ADR-0186). It needs
                        // both textures and a context; without either, the two draws below take
                        // the frame, which is why they were built first and why they stay.
                        let crossed = match (walk.shaders.as_mut(), from, to) {
                            (Some(shaders), Some((from, from_rect)), Some((to, to_rect))) => {
                                let params: &[(String, f32)] =
                                    shader.as_ref().map_or(&[], |(_, params)| params.as_slice());
                                let run = image_shader::Run {
                                    from,
                                    to,
                                    from_rect,
                                    to_rect,
                                    rect,
                                    transform: frame.transform,
                                    clip,
                                    target_size: frame.size,
                                    target_origin: frame.origin,
                                    opacity: *alpha,
                                    progress: *progress,
                                    params,
                                };
                                let effect = shader.as_ref().map(|(path, _)| path.as_path());
                                // SAFETY: `paint_surface` made this context current before calling
                                // `execute`, and it is the one every GL object here belongs to.
                                unsafe { shaders.stage.draw(shaders.gl, painter.canvas_mut(), effect, &run) }
                            }
                            _ => false,
                        };
                        // Last resort, and an approximation: the outgoing at its own full alpha
                        // with the incoming fading in over it. Exact for opaque endpoints at full
                        // opacity, and wrong otherwise -- at `alpha` 0.5 and `progress` 0.5 these
                        // two draws compose to 0.625 where 0.5 is right, showing the surface's
                        // ground through the middle (ADR-0181, corrected by ADR-0186).
                        //
                        // Reached when there is no GL context, when either endpoint has no texture
                        // yet, or when even the engine's own shader would not build. The first is
                        // the test harness; the rest are real and are why this stays.
                        if !crossed {
                            if let Some((id, fitted)) = from {
                                fill_image(painter.canvas_mut(), id, fitted, *alpha);
                            }
                            if let Some((id, fitted)) = to {
                                fill_image(painter.canvas_mut(), id, fitted, *alpha * *progress);
                            }
                        }
                    }
                    // The named source has no texture: still decoding, or a failure the cache has
                    // already logged once. Either way the node keeps its last picture rather than
                    // showing the surface behind it (ADR-0180). A cover that is itself gone --
                    // evicted despite the pin, or deleted from disk -- draws nothing, which is the
                    // old behaviour.
                    None => {
                        if draw_file(painter.canvas_mut(), walk.images, std::path::Path::new(source), draw) {
                            walk.drawn.push(DrawnImage { node: *node, source: source.clone() });
                        } else if let Some(under) = under {
                            draw_file(painter.canvas_mut(), walk.images, under, draw);
                        }
                    }
                }
            }
            Draw::Clipped { radius, commands } => {
                draw_clipped(painter, walk, rect, clip, *radius, commands, target, frame)
            }
            Draw::Transformed { matrix, commands } => {
                let canvas = painter.canvas_mut();
                canvas.save();
                canvas.set_transform(&femtovg::Transform2D(*matrix));
                run(painter, walk, commands, target, Frame { transform: Some(*matrix), ..frame });
                painter.canvas_mut().restore();
            }
        }
    }
}

/// Draws `commands` into an offscreen image, then fills the node's rounded path with that image.
/// femtovg 0.26's `intersect_rounded_scissor` carries one rounded rectangle; on an 80x32 pill at
/// radius 16 with a 30px child it re-rounded the child and leaked the ground 8% through the pill's
/// straight top edge. `dev-config`'s battery indicator worked around the same lozenge by rounding
/// the child, which would only move the bug. Quickshell's `ClippingRectangle` uses a mask texture
/// and two targets; femtovg's image-painted path needs one.
///
/// ponytail: one image allocated and freed per clipping node per repaint. Upgrade path: a pool
/// keyed by size next to `ImageCache`, once a config repaints a rounded clip at pointer rate.
// ponytail: keep the nine scalar/context arguments; passing `DrawCmd` would require a second match.
#[allow(clippy::too_many_arguments)]
fn draw_clipped(
    painter: &mut TextPainter,
    walk: &mut Walk<'_, '_>,
    rect: LogicalRect,
    clip: PhysicalRect,
    radius: f32,
    commands: &[DrawCmd],
    target: RenderTarget,
    frame: Frame,
) {
    let (width, height) = ((clip.x1 - clip.x0) as usize, (clip.y1 - clip.y0) as usize);
    // A box with no area shows nothing, and asking for a 0xN render target leaves GL with an
    // incomplete framebuffer that the next composite on this canvas paints as a full square. A
    // pill cell tweening its width through zero (`components/expanding_pill.lua`) hit this on the
    // first and last frame of every expansion.
    if width == 0 || height == 0 {
        return;
    }
    // `PREMULTIPLIED` prevents a second alpha multiplication; `FLIP_Y` maps canvas y=0 to the last
    // GL texture row. Both match femtovg 0.26.0's drop-shadow flags (`src/lib.rs`).
    let flags = ImageFlags::PREMULTIPLIED | ImageFlags::FLIP_Y;
    let Ok(image) = painter.canvas_mut().create_image_empty(width, height, PixelFormat::Rgba8, flags) else {
        // Out of texture memory: preserve the subtree unmasked rather than drop it.
        // Into the parent's target, so it keeps the parent's frame: the clip this could not
        // allocate is not where these commands are going.
        run(painter, walk, commands, target, frame);
        return;
    };
    walk.scratch.push(image);

    let canvas = painter.canvas_mut();
    canvas.save();
    canvas.set_render_target(RenderTarget::Image(image));
    canvas.clear_rect(0, 0, width as u32, height as u32, Color::rgbaf(0.0, 0.0, 0.0, 0.0));
    // Set, rather than accumulate, the translation; nested clips otherwise sum offsets. Inner
    // scissors transform with it, so absolute command coordinates need no extra math.
    canvas.reset_transform();
    canvas.translate(-clip.x0 as f32, -clip.y0 as f32);
    // The offscreen's own dimensions: a shader quad inside a rounded clip places itself in that
    // target, not on the screen (ADR-0184).
    // The offscreen's own size, its origin at the clip's corner, and no transform: `draw_clipped`
    // reset the canvas transform above, and composites the result under the outer one afterwards.
    let inner =
        Frame { size: (width as f32, height as f32), origin: (clip.x0 as f32, clip.y0 as f32), transform: None };
    run(painter, walk, commands, RenderTarget::Image(image), inner);

    let canvas = painter.canvas_mut();
    canvas.restore();
    canvas.set_render_target(target);
    let path = box_path(rect, radius);
    let paint = Paint::image(image, clip.x0 as f32, clip.y0 as f32, width as f32, height as f32, 0.0, 1.0);
    canvas.fill_path(&path, &paint);
}

/// Multiplies `opacity` into an existing alpha: half-transparent inside a half-faded panel is a
/// quarter.
fn fade(color: Rgba, opacity: f32) -> Rgba {
    Rgba { a: color.a * opacity, ..color }
}

/// Multiplies border-edge alpha; absent edges stay absent.
fn fade_border(colors: BorderColor, opacity: f32) -> BorderColor {
    BorderColor {
        top: colors.top.map(|c| fade(c, opacity)),
        right: colors.right.map(|c| fade(c, opacity)),
        bottom: colors.bottom.map(|c| fade(c, opacity)),
        left: colors.left.map(|c| fade(c, opacity)),
    }
}

/// Converts a node's parsed paint to a draw. `scale` supplies physical image size and `focus`
/// supplies field content; malformed properties already failed `Scene::apply`.
///
/// Takes the node rather than its `paint`, because an `image` reads three things off it -- the
/// paint, the source it last had a texture for, and any dissolve crossing between them -- and the
/// pass supplies only the geometry.
fn draw_for(
    node: &ResolvedNode,
    rect: LogicalRect,
    scale: f32,
    opacity: f32,
    focus: Option<&FieldFocus>,
) -> Option<Draw> {
    let node_id = node.id;
    let retained = node.displayed_source.as_deref();
    let dissolve = node.dissolve.as_ref();
    match node.paint.as_ref()? {
        // The shared paint of `rect`/`row`/`column`/`button` and all four surface roles: background
        // fill, then borders (`lua-api.md` § 5.2 item 1). `clip` is not read here: it
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
        PaintStyle::Text { content, runs, font_size, font, color, align, elide: _, wrap: _, max_lines: _ } => {
            Some(Draw::Text {
                content: content.clone(),
                runs: runs
                    .iter()
                    .map(|run| StyleRun { color: run.color.map(|c| fade(c, opacity)), ..run.clone() })
                    .collect(),
                font_size: *font_size,
                font: font.clone(),
                color: fade(*color, opacity),
                align: *align,
                centered: false,
            })
        }

        // Icons use `Contain` and the shorter edge: § 5.2's `size` is a bounding-box diameter.
        PaintStyle::Icon { name, color } => Some(Draw::Icon {
            name: name.clone(),
            px: physical_edge(rect.width.min(rect.height), scale),
            alpha: opacity,
            color: *color,
        }),

        // Image source and fit (ADR-0054 decision 3). Empty source draws nothing; both physical
        // edges enter the cache because `Cover` may scale an SVG past the shorter edge (ADR-0122).
        // `retained` is what goes *under* the draw, and it is one of two things: mid-dissolve the
        // picture being crossed away from, otherwise the one the node is still covering a decoding
        // source with. One field because the node is never doing both -- `displayed_source` has
        // already moved on to `source` by the time a dissolve starts.
        PaintStyle::Image { source, fit, load, retain, transition } => (!source.is_empty()).then(|| {
            // What goes under the draw. Dropped once the node draws what it names: an equal pair in
            // the list would be one more thing to compare, and its disappearance ends the cover.
            let cover = match dissolve {
                Some(dissolve) => Some(dissolve.from.clone()),
                None => retained.filter(|_| *retain).filter(|last| *last != source.as_str()).map(str::to_string),
            };
            Draw::Image {
                node: node_id,
                // Mid-dissolve the node draws the run's own destination, not whatever a later pass
                // has since resolved: a third source arriving would otherwise drop the picture this
                // run is halfway to and cross to one with no texture yet (ADR-0183).
                source: dissolve.map_or_else(|| source.clone(), |dissolve| dissolve.to.clone()),
                fit: *fit,
                box_px: (physical_edge(rect.width, scale), physical_edge(rect.height, scale)),
                alpha: opacity,
                load: *load,
                retained: cover.clone(),
                shader: dissolve
                    .and_then(|dissolve| dissolve.spec.shader.clone().map(|path| (path, dissolve.spec.params.clone()))),
                dissolve: match dissolve {
                    Some(dissolve) => Some(dissolve.progress),
                    // A declared transition still covering a gap opens its cross *here*, at zero,
                    // before anything has proved the incoming texture exists -- because asking for
                    // the draw is the only way to prove it (ADR-0183). Drawing the incoming at full
                    // alpha on that frame and starting the cross on the next one shows it whole,
                    // snaps back to the outgoing, and only then crosses.
                    None => (transition.is_some() && cover.is_some()).then_some(0.0),
                },
            }
        }),

        // A `textfield` shows its placeholder until focused, then one mask character per typed
        // character. Wrong-password feedback costs a two-second `pam_fail_delay`; three failures
        // trigger `pam_faillock` and a ten-minute lockout. `retarget_secure_submit` zeroizes the
        // buffer on focus changes, so only the focused field can show typed state.
        PaintStyle::TextField { target, placeholder, mask, font_size, color, align } => {
            let content = match focus {
                // An empty masked field remains a prompt.
                Some(FieldFocus::Masked { target: focused, filled }) if *filled > 0 => {
                    if target.as_ref().is_some_and(|declared| declared == *focused) {
                        mask.repeat(*filled)
                    } else {
                        placeholder.clone()
                    }
                }
                // Empty focused fields also show the placeholder (ADR-0135). The old caret-only
                // rule made prompts unreachable in `launcher.lua` and `wallpaper_picker.lua`,
                // whose `autofocus` keeps the keyboard from the first frame. With no arrow-key
                // movement, the caret stays at the end. Keep `target.is_none()` beside the id:
                // the same node may gain `secure_submit`, and a masked field must never draw plain
                // text.
                Some(FieldFocus::Plain { id, text, caret }) if *id == node_id && target.is_none() => {
                    if !text.is_empty() {
                        // The draft remains visible without a caret (ADR-0108).
                        match caret {
                            true => format!("{text}\u{2502}"),
                            false => text.to_string(),
                        }
                    } else if !placeholder.is_empty() {
                        placeholder.clone()
                    } else if *caret {
                        "\u{2502}".to_string()
                    } else {
                        String::new()
                    }
                }
                _ => placeholder.clone(),
            };
            (!content.is_empty()).then_some(Draw::Text {
                content,
                runs: Vec::new(),
                font_size: *font_size,
                // A `textfield` draws its placeholder and its masked content in the declared
                // chain; nothing in § 5.2 lets one name a family.
                font: None,
                color: fade(*color, opacity),
                align: *align,
                centered: true,
            })
        }
    }
}

/// File-draw parameters shared by icon and image commands.
#[derive(Debug, Clone, Copy)]
struct FileDraw {
    fit: Fit,
    rect: LogicalRect,
    box_px: (u32, u32),
    alpha: f32,
    /// `None` for an `image`, which names a file the config chose rather than a themed icon.
    tint: Option<Rgba>,
    load: Load,
}

/// Cache lookup and one `fill_path` over the fitted rect. Filling the full box with a `Contain`
/// paint would let femtovg clamp the outer pixel row into the letterbox; `Cover` is cropped by the
/// run's scissor.
/// Answers whether it drew, which is how an `image` learns its source has no texture yet and its
/// `retain` cover should take the frame (ADR-0180).
fn draw_file(canvas: &mut Canvas<OpenGl>, images: &mut ImageCache, file: &std::path::Path, draw: FileDraw) -> bool {
    let Some((id, fitted)) = file_texture(canvas, images, file, draw) else {
        return false;
    };
    fill_image(canvas, id, fitted, draw.alpha);
    true
}

/// The texture for `file` and the rect its `fit` puts it in, without drawing it. Split out because
/// a shader cross needs both endpoints' textures and rects and draws neither itself (ADR-0184).
fn file_texture(
    canvas: &mut Canvas<OpenGl>,
    images: &mut ImageCache,
    file: &std::path::Path,
    draw: FileDraw,
) -> Option<(ImageId, LogicalRect)> {
    let FileDraw { fit, rect, box_px, alpha: _, tint, load } = draw;
    let id = images.image(canvas, file, box_px, tint, load)?;
    let (width, height) = canvas.image_size(id).ok()?;
    Some((id, image::fitted_rect(rect, width as f32, height as f32, fit)))
}

fn fill_image(canvas: &mut Canvas<OpenGl>, id: ImageId, fitted: LogicalRect, alpha: f32) {
    let mut path = Path::new();
    path.rect(fitted.x, fitted.y, fitted.width, fitted.height);
    canvas.fill_path(&path, &Paint::image(id, fitted.x, fitted.y, fitted.width, fitted.height, 0.0, alpha));
}

/// One logical edge in physical pixels, rounded and floored at 1. `ImageCache` keys on this
/// integer.
fn physical_edge(logical: f32, scale: f32) -> u32 {
    let physical = logical * scale;
    if !physical.is_finite() || physical <= 1.0 {
        return 1;
    }
    physical.round() as u32
}

/// The path a box with `radius` asks for: a rectangle, rounded rectangle, or stadium. femtovg
/// clamps radius with `rad.min(halfw)`; near that clamp, `rounded_rect` fails in two bands. At
/// exactly half, its zero-length straight segments collapse the fill to a square. Just below,
/// `path::cache`'s half-pixel `woff` bevel inset folds the fill fan back at each join: opaque fill
/// hides it, translucent fill blends folded slivers twice (a one-pixel chord at 1.6x alpha on the
/// dev bar's 42%-alpha controls).
///
/// A sweep of square boxes from 24 to 43.5 logical pixels, at two sub-pixel offsets and 80
/// geometries per row, found:
///
/// | radius below half | filled square | interior seams |
/// | ----------------- | ------------- | -------------- |
/// | 0 (exactly half)  | 80            | 0              |
/// | 0.0001 to 0.01 px | 0             | 29             |
/// | 0.05 px and more  | 0             | 0              |
///
/// A shortfall clears both bands. A shipped `FILL_RADIUS_EPSILON` of 0.01 sat in the second; Qt's
/// `qMin(w, h) * 0.4999f` (`qsgbasicinternalrectanglenode.cpp`) did too at every size. More
/// epsilon is still a constant tuned to one tessellator, so this builds the shape instead.
///
/// Half the smaller side is how config spells a pill: `components/icon_button.lua` writes
/// `side / 2` for a circle, while independently scaled `theme.item_radius` can exceed half of
/// `item_height`. Equal sides use femtovg's circle (four beziers, no straight segments); unequal
/// sides use two semicircular caps joined by `|width - height|`. Both wind like `rounded_rect`
/// (left, bottom, right, top), which controls the fill-fan inset.
///
/// A box whose sides differ by a hair is a circle. A hair-length straight run between the two
/// caps is worse than a bevel: for a box 2 µm narrower than it is tall, the vertical-cap path's
/// fill fan folds over and paints the whole bounding square (`a_box_a_hair_narrower_than_tall_is_
/// still_a_circle`). Tweens produce exactly that: a `width = "Fill"` circle inside a cell whose
/// width and padding both ease lands a rounding error either side of its height on different
/// frames (`components/expanding_pill.lua`), and the narrow frames flashed as squares.
/// Below this the two sides of a box count as equal (`box_path`): the 0.05 px band the sweep in
/// its doc comment found clear of femtovg's bevel fold, and far above any layout rounding error.
const HAIR: f32 = 0.05;

fn box_path(rect: LogicalRect, radius: f32) -> Path {
    let LogicalRect { x, y, width: w, height: h } = rect;
    let mut path = Path::new();

    if radius <= 0.0 || w <= 0.0 || h <= 0.0 {
        path.rect(x, y, w, h);
    } else if radius < w.min(h) / 2.0 {
        path.rounded_rect(x, y, w, h, radius);
    } else if (w - h).abs() <= HAIR {
        path.circle(x + w / 2.0, y + h / 2.0, w.min(h) / 2.0);
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
    let LogicalRect { x, y, width: w, height: h } = rect;
    for (color, thickness, edge_rect, axis) in [
        (colors.top, widths.top, LogicalRect { x, y, width: w, height: widths.top }, EdgeAxis::Horizontal),
        (
            colors.bottom,
            widths.bottom,
            LogicalRect { x, y: y + h - widths.bottom, width: w, height: widths.bottom },
            EdgeAxis::Horizontal,
        ),
        (colors.left, widths.left, LogicalRect { x, y, width: widths.left, height: h }, EdgeAxis::Vertical),
        (
            colors.right,
            widths.right,
            LogicalRect { x: x + w - widths.right, y, width: widths.right, height: h },
            EdgeAxis::Vertical,
        ),
    ] {
        paint_border_edge(canvas, color, thickness, edge_rect, axis, scale);
    }
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
    use crate::text::shaping::{FaceRole, ShapeRequest, ShapingHandle};

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
            shaping.font_generation(),
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
            measured_axes: (false, false),
        }];
        scene.apply(&[surface], &instances, &shaping, lua).unwrap();
        scene.surface("bar@TEST").unwrap().clone()
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

    /// ADR-0183. The frame that first has the incoming texture must already be drawing the cross,
    /// or it shows the incoming at full alpha for one frame and the cross then starts by jumping
    /// back to the outgoing. Readiness is only knowable by asking for the draw, so the ask happens
    /// at zero.
    #[test]
    fn a_transition_waiting_on_its_incoming_texture_draws_it_at_zero_rather_than_at_full_alpha() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = image { id = "wp", source = "/tmp/new.png", async = true,
                transition = { duration = 400, easing = "Linear" },
                width = "Fill", height = "Fill" } }"##;
        let mut tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let image_draw = |tree: &ResolvedNode| {
            build(tree, 1.0, None).commands.iter().find_map(|cmd| match &cmd.draw {
                Draw::Image { retained, dissolve, .. } => Some((retained.clone(), *dissolve)),
                _ => None,
            })
        };

        // Holding the old picture, the new one not yet drawn: no dissolve has started, because
        // nothing has proved the texture exists.
        tree.children[0].displayed_source = Some("/tmp/old.png".to_string());
        assert_eq!(
            image_draw(&tree),
            Some((Some("/tmp/old.png".to_string()), Some(0.0))),
            "the incoming is asked for at zero, so the frame that first has it still shows the outgoing"
        );

        // `retain` with no transition keeps the plain cover: it has no cross to open on.
        let plain = r##"return panel { id = "bar", width = 200, height = 40,
            child = image { id = "wp", source = "/tmp/new.png", async = true, retain = true,
                width = "Fill", height = "Fill" } }"##;
        let mut tree = resolved_surface(&lua, plain, LogicalSize { width: 200.0, height: 40.0 });
        tree.children[0].displayed_source = Some("/tmp/old.png".to_string());
        assert_eq!(image_draw(&tree), Some((Some("/tmp/old.png".to_string()), None)));
    }

    /// ADR-0181. Mid-dissolve the list carries the outgoing picture and the alpha the incoming is
    /// drawn over it at, in the same field the gap cover uses -- the node is never doing both.
    #[test]
    fn a_dissolving_image_carries_the_outgoing_picture_and_the_alpha_to_draw_the_incoming_at() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = image { id = "wp", source = "/tmp/new.png", async = true,
                transition = { duration = 400, easing = "Linear" },
                width = "Fill", height = "Fill" } }"##;
        let mut tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let image_draw = |tree: &ResolvedNode| {
            build(tree, 1.0, None).commands.iter().find_map(|cmd| match &cmd.draw {
                Draw::Image { retained, dissolve, .. } => Some((retained.clone(), *dissolve)),
                _ => None,
            })
        };
        assert_eq!(image_draw(&tree), Some((None, None)), "nothing has landed, so nothing is crossing");

        // What `note_landed_images` leaves behind: the source moved on, the outgoing on the run.
        tree.children[0].displayed_source = Some("/tmp/new.png".to_string());
        tree.children[0].dissolve = Some(node::Dissolve {
            from: "/tmp/old.png".to_string(),
            to: "/tmp/new.png".to_string(),
            started: std::time::Instant::now(),
            spec: node::TransitionSpec {
                duration: std::time::Duration::from_millis(400),
                easing: Default::default(),
                shader: None,
                params: Vec::new(),
            },
            progress: 0.25,
        });
        assert_eq!(image_draw(&tree), Some((Some("/tmp/old.png".to_string()), Some(0.25))));

        // Both are drawn, so both are pinned; losing the outgoing mid-cross is a hole in the frame.
        let mut pinned = Vec::new();
        build(&tree, 1.0, None).drawn_images(&mut pinned);
        assert_eq!(
            pinned,
            vec![
                (std::path::PathBuf::from("/tmp/new.png"), (200, 40)),
                (std::path::PathBuf::from("/tmp/old.png"), (200, 40)),
            ]
        );

        // `transition` implies `retain`, so the same node covers a gap without the property being
        // written twice -- and covering with a transition declared opens the cross at zero, which
        // is the subject of its own test below.
        tree.children[0].dissolve = None;
        tree.children[0].displayed_source = Some("/tmp/old.png".to_string());
        assert_eq!(image_draw(&tree), Some((Some("/tmp/old.png".to_string()), Some(0.0))));
    }

    /// ADR-0180. The cover only reaches the list while the node is behind its own source, it is
    /// pinned so `trim` cannot free the texture it is covering with, and `retain` is what turns it
    /// on: without the property the same stale `displayed_source` says nothing.
    #[test]
    fn a_retaining_image_carries_the_source_it_still_shows_until_the_named_one_catches_up() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = image { id = "wp", source = "/tmp/new.png", async = true, retain = true,
                width = "Fill", height = "Fill" } }"##;
        let mut tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });

        // Nothing has landed yet, so there is nothing to cover the gap with.
        let cover_of = |tree: &ResolvedNode| {
            build(tree, 1.0, None).commands.iter().find_map(|cmd| match &cmd.draw {
                Draw::Image { retained, .. } => Some(retained.clone()),
                _ => None,
            })
        };
        assert_eq!(cover_of(&tree), Some(None), "an image that never drew has no cover");

        tree.children[0].displayed_source = Some("/tmp/old.png".to_string());
        assert_eq!(cover_of(&tree), Some(Some("/tmp/old.png".to_string())));
        let list = build(&tree, 1.0, None);
        let mut pinned = Vec::new();
        list.drawn_images(&mut pinned);
        assert_eq!(
            pinned,
            vec![
                (std::path::PathBuf::from("/tmp/new.png"), (200, 40)),
                (std::path::PathBuf::from("/tmp/old.png"), (200, 40)),
            ],
            "both are pinned on the box they are drawn at, or the cover is evicted mid-cover"
        );
        assert!(list.draws_any_of(&[std::path::PathBuf::from("/tmp/old.png")]));

        // Caught up: the pair is equal, so the list settles instead of carrying a second copy.
        tree.children[0].displayed_source = Some("/tmp/new.png".to_string());
        assert_eq!(cover_of(&tree), Some(None));

        // The same stale state without the property draws nothing while the source decodes, which
        // is the behaviour every image had before this.
        let plain = r##"return panel { id = "bar", width = 200, height = 40,
            child = image { id = "wp", source = "/tmp/new.png", async = true,
                width = "Fill", height = "Fill" } }"##;
        let mut tree = resolved_surface(&lua, plain, LogicalSize { width: 200.0, height: 40.0 });
        tree.children[0].displayed_source = Some("/tmp/old.png".to_string());
        assert_eq!(cover_of(&tree), Some(None), "`retain` is what carries the cover, not the state");
    }

    #[test]
    fn a_list_knows_which_files_it_draws_through_a_rounded_clip_too() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = rect { width = 100, height = 40, radius = 8, clip = "Rounded",
                children = { image { source = "/tmp/a.png", async = true, width = "Fill", height = "Fill" } } } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let list = build(&tree, 1.0, None);
        assert!(list.draws_any_of(&[std::path::PathBuf::from("/tmp/a.png")]));
        assert!(!list.draws_any_of(&[std::path::PathBuf::from("/tmp/b.png")]));
        assert!(!list.draws_any_of(&[]));
        // The same walk names the pin `ImageCache::trim` keeps: the path with the box the image
        // was keyed on, the 100x40 rect it fills.
        let mut pinned = Vec::new();
        list.drawn_images(&mut pinned);
        assert_eq!(pinned, vec![(std::path::PathBuf::from("/tmp/a.png"), (100, 40))]);
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

    fn reply_surface(lua: &Lua) -> ResolvedNode {
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = textfield { width = "Fill", height = 28, placeholder = "Reply",
                on_submit = function(text) end } }"##;
        resolved_surface(lua, src, LogicalSize { width: 200.0, height: 40.0 })
    }

    /// The plain half of § 5.2 item 8 (ADR-0092). Unfocused it is a placeholder like any other
    /// field; focused it shows what has been typed, with a caret after it.
    #[test]
    fn a_plain_textfield_shows_its_placeholder_until_it_is_focused() {
        let lua = Lua::new();
        let tree = reply_surface(&lua);
        assert_eq!(drawn_text(&build(&tree, 1.0, None)), vec!["Reply".to_string()]);

        let id = tree.children[0].id;
        let typed = build(&tree, 1.0, Some(&FieldFocus::Plain { id, text: "on my way", caret: true }));
        assert_eq!(drawn_text(&typed), vec!["on my way\u{2502}".to_string()]);
    }

    /// An empty focused field shows its placeholder, the same as an empty idle one and the same as
    /// an empty masked one (ADR-0135). This asserted the opposite until `autofocus` proved the
    /// distinction unreachable: a field that holds the keyboard from its first frame has no idle
    /// state to be confused with, and the placeholder was text nothing could ever display.
    #[test]
    fn a_focused_but_empty_plain_field_still_shows_its_placeholder() {
        let lua = Lua::new();
        let tree = reply_surface(&lua);
        let id = tree.children[0].id;
        assert_eq!(
            drawn_text(&build(&tree, 1.0, Some(&FieldFocus::Plain { id, text: "", caret: true }))),
            vec!["Reply".to_string()]
        );
    }

    /// The caret alone is what a field with no placeholder to show falls back to, which is the one
    /// case left where an empty focused field still says it is live by drawing something.
    #[test]
    fn a_focused_empty_field_that_declared_no_placeholder_draws_the_caret_alone() {
        let lua = Lua::new();
        let src = r##"return panel { id = "bar", width = 200, height = 40,
            child = textfield { width = "Fill", height = 28, on_submit = function(text) end } }"##;
        let tree = resolved_surface(&lua, src, LogicalSize { width: 200.0, height: 40.0 });
        let id = tree.children[0].id;

        assert_eq!(
            drawn_text(&build(&tree, 1.0, Some(&FieldFocus::Plain { id, text: "", caret: true }))),
            vec!["\u{2502}".to_string()]
        );
        assert!(
            drawn_text(&build(&tree, 1.0, Some(&FieldFocus::Plain { id, text: "", caret: false }))).is_empty(),
            "with no keyboard and nothing to say, an empty field draws nothing at all"
        );
    }

    /// Focus names one node, so a focus naming another leaves this field alone. The case it guards
    /// is two reply fields in one card: only the one clicked into fills.
    /// ADR-0108: the keyboard left, the draft did not.
    #[test]
    fn a_plain_field_without_the_keyboard_draws_its_draft_with_no_caret_and_its_placeholder_when_empty() {
        let lua = Lua::new();
        let tree = reply_surface(&lua);
        let id = tree.children[0].id;
        assert_eq!(
            drawn_text(&build(&tree, 1.0, Some(&FieldFocus::Plain { id, text: "on my way", caret: false }))),
            vec!["on my way".to_string()]
        );
        assert_eq!(
            drawn_text(&build(&tree, 1.0, Some(&FieldFocus::Plain { id, text: "", caret: false }))),
            vec!["Reply".to_string()]
        );
    }

    #[test]
    fn a_plain_field_that_is_not_the_focused_node_keeps_its_placeholder() {
        let lua = Lua::new();
        let tree = reply_surface(&lua);
        let elsewhere = crate::layout::scene::NodeId::test(9999);
        let list = build(&tree, 1.0, Some(&FieldFocus::Plain { id: elsewhere, text: "not mine", caret: true }));
        assert_eq!(drawn_text(&list), vec!["Reply".to_string()]);
    }

    /// The bug the box stood in the way of (ADR-0099). A notification arriving above the card being
    /// replied to re-lays the surface out and the field lands somewhere else, and under a
    /// box-keyed focus paint stopped finding it -- the caret and the typed text vanished from a
    /// field that was still receiving every keystroke. Here the same tree is drawn at two different
    /// geometries and the focus follows the node.
    #[test]
    fn a_focused_plain_field_keeps_its_caret_when_the_layout_moves_it() {
        let lua = Lua::new();
        let tree = reply_surface(&lua);
        let id = tree.children[0].id;
        assert_eq!(
            drawn_text(&build(&tree, 1.0, Some(&FieldFocus::Plain { id, text: "on my way", caret: true }))),
            vec!["on my way\u{2502}".to_string()]
        );

        // The same node, pushed down and narrowed the way a re-resolve would. Kept inside the
        // surface, since a node clipped out entirely draws nothing for reasons unrelated to focus.
        let mut moved = tree.clone();
        moved.children[0].rect.y += 6.0;
        moved.children[0].rect.width -= 40.0;
        assert_eq!(
            drawn_text(&build(&moved, 1.0, Some(&FieldFocus::Plain { id, text: "on my way", caret: true }))),
            vec!["on my way\u{2502}".to_string()],
            "the caret follows the node, not the box it used to occupy"
        );
    }

    /// The other direction, and the reason the id has to be the node's own rather than anything
    /// positional: a *different* field that comes to sit where the focused one was must not
    /// inherit its text.
    #[test]
    fn a_different_field_that_takes_the_focused_ones_box_draws_nothing_of_its_text() {
        let lua = Lua::new();
        let tree = reply_surface(&lua);
        let vacated = tree.children[0].rect;

        let mut other = tree.clone();
        other.children[0].id = crate::layout::scene::NodeId::test(4242);
        other.children[0].rect = vacated;

        let list =
            build(&other, 1.0, Some(&FieldFocus::Plain { id: tree.children[0].id, text: "on my way", caret: true }));
        assert_eq!(drawn_text(&list), vec!["Reply".to_string()]);
    }

    /// A masked field ignores a plain focus outright: the two are different kinds, and a
    /// `secure_submit` field must never draw text a `FieldFocus::Plain` is carrying.
    #[test]
    fn a_masked_field_never_draws_a_plain_focuss_text() {
        let lua = Lua::new();
        let tree = password_surface(&lua);
        let id = tree.children[0].id;
        let list = build(&tree, 1.0, Some(&FieldFocus::Plain { id, text: "hunter2", caret: true }));
        assert_eq!(drawn_text(&list), vec!["password".to_string()]);
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
        let list = build(&password_surface(&lua), 1.0, Some(&FieldFocus::Masked { target: &target, filled: 4 }));
        assert_eq!(drawn_text(&list), vec!["****".to_string()]);
    }

    #[test]
    fn a_focused_but_empty_password_field_still_shows_its_placeholder() {
        let lua = Lua::new();
        let target = lock_target();
        let list = build(&password_surface(&lua), 1.0, Some(&FieldFocus::Masked { target: &target, filled: 0 }));
        assert_eq!(drawn_text(&list), vec!["password".to_string()]);
    }

    /// Focus is a `{ capability, action }` pair, so a field addressed somewhere else must not
    /// fill just because some other field is focused on the same surface. This is the same
    /// routing rule `input::keyboard::retarget_secure_submit` enforces for the bytes themselves.
    #[test]
    fn a_field_addressed_to_another_capability_does_not_draw_the_focused_fields_characters() {
        let lua = Lua::new();
        let elsewhere = node::SecureSubmitTarget { capability: "network".to_string(), action: "connect".to_string() };
        let list = build(&password_surface(&lua), 1.0, Some(&FieldFocus::Masked { target: &elsewhere, filled: 9 }));
        assert_eq!(
            drawn_text(&list),
            vec!["password".to_string()],
            "a PSK's length must not leak onto the lock screen's field"
        );
    }

    /// The count is all paint ever gets (see [`FieldFocus`]), so there is no path by which a
    /// typed character reaches the list. Asserted because a display list is cloned, compared and
    /// retained in `last_painted` -- exactly the places ADR-0005 keeps a secret out of.
    #[test]
    fn a_masked_field_draws_only_the_mask_character() {
        let lua = Lua::new();
        let target = lock_target();
        let list = build(&password_surface(&lua), 1.0, Some(&FieldFocus::Masked { target: &target, filled: 6 }));
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
        let list = build(&tree, 1.0, Some(&FieldFocus::Masked { target: &target, filled: 3 }));
        assert_eq!(drawn_text(&list), vec!["\u{2022}\u{2022}\u{2022}".to_string()]);
    }

    /// The property this feature needs from the display list: typing has to change it, or
    /// `paint_surface` skips the repaint and the dots never appear.
    #[test]
    fn each_typed_character_changes_the_list_so_the_repaint_is_not_skipped() {
        let lua = Lua::new();
        let tree = password_surface(&lua);
        let target = lock_target();
        let three = build(&tree, 1.0, Some(&FieldFocus::Masked { target: &target, filled: 3 }));
        let four = build(&tree, 1.0, Some(&FieldFocus::Masked { target: &target, filled: 4 }));
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
                text { content = "Obelisk", font_size = 24, foreground = "#00FF00FF" },
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
    /// `radius = side / 2`, while independently scaled `theme.item_radius` can exceed half the
    /// item height. Both reached femtovg's half-box clamp, whose fill tessellation collapses to a
    /// rectangle: every bar pill and circle had a square ground under a round border.
    ///
    /// 32x32 at radius 16 is the live dev-bar case. 40x40 at 20 rounded before the fix while
    /// 32x32 did not, showing size-dependent degeneracy rather than a clean threshold; keep both
    /// so one passing size cannot hide it. Both use [`box_path`]'s stadium branch: this catches an
    /// exact half radius, while the companion test catches the just-below-half fill-fold case.
    ///
    /// Both halves are asserted: the corner must show the parent through the round fill, while the
    /// mid-edge must remain border. Squaring the border to match a broken fill would satisfy only
    /// one of those checks.
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
            // Well outside the inscribed circle: `side/2 * (sqrt(2) - 1)` clear, about 6px at 32
            // and 8px at 40, so this is not an antialiasing read.
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

        const TEXT: &str = "Obelisk Shell Renderer";
        const FONT_SIZE: f32 = 24.0;

        let shaped = shaping.shape(ShapeRequest {
            text: TEXT.into(),
            font_size: FONT_SIZE,
            line_height: crate::text::shaping::line_height(FONT_SIZE),
            max_width: None,
            runs: Vec::new(),
            font: None,
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

    /// The same agreement for a bold run (ADR-0104): cosmic-text shaping the run in the family's
    /// bold face and femtovg painting it from the bold variant chain have to land on one width, or
    /// a styled line's pieces drift apart from the box they were measured into.
    #[test]
    fn femtovg_and_cosmic_text_agree_on_a_bold_runs_width() {
        let Some(instance) = init_headless_egl(400, 60) else { return };
        let shaping = ShapingHandle::spawn();
        if !shaping.font_chain_data().iter().any(|face| matches!(face.role, FaceRole::Declared) && face.bold) {
            eprintln!("skip: the chain's family has no bold face installed");
            return;
        }
        let Some(mut painter) = text_painter(&instance, &shaping, 400, 60) else { return };
        assert_ne!(
            painter.variant_fonts(true, false)[0],
            painter.fonts()[0],
            "the bold chain must lead with a face of its own"
        );

        const TEXT: &str = "Obelisk Shell Renderer";
        const FONT_SIZE: f32 = 24.0;
        let shaped = shaping.shape(ShapeRequest {
            text: TEXT.into(),
            font_size: FONT_SIZE,
            line_height: crate::text::shaping::line_height(FONT_SIZE),
            max_width: None,
            runs: vec![crate::text::shaping::FontRun { range: 0..TEXT.len(), bold: true, italic: false }],
            font: None,
        });
        let mut paint = Paint::color(Color::black());
        paint.set_font(painter.variant_fonts(true, false));
        paint.set_font_size(FONT_SIZE);
        let femtovg_width = painter.canvas_mut().measure_text(0.0, 0.0, TEXT, &paint).unwrap().width();
        let diff = (shaped.width - femtovg_width).abs();
        assert!(
            diff <= shaped.width.max(femtovg_width) * 0.02,
            "cosmic-text measured the bold run at {} but femtovg at {}",
            shaped.width,
            femtovg_width
        );
    }

    /// The whole runtime path in one test: a family named for the first time is resolved on the
    /// shaping worker, and `sync` has to get those faces into femtovg -- otherwise the node
    /// measures in one family and paints in another, the divergence ADR-0043 decision 2 closed.
    #[test]
    fn a_family_named_after_the_painter_was_built_reaches_femtovg_through_sync() {
        let Some(instance) = init_headless_egl(400, 60) else { return };
        let (declared, named) = ("Noto Sans", "Noto Sans Mono");
        if !crate::text::fonts::fc_match_available()
            || !crate::text::fonts::family_installed(declared)
            || !crate::text::fonts::family_installed(named)
        {
            eprintln!("skip: need two installed families to tell apart");
            return;
        }
        let shaping = ShapingHandle::spawn();
        shaping.set_chain(&[declared.to_string()]);
        let Some(mut painter) = text_painter(&instance, &shaping, 400, 60) else { return };
        assert_eq!(painter.named_fonts(named), None, "nothing is registered for a family nobody named");

        // What a text node does: measure first, which is what resolves the family.
        shaping.shape(ShapeRequest {
            text: "hello".into(),
            font_size: 20.0,
            line_height: crate::text::shaping::line_height(20.0),
            max_width: None,
            runs: Vec::new(),
            font: Some(std::sync::Arc::from(named)),
        });
        let generation = shaping.font_generation();
        assert_ne!(generation, painter.font_generation(), "the painter should now be behind");

        painter.sync(&shaping.font_chain_data(), generation);
        let chain = painter.named_fonts(named).expect("the named family should lead a chain of its own");
        assert_ne!(chain[0], painter.fonts()[0], "and that chain leads with a face of its own");
        assert_eq!(painter.font_generation(), generation);
    }

    /// femtovg's `add_shared_font_with_index` is a `SlotMap::insert`: it mints a new `FontId` every
    /// call and never dedups by bytes. A `sync` that re-registered the whole list would re-parse
    /// every face, strand the previous `Font` entries for the life of the surface, and hand back
    /// different ids each time -- so the declared chain's ids must survive a sync untouched.
    #[test]
    fn syncing_a_new_family_does_not_re_register_the_faces_femtovg_already_holds() {
        let Some(instance) = init_headless_egl(400, 60) else { return };
        let (declared, named) = ("Noto Sans", "Noto Sans Mono");
        if !crate::text::fonts::fc_match_available()
            || !crate::text::fonts::family_installed(declared)
            || !crate::text::fonts::family_installed(named)
        {
            eprintln!("skip: need two installed families to tell apart");
            return;
        }
        let shaping = ShapingHandle::spawn();
        shaping.set_chain(&[declared.to_string()]);
        let Some(mut painter) = text_painter(&instance, &shaping, 400, 60) else { return };
        let before: Vec<_> = painter.fonts().to_vec();

        shaping.ensure_family(&std::sync::Arc::from(named));
        painter.sync(&shaping.font_chain_data(), shaping.font_generation());

        assert_eq!(painter.fonts(), &before[..], "the declared chain's ids must not be re-minted");

        // And a second sync over the same faces mints nothing new either.
        let after_first = painter.named_fonts(named).map(<[_]>::to_vec);
        painter.sync(&shaping.font_chain_data(), painter.font_generation() + 1);
        assert_eq!(painter.fonts(), &before[..]);
        assert_eq!(painter.named_fonts(named).map(<[_]>::to_vec), after_first);
    }

    /// A name nothing on the system answers gets no chain, so `chain_for` hands the node the
    /// declared one -- a typo draws the text in the wrong face, never as nothing.
    #[test]
    fn a_family_nothing_answers_gets_no_chain_of_its_own() {
        let Some(instance) = init_headless_egl(400, 60) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 400, 60) else { return };
        shaping.shape(ShapeRequest {
            text: "hello".into(),
            font_size: 20.0,
            line_height: crate::text::shaping::line_height(20.0),
            max_width: None,
            runs: Vec::new(),
            font: Some(std::sync::Arc::from("ZZ No Such Family 9184")),
        });
        painter.sync(&shaping.font_chain_data(), shaping.font_generation());
        assert_eq!(painter.named_fonts("ZZ No Such Family 9184"), None);
        assert!(!painter.fonts().is_empty(), "and the declared chain is still there to draw with");
    }

    /// A `text` node's content wider than the box layout gave it must stop at that box's edge, not
    /// paint over whatever sits to its right. The live MPRIS-title-through-two-cells bug the doc
    /// comment describes.
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
                    text { content = "Obelisk Shell Renderer Overflow", font_size = 24, foreground = "#FFFFFFFF" },
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

    /// Regression test: a 1px border at a fractional position must land on exactly one physical
    /// pixel row, not blur across two. `padding.top = 10.3` puts the bordered rect's absolute y at
    /// a fractional offset -- unsnapped, femtovg's own antialiasing fills part of row 10 and part
    /// of row 11 at partial coverage instead of one row at full coverage.
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

    /// A `row`/`column`/`rect` container clips its children just as much as a `text` node clips its
    /// glyphs. A `row` whose children overflow is the same defect as an overflowing `text`, not a
    /// separate case. This is the non-text half of that claim: a child rect explicitly larger than
    /// its parent must not paint past the parent's own box.
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

    /// A `width = "Fill"` circle in a tweening cell lands a rounding error narrower than its
    /// height on some frames. That box took the vertical-cap branch, whose hair-length straight run
    /// folded the fill fan over the whole square; the wider case never did, which is why it showed
    /// on some frames and not others.
    #[test]
    fn a_box_a_hair_narrower_than_tall_is_still_a_circle() {
        let Some(instance) = init_headless_egl(64, 48) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 48) else { return };
        let white = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
        for (name, w) in [("a hair narrower", 31.999_998), ("a hair wider", 32.000_004), ("square", 32.0)] {
            let canvas = painter.canvas_mut();
            canvas.clear_rect(0, 0, 64, 48, Color::rgbaf(0.0, 0.0, 0.0, 1.0));
            fill_rect(canvas, LogicalRect { x: 8.0, y: 8.0, width: w, height: 32.0 }, 17.0, white);
            canvas.flush();
            assert_eq!(pixel_at(canvas, 9, 9), (0, 0, 0, 255), "{name}: the corner outside the circle stays black");
            assert_eq!(pixel_at(canvas, 24, 24), (255, 255, 255, 255), "{name}: the centre is filled");
        }
    }

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
    /// ADR-0149: `scale` paints a node bigger without moving its layout box. A 16px white square
    /// centred in a 64x48 black panel, scaled 2x, covers 32px around the same centre.
    #[test]
    fn a_scaled_node_paints_about_its_origin_and_its_layout_box_is_unchanged() {
        let Some(instance) = init_headless_egl(64, 48) else { return };
        let lua = Lua::new();
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 48) else { return };
        let root = resolved_surface(
            &lua,
            r##"return panel { id = "bar", width = 64, height = 48, background = "#000000ff", child = rect {
                width = 16, height = 16, margin = { left = 24, top = 16 }, background = "#ffffffff", scale = 2 } }"##,
            LogicalSize { width: 64.0, height: 48.0 },
        );
        assert_eq!(root.children[0].rect.width, 16.0, "the solver never sees the scale");
        assert_eq!(root.children[0].transform.scale, (2.0, 2.0));
        paint_tree(&mut painter, &mut ImageCache::new(), &root, 1.0);
        let canvas = painter.canvas_mut();
        assert_eq!(pixel_at(canvas, 18, 10), (255, 255, 255, 255), "inside the painted 16..48 x 8..40 box");
        assert_eq!(pixel_at(canvas, 45, 38), (255, 255, 255, 255));
        assert_eq!(pixel_at(canvas, 12, 10), (0, 0, 0, 255), "outside it");
        assert_eq!(pixel_at(canvas, 32, 24), (255, 255, 255, 255), "the centre stays put");
    }

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
