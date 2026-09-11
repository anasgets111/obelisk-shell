//! Runs a config-supplied fragment shader over an `image` node's two transition endpoints
//! (ADR-0184), which is what makes a wipe, a disc or a pixelate a config's to write rather than a
//! name this engine has to ship.
//!
//! The reference Quickshell config keeps its six `wp_*.frag` files in the *user's* config
//! directory. Quickshell ships `ShaderEffect`; the shaders are the user's. So a `Wipe` arm in a
//! Rust match would be the mistake ADR-0055 already ruled on for the wallpaper itself, one layer
//! down. This module is that ruling applied to effects: the engine owns compiling, binding and
//! restoring, and owns nothing about what the pixels do.
//!
//! # What the engine draws around the shader
//!
//! femtovg batches; a shader run has to happen *between* its draws, not beside them. So each run
//! flushes the canvas, captures the GL state it is about to change, draws one quad, and puts every
//! captured value back. It never binds framebuffer zero: it draws into whatever target `run` is
//! already on, which is how a dissolve inside `draw_clipped`'s offscreen still composites through
//! its parent's rounded mask.
//!
//! # Failure
//!
//! A shader that will not compile or link is reported once per revision and that path is refused
//! from then on, which drops the node back to `layout::paint`'s cross-dissolve for the rest of
//! the run. That is a real fallback rather than a snap, and it is the reason the dissolve was built
//! before this (ADR-0181).
//!
//! A shader that compiles and draws something ugly draws it: the engine cannot tell intent from
//! mistake. A shader that hangs the GPU hangs the session, and nothing here promises otherwise --
//! this is the config's own code, at the same trust level as the `process.run` it can already call,
//! with a worse failure mode and no containment claimed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, ImageId};
use glow::HasContext;

use crate::layout::node;
use crate::text::snap::{LogicalRect, PhysicalRect};

/// Prepended to every config shader, and the whole of the contract a shader writes against
/// (ADR-0184). Kept here rather than asked of the config so that a shader is a mask and not a
/// pile of boilerplate, and so the sampling convention cannot drift between two of them.
///
/// `obelisk_from`/`obelisk_to` return the endpoint's colour at a node-space coordinate, or
/// `u_fill` outside the picture -- the engine has already applied each endpoint's `fit`, so a
/// shader never repeats that arithmetic and never disagrees with how the same image draws
/// ordinarily.
const PRELUDE: &str = r#"#version 300 es
precision highp float;
precision highp sampler2D;

in vec2 v_uv;
out vec4 fragColor;

uniform sampler2D u_from;
uniform sampler2D u_to;
uniform float u_progress;
uniform vec2 u_size;
uniform vec4 u_from_rect;
uniform vec4 u_to_rect;
uniform vec4 u_fill;
uniform float obelisk_opacity;

vec4 obelisk_sample(sampler2D tex, vec4 rect, vec2 uv) {
    vec2 local = (uv - rect.xy) / rect.zw;
    if (local.x < 0.0 || local.x > 1.0 || local.y < 0.0 || local.y > 1.0) {
        return u_fill;
    }
    return texture(tex, local);
}

vec4 obelisk_from(vec2 uv) { return obelisk_sample(u_from, u_from_rect, uv); }
vec4 obelisk_to(vec2 uv) { return obelisk_sample(u_to, u_to_rect, uv); }

#define main obelisk_effect
#line 1
"#;

/// Appended after the config's source, and the reason a config writes `void main()` and still
/// cannot get the node's `opacity` wrong (ADR-0184). The `#define` above renamed its entry point,
/// so this is the real one: it runs the effect, then applies the opacity the node inherited to the
/// premultiplied result -- all four channels, once, where the engine can guarantee it.
///
/// Documenting that rule and leaving a config to obey it would be the promise-without-mechanism
/// this branch has already made twice.
const EPILOGUE: &str = r#"
#undef main
void main() {
    obelisk_effect();
    fragColor *= obelisk_opacity;
}
"#;

/// The engine's own effect: a straight cross-dissolve, and what a `transition` with no `shader`
/// runs (ADR-0186). Written exactly as a config would write it, against the same contract and
/// through the same [`assemble`], so there is one sampling convention and not two.
///
/// This replaced two source-over draws, which composed correctly only for opaque endpoints at full
/// opacity: `from` at `alpha` with `to` at `alpha * progress` over it leaves `alpha=0.5`,
/// `progress=0.5` showing 0.625 opacity where 0.5 is right, and the surface's ground through the
/// middle. Textures upload premultiplied (ADR-0184), so mixing them *is* the composite, and the
/// epilogue applies the node's opacity once afterwards.
const FADE: &str = r#"
void main() {
    fragColor = mix(obelisk_from(v_uv), obelisk_to(v_uv), u_progress);
}
"#;

/// One quad covering the node's box, in clip space, with the node-space `v_uv` the prelude reads.
/// The engine owns the vertex stage so that a config shader is a fragment and nothing else.
const VERTEX: &str = r#"#version 300 es
precision highp float;
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_uv;
out vec2 v_uv;
void main() {
    v_uv = a_uv;
    gl_Position = vec4(a_pos, 0.0, 1.0);
}
"#;

/// A compiled config shader and the uniform locations it turned out to have.
struct Program {
    program: glow::Program,
    from: Option<glow::UniformLocation>,
    to: Option<glow::UniformLocation>,
    progress: Option<glow::UniformLocation>,
    size: Option<glow::UniformLocation>,
    from_rect: Option<glow::UniformLocation>,
    to_rect: Option<glow::UniformLocation>,
    fill: Option<glow::UniformLocation>,
    opacity: Option<glow::UniformLocation>,
    /// Every other active uniform, by the name a config's `params` key has to match. A shader may
    /// declare one and never use it, in which case the compiler drops it and it is absent here;
    /// supplying a value for it is not an error.
    params: HashMap<String, glow::UniformLocation>,
}

/// What one run draws: the two endpoint textures, where each sits inside the node's box after its
/// `fit`, and how far across the run is.
pub struct Run<'a> {
    pub from: ImageId,
    pub to: ImageId,
    /// Fitted rects, absolute like `rect`; the prelude reads them as fractions of it.
    pub from_rect: LogicalRect,
    pub to_rect: LogicalRect,
    /// The node's box, absolute in the surface.
    pub rect: LogicalRect,
    /// The node's paint-only affine (ADR-0149), about its own origin, or `None` for no transform.
    /// Applied to the quad's corners here rather than by femtovg, which never sees this draw.
    pub transform: Option<node::Affine>,
    /// The scissor this draw is under, absolute in the surface, already intersected down the
    /// ancestor chain. femtovg scissors paths through a uniform its own shader reads, so a quad
    /// this stage draws is clipped by nothing unless this stage clips it.
    pub clip: PhysicalRect,
    /// The framebuffer being drawn into: its size, and where its top-left sits in surface
    /// coordinates. Both are needed inside `draw_clipped`'s offscreen, whose origin is the clip's
    /// corner rather than the screen's.
    pub target_size: (f32, f32),
    pub target_origin: (f32, f32),
    /// What the node inherited, applied by [`EPILOGUE`] after the config's `main`.
    pub opacity: f32,
    pub progress: f32,
    pub params: &'a [(String, f32)],
}

/// Compiled config shaders for this GL context, and the one quad they all draw.
#[derive(Default)]
pub struct ShaderStage {
    /// Keyed by path *and* file version, so editing a shader recompiles it and correcting one that
    /// would not build clears the refusal. `None` is a version that failed and has been reported;
    /// it is not tried again until the bytes change. Keying on the path alone left a config editing
    /// its own effect looking at a program compiled minutes ago, with no way to reach it short of
    /// restarting the shell.
    programs: HashMap<PathBuf, (crate::image::FileVersion, Option<Program>)>,
    /// [`FADE`], compiled on first use. `None` until then; `Some(None)` if the engine's own shader
    /// would not build, which is not tried again -- the same shape `programs` uses, and the point
    /// where a node drops back to the two-draw approximation.
    fade: Option<Option<Program>>,
    /// Positions and texture coordinates for one quad, rewritten per draw because the corners carry
    /// the node's transform.
    quad: Option<(glow::VertexArray, glow::Buffer)>,
    vertex: Option<glow::Shader>,
}

impl ShaderStage {
    /// Draws `run` with the config shader at `effect`, or with the engine's own [`FADE`] when
    /// `effect` is `None` *or* the config's would not build. Answers `false` for anything that
    /// stops it, which leaves the caller's two-draw approximation to take the frame.
    ///
    /// A config shader that fails falls back to `FADE` rather than to the caller: an effect that
    /// stops compiling is a reason to lose the effect, not a reason to lose correct compositing
    /// (ADR-0186).
    ///
    /// # Safety
    ///
    /// `gl` must be the context current on this thread, and `canvas` the femtovg canvas sharing
    /// it. Neither can be checked here, and every GL object this stage owns belongs to that one
    /// context.
    pub unsafe fn draw(
        &mut self,
        gl: &glow::Context,
        canvas: &mut Canvas<OpenGl>,
        effect: Option<&Path>,
        run: &Run,
    ) -> bool {
        let (Ok(from), Ok(to)) = (canvas.get_native_texture(run.from), canvas.get_native_texture(run.to)) else {
            return false;
        };
        // Before the program is borrowed, because this needs `self` mutably and that borrow would
        // still be live.
        // SAFETY: caller's contract.
        let Some(quad) = (unsafe { self.ensure_quad(gl) }) else { return false };
        // SAFETY: caller's contract.
        let chosen = effect.filter(|path| unsafe { self.ensure_program(gl, path) });
        let program = match chosen {
            Some(path) => self.programs.get(path).and_then(|(_, program)| program.as_ref()),
            None => {
                // SAFETY: caller's contract.
                unsafe { self.ensure_fade(gl) };
                self.fade.as_ref().and_then(Option::as_ref)
            }
        };
        let Some(program) = program else { return false };

        // Everything femtovg has recorded so far has to reach the framebuffer before this quad
        // does; `gl.flush()` would not, because the queue this drains is femtovg's own, on the CPU.
        canvas.flush();

        // SAFETY: caller's contract, and every value read here is put back below before femtovg
        // records another command. The restore runs on the failing path too: a run that gives up
        // inside `render` has already changed state.
        unsafe {
            let saved = State::capture(gl);
            let drew = Self::render(gl, program, quad, from, to, run);
            saved.restore(gl);
            drew
        }
    }

    /// Compiles [`FADE`] on first use. Reported once if it will not build, which would mean the
    /// engine's own shader is broken rather than a config's.
    ///
    /// # Safety
    ///
    /// The context is current.
    unsafe fn ensure_fade(&mut self, gl: &glow::Context) {
        if self.fade.is_some() {
            return;
        }
        // SAFETY: caller's contract.
        let built = unsafe { self.build(gl, Path::new("<engine cross-dissolve>"), FADE) };
        self.fade = Some(built);
    }

    /// The shared quad, created on first use.
    ///
    /// # Safety
    ///
    /// The context is current.
    unsafe fn ensure_quad(&mut self, gl: &glow::Context) -> Option<(glow::VertexArray, glow::Buffer)> {
        if self.quad.is_none() {
            // SAFETY: caller's contract.
            self.quad = Some(unsafe { make_quad(gl) }?);
        }
        self.quad
    }

    /// Compiles and links `path` if this version of it is not already known. Answers whether a
    /// program is ready.
    ///
    /// # Safety
    ///
    /// The context is current.
    unsafe fn ensure_program(&mut self, gl: &glow::Context, path: &Path) -> bool {
        let version = crate::image::FileVersion::read(path);
        if let Some((known, program)) = self.programs.get(path)
            && *known == version
        {
            return program.is_some();
        }
        // A superseded program is deleted here, while the context is current, rather than left for
        // teardown: a config iterating on an effect would otherwise leak one program per save.
        if let Some((_, Some(stale))) = self.programs.remove(path) {
            // SAFETY: caller's contract.
            unsafe { gl.delete_program(stale.program) };
        }
        let built = match std::fs::read_to_string(path) {
            // SAFETY: caller's contract.
            Ok(source) => unsafe { self.build(gl, path, &source) },
            Err(err) => {
                eprintln!("[obelisk-renderer] shader: {}: {err}", path.display());
                None
            }
        };
        let ready = built.is_some();
        self.programs.insert(path.to_path_buf(), (version, built));
        ready
    }

    /// # Safety
    ///
    /// The context is current.
    unsafe fn build(&mut self, gl: &glow::Context, path: &Path, source: &str) -> Option<Program> {
        // SAFETY: caller's contract. Every object created here is deleted on the paths that fail
        // after creating it, and by `destroy` at teardown.
        unsafe {
            let vertex = match self.vertex {
                Some(vertex) => vertex,
                None => {
                    let vertex = compile(gl, glow::VERTEX_SHADER, VERTEX, Path::new("<engine vertex stage>"))?;
                    self.vertex = Some(vertex);
                    vertex
                }
            };
            let fragment = compile(gl, glow::FRAGMENT_SHADER, &assemble(source), path)?;
            let Ok(program) = gl.create_program() else {
                gl.delete_shader(fragment);
                return None;
            };
            gl.attach_shader(program, vertex);
            gl.attach_shader(program, fragment);
            gl.link_program(program);
            gl.detach_shader(program, vertex);
            gl.detach_shader(program, fragment);
            gl.delete_shader(fragment);
            if !gl.get_program_link_status(program) {
                eprintln!("[obelisk-renderer] shader: {}: {}", path.display(), gl.get_program_info_log(program));
                gl.delete_program(program);
                return None;
            }

            let named = |name: &str| gl.get_uniform_location(program, name);
            let mut params = HashMap::new();
            for index in 0..gl.get_active_uniforms(program) {
                let Some(uniform) = gl.get_active_uniform(program, index) else { continue };
                if uniform.name.starts_with("u_") || uniform.name.starts_with("obelisk_") {
                    continue;
                }
                if uniform.utype != glow::FLOAT {
                    eprintln!(
                        "[obelisk-renderer] shader: {}: `{}` is not a `float`, and `params` carries numbers only",
                        path.display(),
                        uniform.name
                    );
                    gl.delete_program(program);
                    return None;
                }
                if let Some(location) = gl.get_uniform_location(program, &uniform.name) {
                    params.insert(uniform.name, location);
                }
            }
            Some(Program {
                program,
                from: named("u_from"),
                to: named("u_to"),
                progress: named("u_progress"),
                size: named("u_size"),
                from_rect: named("u_from_rect"),
                to_rect: named("u_to_rect"),
                fill: named("u_fill"),
                opacity: named("obelisk_opacity"),
                params,
            })
        }
    }

    /// Draws the chosen program's quad. Takes no `self`: the program arrives by reference, which
    /// is what lets the caller pick between a config's and the engine's own without cloning either
    /// or holding a borrow of the map across the draw.
    ///
    /// # Safety
    ///
    /// The context is current and its state has been captured by the caller.
    unsafe fn render(
        gl: &glow::Context,
        program: &Program,
        quad: (glow::VertexArray, glow::Buffer),
        from: glow::Texture,
        to: glow::Texture,
        run: &Run,
    ) -> bool {
        let (vao, buffer) = quad;
        let (Some(width), Some(height)) = (positive(run.rect.width), positive(run.rect.height)) else {
            return false;
        };

        // SAFETY: caller's contract.
        unsafe {
            gl.use_program(Some(program.program));
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(buffer));
            let vertices = quad_corners(run.rect, (width, height), run.transform, run.target_size, run.target_origin);
            let bytes = std::slice::from_raw_parts(vertices.as_ptr().cast::<u8>(), std::mem::size_of_val(&vertices));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STREAM_DRAW);

            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(from));
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(to));
            gl.uniform_1_i32(program.from.as_ref(), 0);
            gl.uniform_1_i32(program.to.as_ref(), 1);

            gl.uniform_1_f32(program.progress.as_ref(), run.progress);
            gl.uniform_1_f32(program.opacity.as_ref(), run.opacity);
            gl.uniform_2_f32(program.size.as_ref(), width, height);
            let relative = |inner: LogicalRect| {
                [
                    (inner.x - run.rect.x) / width,
                    (inner.y - run.rect.y) / height,
                    inner.width / width,
                    inner.height / height,
                ]
            };
            gl.uniform_4_f32_slice(program.from_rect.as_ref(), &relative(run.from_rect));
            gl.uniform_4_f32_slice(program.to_rect.as_ref(), &relative(run.to_rect));
            gl.uniform_4_f32(program.fill.as_ref(), 0.0, 0.0, 0.0, 0.0);

            // Every param the program has, not only the ones this node supplied. A uniform holds
            // its value in the program, and two nodes sharing one shader would otherwise inherit
            // each other's: the one that omits `softness` would get whatever the other last set.
            for (name, location) in &program.params {
                let value = run.params.iter().find(|(param, _)| param == name).map_or(0.0, |(_, value)| *value);
                gl.uniform_1_f32(Some(location), value);
            }

            // Premultiplied source-over, and `FUNC_ADD` set rather than inherited: nothing in
            // femtovg ever sets an equation, so it is whatever GL was left at.
            gl.enable(glow::BLEND);
            gl.blend_equation_separate(glow::FUNC_ADD, glow::FUNC_ADD);
            gl.blend_func_separate(glow::ONE, glow::ONE_MINUS_SRC_ALPHA, glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::STENCIL_TEST);
            gl.disable(glow::CULL_FACE);

            // The ancestor clip, which femtovg applies to its own paths through a uniform this
            // quad never reads. GL scissors from the bottom left, so the box is flipped in the
            // target it is being drawn into.
            let (target_width, target_height) = run.target_size;
            let (left, right) = (run.clip.x0 as f32 - run.target_origin.0, run.clip.x1 as f32 - run.target_origin.0);
            let (top, bottom) = (run.clip.y0 as f32 - run.target_origin.1, run.clip.y1 as f32 - run.target_origin.1);
            let x = left.max(0.0);
            let y = (target_height - bottom).max(0.0);
            let scissor_width = right.min(target_width) - x;
            let scissor_height = bottom.min(target_height) - top.max(0.0);
            if scissor_width <= 0.0 || scissor_height <= 0.0 {
                return false;
            }
            gl.enable(glow::SCISSOR_TEST);
            gl.scissor(x as i32, y as i32, scissor_width as i32, scissor_height as i32);

            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
        true
    }

    /// Frees every GL object while the context is still current. A context that has gone away takes
    /// its objects with it, so this is for an orderly teardown and not for recovery.
    ///
    /// # Safety
    ///
    /// `gl` must be the context these were created on, current on this thread.
    pub unsafe fn destroy(&mut self, gl: &glow::Context) {
        // SAFETY: caller's contract.
        unsafe {
            for (_, program) in self.programs.values() {
                if let Some(program) = program {
                    gl.delete_program(program.program);
                }
            }
            if let Some(Some(fade)) = self.fade.take() {
                gl.delete_program(fade.program);
            }
            if let Some(vertex) = self.vertex.take() {
                gl.delete_shader(vertex);
            }
            if let Some((vao, buffer)) = self.quad.take() {
                gl.delete_vertex_array(vao);
                gl.delete_buffer(buffer);
            }
        }
        self.programs.clear();
    }
}

/// The whole source a config's file is compiled as: the contract, then the file at line 1, then the
/// engine's own `main`. Split out so a test can read it without a GL context.
fn assemble(source: &str) -> String {
    format!("{PRELUDE}{source}{EPILOGUE}")
}

/// A finite, positive dimension, or `None`. Sizes below one are legitimate and must not be clamped
/// up to it; zero and negative ones have no quad to draw.
fn positive(value: f32) -> Option<f32> {
    (value.is_finite() && value > 0.0).then_some(value)
}

/// The quad's four corners as `(clip-space position, unit-square coordinate)` pairs, with the
/// node's transform applied and the target's own origin subtracted. Pure, and takes what it reads
/// rather than the whole run, so the placement is testable without a canvas to make an `ImageId`.
fn quad_corners(
    rect: LogicalRect,
    size: (f32, f32),
    transform: Option<node::Affine>,
    target_size: (f32, f32),
    target_origin: (f32, f32),
) -> [[f32; 4]; 4] {
    let (target_width, target_height) = (target_size.0.max(1.0), target_size.1.max(1.0));
    let corner = |u: f32, v: f32| {
        let (mut x, mut y) = (rect.x + u * size.0, rect.y + v * size.1);
        if let Some(matrix) = transform {
            // femtovg's `Transform2D` order, which is what `Draw::Transformed` hands the canvas.
            (x, y) = node::apply_affine(matrix, x, y);
        }
        // Into the target, then into clip space, with y flipped: GL's origin is the bottom left.
        let ndc_x = (x - target_origin.0) / target_width * 2.0 - 1.0;
        let ndc_y = 1.0 - (y - target_origin.1) / target_height * 2.0;
        [ndc_x, ndc_y, u, v]
    };
    [corner(0.0, 0.0), corner(1.0, 0.0), corner(0.0, 1.0), corner(1.0, 1.0)]
}

/// # Safety
///
/// The context is current.
unsafe fn compile(gl: &glow::Context, kind: u32, source: &str, path: &Path) -> Option<glow::Shader> {
    // SAFETY: caller's contract.
    unsafe {
        let shader = gl.create_shader(kind).ok()?;
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
        if gl.get_shader_compile_status(shader) {
            return Some(shader);
        }
        // `#line 1` ends the prelude, so a reported line number is the config's own.
        eprintln!("[obelisk-renderer] shader: {}: {}", path.display(), gl.get_shader_info_log(shader));
        gl.delete_shader(shader);
        None
    }
}

/// The vertex array and buffer every run reuses. The contents are rewritten per draw, because the
/// corners carry the node's box, target and transform; only the layout is fixed here.
///
/// # Safety
///
/// The context is current.
unsafe fn make_quad(gl: &glow::Context) -> Option<(glow::VertexArray, glow::Buffer)> {
    // SAFETY: caller's contract.
    unsafe {
        let vao = gl.create_vertex_array().ok()?;
        let Ok(buffer) = gl.create_buffer() else {
            gl.delete_vertex_array(vao);
            return None;
        };
        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(buffer));
        // Four vertices of `[x, y, u, v]`, matching the two `layout(location = ...)` inputs the
        // engine's vertex stage declares -- bound explicitly rather than left to the linker.
        let stride = 4 * std::mem::size_of::<f32>() as i32;
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, stride, 0);
        gl.enable_vertex_attrib_array(1);
        gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, stride, 2 * std::mem::size_of::<f32>() as i32);
        Some((vao, buffer))
    }
}

/// Every piece of GL state one run changes, read before and put back after.
///
/// femtovg keeps its own idea of what is bound and re-binds lazily, so anything left changed here
/// is a draw it makes later against state it never set. The framebuffer bindings are deliberately
/// absent: this draws into whichever target is already current, and never touches that. The scissor
/// box *is* present, because this stage sets one: femtovg clips its own paths through a uniform its
/// shader reads, so a quad drawn here is clipped by nothing unless GL scissors it.
///
/// The colour mask is absent too, and not by oversight. femtovg masks colour writes while it lays
/// down a stencil and puts the mask back within the same operation (`renderer/opengl.rs`), so after
/// the flush this run begins with, all four channels are on. Nothing here changes it, so there is
/// nothing to put back -- and `glow` has no four-channel read for it, so querying would mean
/// storing one channel's answer and restoring it to all four.
struct State {
    program: Option<glow::Program>,
    scissor_box: [i32; 4],
    vertex_array: Option<glow::VertexArray>,
    array_buffer: Option<glow::Buffer>,
    active_texture: u32,
    texture_0: Option<glow::Texture>,
    texture_1: Option<glow::Texture>,
    blend: bool,
    blend_src_rgb: i32,
    blend_dst_rgb: i32,
    blend_src_alpha: i32,
    blend_dst_alpha: i32,
    blend_equation_rgb: i32,
    blend_equation_alpha: i32,
    depth_test: bool,
    stencil_test: bool,
    cull_face: bool,
    scissor_test: bool,
}

impl State {
    /// # Safety
    ///
    /// The context is current.
    unsafe fn capture(gl: &glow::Context) -> Self {
        // SAFETY: caller's contract. Every query below is a plain `glGet` on the current context.
        unsafe {
            let name = |slot: u32| {
                let raw = gl.get_parameter_i32(slot);
                (raw != 0).then_some(raw as u32)
            };
            let active_texture = gl.get_parameter_i32(glow::ACTIVE_TEXTURE) as u32;
            gl.active_texture(glow::TEXTURE0);
            let texture_0 = name(glow::TEXTURE_BINDING_2D).map(|raw| glow::NativeTexture(raw.try_into().unwrap()));
            gl.active_texture(glow::TEXTURE1);
            let texture_1 = name(glow::TEXTURE_BINDING_2D).map(|raw| glow::NativeTexture(raw.try_into().unwrap()));
            gl.active_texture(active_texture);
            let mut scissor_box = [0; 4];
            gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut scissor_box);
            Self {
                program: name(glow::CURRENT_PROGRAM).map(|raw| glow::NativeProgram(raw.try_into().unwrap())),
                scissor_box,
                vertex_array: name(glow::VERTEX_ARRAY_BINDING)
                    .map(|raw| glow::NativeVertexArray(raw.try_into().unwrap())),
                array_buffer: name(glow::ARRAY_BUFFER_BINDING).map(|raw| glow::NativeBuffer(raw.try_into().unwrap())),
                active_texture,
                texture_0,
                texture_1,
                blend: gl.is_enabled(glow::BLEND),
                blend_src_rgb: gl.get_parameter_i32(glow::BLEND_SRC_RGB),
                blend_dst_rgb: gl.get_parameter_i32(glow::BLEND_DST_RGB),
                blend_src_alpha: gl.get_parameter_i32(glow::BLEND_SRC_ALPHA),
                blend_dst_alpha: gl.get_parameter_i32(glow::BLEND_DST_ALPHA),
                blend_equation_rgb: gl.get_parameter_i32(glow::BLEND_EQUATION_RGB),
                blend_equation_alpha: gl.get_parameter_i32(glow::BLEND_EQUATION_ALPHA),
                depth_test: gl.is_enabled(glow::DEPTH_TEST),
                stencil_test: gl.is_enabled(glow::STENCIL_TEST),
                cull_face: gl.is_enabled(glow::CULL_FACE),
                scissor_test: gl.is_enabled(glow::SCISSOR_TEST),
            }
        }
    }

    /// # Safety
    ///
    /// The context is current and is the one [`State::capture`] read.
    unsafe fn restore(self, gl: &glow::Context) {
        // SAFETY: caller's contract.
        unsafe {
            gl.use_program(self.program);
            gl.bind_vertex_array(self.vertex_array);
            gl.bind_buffer(glow::ARRAY_BUFFER, self.array_buffer);
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, self.texture_1);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, self.texture_0);
            gl.active_texture(self.active_texture);
            let toggle = |enabled: bool, slot: u32| {
                if enabled {
                    gl.enable(slot);
                } else {
                    gl.disable(slot);
                }
            };
            toggle(self.blend, glow::BLEND);
            gl.blend_func_separate(
                self.blend_src_rgb as u32,
                self.blend_dst_rgb as u32,
                self.blend_src_alpha as u32,
                self.blend_dst_alpha as u32,
            );
            gl.blend_equation_separate(self.blend_equation_rgb as u32, self.blend_equation_alpha as u32);
            toggle(self.depth_test, glow::DEPTH_TEST);
            toggle(self.stencil_test, glow::STENCIL_TEST);
            toggle(self.cull_face, glow::CULL_FACE);
            toggle(self.scissor_test, glow::SCISSOR_TEST);
            let [x, y, width, height] = self.scissor_box;
            gl.scissor(x, y, width, height);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0184. The source a config writes is wrapped, not trusted: its `main` is renamed so the
    /// engine's own can apply the node's opacity after it, and `#line 1` puts the config's first
    /// line at line 1 so a compiler error names a line the config can find.
    #[test]
    fn a_config_shader_is_wrapped_so_the_engine_owns_the_last_operation() {
        let assembled = assemble("void main() { fragColor = obelisk_to(v_uv); }\n");

        let effect = assembled.find("#define main obelisk_effect").expect("the rename");
        let user = assembled.find("void main() { fragColor").expect("the config's own source");
        let undef = assembled.find("#undef main").expect("the rename ends");
        let engine = assembled.rfind("fragColor *= obelisk_opacity;").expect("the engine's last word");
        assert!(effect < user, "the rename has to reach the config's `main`");
        assert!(user < undef, "and has to stop before the engine writes its own");
        assert!(undef < engine);

        // The line directive is the last thing before the config's source, so its line numbers are
        // its own however long the prelude grows.
        let prelude = &assembled[..user];
        assert!(prelude.trim_end().ends_with("#line 1"), "got: {:?}", prelude.trim_end().rsplit('\n').next());
    }

    /// A box smaller than one logical pixel is legitimate -- a tween passes through it -- and must
    /// not be clamped up to one, which would misplace the quad. Zero and below have nothing to
    /// draw.
    #[test]
    fn a_fractional_box_is_kept_and_an_empty_one_is_refused() {
        assert_eq!(positive(0.25), Some(0.25));
        assert_eq!(positive(1920.0), Some(1920.0));
        assert_eq!(positive(0.0), None);
        assert_eq!(positive(-4.0), None);
        assert_eq!(positive(f32::NAN), None);
        assert_eq!(positive(f32::INFINITY), None);
    }

    /// The quad is placed in the target it is drawn into, not on the screen: inside a rounded
    /// clip's offscreen the origin is the clip's corner, and a node's paint-only affine moves the
    /// corners because femtovg never sees this draw to apply it.
    #[test]
    fn the_quad_lands_in_its_target_and_carries_the_nodes_transform() {
        let rect = LogicalRect { x: 10.0, y: 20.0, width: 100.0, height: 50.0 };
        let close = |got: &[f32], want: [f32; 2]| {
            assert!((got[0] - want[0]).abs() < 1e-5 && (got[1] - want[1]).abs() < 1e-5, "got {got:?}, want {want:?}");
        };

        // Top-left of a 100x50 box at (10, 20) in a 200x100 target: x = 10/200*2-1 = -0.9,
        // y = 1 - 20/100*2 = 0.6. Bottom-right: (110, 70) -> (0.1, -0.4).
        let corners = quad_corners(rect, (100.0, 50.0), None, (200.0, 100.0), (0.0, 0.0));
        close(&corners[0][..2], [-0.9, 0.6]);
        assert_eq!(corners[0][2..], [0.0, 0.0], "and the unit square travels with it");
        close(&corners[3][..2], [0.1, -0.4]);

        // The same node inside an offscreen whose top-left is the node's own corner sits at that
        // target's origin. Placed from absolute coordinates it landed elsewhere entirely.
        let inner = quad_corners(rect, (100.0, 50.0), None, (100.0, 50.0), (10.0, 20.0));
        close(&inner[0][..2], [-1.0, 1.0]);
        close(&inner[3][..2], [1.0, -1.0]);

        // A translate by (100, 0) moves every corner right by a full half of a 200-wide target.
        let moved =
            quad_corners(rect, (100.0, 50.0), Some([1.0, 0.0, 0.0, 1.0, 100.0, 0.0]), (200.0, 100.0), (0.0, 0.0));
        close(&moved[0][..2], [0.1, 0.6]);
    }
}
