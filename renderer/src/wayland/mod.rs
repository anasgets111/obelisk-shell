pub mod egl;

use std::error::Error;
use std::ffi::c_void;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::dispatch2::Dispatch2;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::presentation_time::{PresentTime, PresentationTimeHandler, PresentationTimeState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::{delegate_registry, registry_handlers};
use khronos_egl::Surface as EglSurface;
use wayland_client::globals::{registry_queue_init, GlobalList};
use wayland_client::protocol::{wl_output, wl_seat, wl_surface};
use wayland_client::{Connection, Proxy, QueueHandle, WEnum};
use wayland_egl::WlEglSurface;
use wayland_protocols::wp::presentation_time::client::wp_presentation_feedback;
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3::{self, ZwpTextInputManagerV3},
    zwp_text_input_v3::{self, ZwpTextInputV3},
};
use shared::{PresentationEvidence, ReadySignal, RendererFrame, SecureSubmit, SupervisorFrame, Zeroize};

use crate::socket::RendererClient;
use crate::text::atlas::TextPainter;
use crate::text::shaping::{ShapeRequest, ShapingHandle};
use crate::text::snap::LogicalRect;

/// The three static surfaces from ADR-0007 / build-steps.md Phase 3, point 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SurfaceRole {
    MainBar,
    OverlayCanvas,
    WallpaperLayer,
}

impl SurfaceRole {
    fn label(self) -> &'static str {
        match self {
            SurfaceRole::MainBar => "main_bar",
            SurfaceRole::OverlayCanvas => "overlay_canvas",
            SurfaceRole::WallpaperLayer => "wallpaper_layer",
        }
    }
}

/// ponytail: no real per-`textfield` focus/attribution exists yet (see `App::bind_text_input`'s
/// own `ponytail` comment) -- every `secure_submit` completed this way is attributed to this
/// fixed placeholder until real focus wiring can read the specific `textfield` node's own
/// `secure_submit = { capability, action }` table (`oblisk-idl-api-specs.md` § 5.2 item 8), which
/// is now a lookup in the retained `Scene` this thread owns rather than a cross-thread hop
/// (docs/adr/0039; build-steps.md Phase 21 item 3).
const PLACEHOLDER_SECURE_SUBMIT_CAPABILITY: &str = "unknown";
const PLACEHOLDER_SECURE_SUBMIT_ACTION: &str = "unknown";

/// A live window surface bound to the shared EGL context, once its first configure
/// has arrived. Holds the native window alongside the EGL surface: per wayland-egl's
/// contract, `WlEglSurface` must outlive the EGL surface built from it -- fields are
/// declared in the order Rust drops them (top to bottom), so `egl_surface` goes first.
struct BoundSurface {
    #[allow(dead_code)]
    egl_surface: EglSurface,
    #[allow(dead_code)]
    native_window: WlEglSurface,
}

/// Logs an EGL/Wayland bind-time failure in a consistent shape across `bind_and_clear`'s
/// fallible steps.
fn log_bind_failure(role: SurfaceRole, stage: &str, err: impl std::fmt::Display) {
    eprintln!("[oblisk-renderer] {}: {stage} failed: {err}", role.label());
}

struct TrackedSurface {
    role: SurfaceRole,
    layer: LayerSurface,
    bound: Option<BoundSurface>,
    /// § 15's "surface_id": `role.label()` for `main_bar`/`overlay_canvas`,
    /// `"wallpaper_layer@{name}"` per wallpaper instance -- resolved once at creation time (see
    /// [`create_wallpaper_layers`](App::create_wallpaper_layers)), not recomputed later.
    surface_id: String,
    /// Set once this surface's null buffer has been committed (PBA candidate mode only, § 15.2
    /// points 2-3). Irrelevant, always `false`, outside candidate mode.
    null_buffered: bool,
    /// The most recent `configure` event's size, remembered so [`App::activate_draw`] has a real
    /// size to bind its EGL window surface to -- in candidate mode, the first configure doesn't
    /// bind EGL at all (see [`App::bind_and_clear`]), so this is the only place that size
    /// survives until `ActivateDraw` arrives.
    configured_size: (u32, u32),
}

pub struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor_state: CompositorState,
    seat_state: SeatState,
    layer_shell: LayerShell,
    egl: egl::EglState,
    gl: Option<glow::Context>,
    /// The one `ShapingHandle` for the whole process; `client` holds a clone of it, so
    /// content-sizing and painting share one worker thread and one `FontSystem`
    /// (docs/adr/0023 item 8, closed by docs/adr/0039 decision 3).
    shaping: ShapingHandle,
    text_painter: Option<TextPainter>,
    /// The Lua VM, the `Loader`, the retained `Scene`, the live signals, and the reload
    /// bookkeeping, all owned by this dispatch state rather than by a separate thread
    /// (docs/adr/0039). `mlua::Lua` is `!Send`, so `App` is `!Send` too -- fine, since
    /// `wayland-client` puts no `Send` bound on the dispatch state.
    client: RendererClient,
    surfaces: Vec<TrackedSurface>,
    exit: bool,
    /// `OBLISK_PBA_CANDIDATE` is set (build-steps.md Phase 14, § 15.2) -- read once in [`run`]
    /// and stored here rather than re-reading the env var on every configure event.
    is_pba_candidate: bool,
    /// Set once [`App::maybe_send_ready_signal`] has sent `ReadySignal` -- a one-time signal,
    /// never resent even if a later spurious configure re-triggers the check.
    ready_signal_sent: bool,
    /// Every frame this thread sends the Supervisor goes here; the socket thread's `pump` drains
    /// it and writes each one to the wire (docs/adr/0039). `UnboundedSender::send` is
    /// synchronous and non-blocking, so it's safe to call from inside a `Dispatch` callback.
    outbound_tx: tokio::sync::mpsc::UnboundedSender<RendererFrame>,
    /// This Renderer's own generation id, stamped into every `SecureSubmit` it writes
    /// (build-steps.md Phase 15 item 2) -- read once in `main` from `OBLISK_GENERATION_ID`.
    generation_id: u32,
    presentation_time: PresentationTimeState,
    /// Cloned once in [`run`] so [`App::activate_draw`] (called from the poll loop, not a
    /// `Dispatch` callback) can still request `wp_presentation_feedback` -- `QueueHandle` is a
    /// cheap, `Clone`, reference-counted handle.
    queue_handle: QueueHandle<App>,
    /// The `ActivateDraw` nonce currently being drawn, if any -- tags every
    /// `wp_presentation_feedback` `presented` event reported while it's in flight. PBA only
    /// drives one handshake at a time (docs/adr/0025 item 5), so one field, not a per-surface
    /// map, is enough.
    active_nonce: Option<u64>,
    /// Kept alive for the object's whole lifetime (never read again after
    /// [`App::bind_text_input`] enables it) -- dropping the proxy would destroy the protocol
    /// object, same reasoning `BoundSurface`'s `#[allow(dead_code)]` fields already document.
    #[allow(dead_code)]
    text_input: Option<ZwpTextInputV3>,
    text_input_pending: TextInputPending,
    /// Accumulates committed `wp-text-input-v3` edits until a protocol-native `ACTION_SUBMIT`
    /// completes them (build-steps.md Phase 15 item 2; ADR-0005/ADR-0009/ADR-0027) -- never
    /// surfaced to Lua.
    secure_buffer: shared::SecureBuffer,
}

/// The Renderer's main thread: Wayland dispatch, EGL, and (since docs/adr/0039) the Lua VM, the
/// retained `Scene`, and the live signals. `inbound_rx` carries `SupervisorFrame`s decoded by the
/// socket thread; `outbound_tx` carries every frame this thread sends back.
pub fn run(
    generation_id: u32,
    inbound_rx: std::sync::mpsc::Receiver<SupervisorFrame>,
    outbound_tx: tokio::sync::mpsc::UnboundedSender<RendererFrame>,
) -> Result<(), Box<dyn Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init::<App>(&conn)?;
    let qh = event_queue.handle();

    let compositor_state = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let output_state = OutputState::new(&globals, &qh);
    let seat_state = SeatState::new(&globals, &qh);
    let registry_state = RegistryState::new(&globals);
    // Stable protocol, no `staging`/`unstable` Cargo feature needed -- `PresentationTimeState::
    // bind` tolerates a compositor that doesn't advertise it (later `feedback()` calls fail with
    // `GlobalError::MissingGlobal` instead of failing this whole bind).
    let presentation_time = PresentationTimeState::bind(&globals, &qh);

    let egl_state = egl::init(conn.backend().display_ptr() as *mut c_void)?;

    let is_pba_candidate = std::env::var("OBLISK_PBA_CANDIDATE").is_ok();

    // One `ShapingHandle` for the process: `App` keeps this one, `RendererClient` gets a clone
    // (docs/adr/0039 decision 3). `Loader::new()` runs inside `start`, on this thread, because
    // `mlua::Lua` is `!Send` -- the move this whole phase is about.
    let shaping = ShapingHandle::spawn();
    let client = RendererClient::start(shaping.clone(), outbound_tx.clone(), generation_id)?;

    let mut app = App {
        registry_state,
        output_state,
        compositor_state,
        seat_state,
        layer_shell,
        egl: egl_state,
        gl: None,
        shaping,
        text_painter: None,
        client,
        surfaces: Vec::new(),
        exit: false,
        is_pba_candidate,
        ready_signal_sent: false,
        outbound_tx,
        generation_id,
        presentation_time,
        queue_handle: qh.clone(),
        active_nonce: None,
        text_input: None,
        text_input_pending: TextInputPending::default(),
        secure_buffer: shared::SecureBuffer::new(),
    };

    // Outputs (and the seat) arrive as a burst of registry + wl_seat/wl_output events after
    // binding; two roundtrips is enough to have both the full initial output list (before we
    // create one wallpaper_layer surface per monitor) and the seat `bind_text_input` needs.
    event_queue.roundtrip(&mut app)?;
    event_queue.roundtrip(&mut app)?;

    // `oblisk-supervisor-services-dbus.md` § 15.2's Candidate order, which on one thread is just
    // the order of these statements: evaluate shell.lua, bind the layer-shell surfaces, commit
    // null buffers (in `bind_and_clear`'s candidate branch), signal ready
    // (`maybe_send_ready_signal`).
    //
    // ponytail: this runs inside the PBA ready window -- no layer surface exists until it
    // returns, so `maybe_send_ready_signal` cannot fire until after this call, and the
    // Supervisor's `ready_timeout` is 2s (`supervisor/src/main.rs`'s `PBA_TIMINGS`). The first
    // `text` node's shaping blocks on `ShapingHandle::shape` until the worker's `FontSystem::new()`
    // finishes, eating into that same 2s budget. The § 15.2 ordering (evaluate before bind) is
    // required, not incidental, so this stays sequential -- not a fix, just the accepted cost.
    app.client.run_startup_evaluation();

    app.create_main_bar(&qh);
    app.create_overlay_canvas(&qh);
    app.create_wallpaper_layers(&qh);
    app.bind_text_input(&globals, &qh);

    // Replaces `event_queue.blocking_dispatch(&mut app)?` (used through Phase 13): a real
    // Wayland event might not arrive for a long time after `ActivateDraw` is sent, since nothing
    // else happens on these mostly-static surfaces once staged -- this loop also checks
    // `inbound_rx` on a bounded latency instead of blocking indefinitely on the Wayland
    // connection's fd alone. The existing immediate-draw behavior on first configure (non-
    // candidate mode) is unaffected -- it still happens synchronously inside the `configure`
    // handler, which `dispatch_pending` still calls.
    loop {
        event_queue.dispatch_pending(&mut app)?;
        if app.exit {
            break;
        }
        // Drain, not one-per-pass: every `SupervisorFrame` reaches this thread through this
        // channel now (docs/adr/0039), so a burst of `StateSnapshot` pushes must not be spread
        // one per 15ms poll tick the way a lone `ActivateDraw` nonce could afford to be.
        //
        // ponytail: `try_recv`'s `Err` collapses `Disconnected` and `Empty` alike, so a dead
        // socket thread (pump exited, see `crate::socket`) reads the same as an idle one -- this
        // process spins its 15ms poll forever with a live but unreachable shell. Predates this
        // diff, but the blast radius is wider now that the VM and scene live on this same
        // surviving thread (docs/adr/0039). Not fixed here: adding an exit path on disconnect is
        // a policy change outside this refactor's scope.
        // One turn is three ordered stages: drain everything, re-resolve once, then draw. An
        // `ActivateDraw` nonce is therefore collected here rather than serviced in place. Drawing
        // in the loop body painted whatever layout the scene happened to hold at that instant, so
        // a `StateSnapshot` and an `ActivateDraw` arriving in the same drain -- snapshot first,
        // which is exactly the PBA hydrate-then-activate order (§ 15.2) -- hydrated the signal,
        // painted the *pre-push* layout, and only then re-resolved. Nothing requests another draw
        // after a re-resolve (that gating is build-steps.md Phase 19 items 6 through 11), so that
        // stale frame was the one the Supervisor accepted as presentation evidence.
        //
        // A `Vec`, not a single nonce: two `ActivateDraw`s in one drain would be unusual, but each
        // one owes the Supervisor its own `PresentationEvidence` per surface, so none may be
        // dropped by coalescing.
        let mut draw_nonces: Vec<u64> = Vec::new();
        while let Ok(frame) = inbound_rx.try_recv() {
            if let Some(nonce) = app.client.handle_frame(frame) {
                draw_nonces.push(nonce);
            }
            if app.exit {
                break;
            }
        }
        if app.exit {
            break;
        }
        // Once per turn, after the drain above has emptied `inbound_rx` -- not inside that
        // `while` loop's body (ADR-0044 decision 2). A burst of `StateSnapshot` pushes marks the
        // dirty flag repeatedly while draining, but `DirtyFlag::take` only reports it once, so
        // this coalesces the whole burst into a single `Scene::apply` per poll turn instead of one
        // per pushed frame -- and, per the comment above, it lands before this turn's draw.
        app.client.re_resolve_if_dirty();
        for nonce in draw_nonces {
            app.activate_draw(nonce);
            if app.exit {
                break;
            }
        }
        if app.exit {
            break;
        }
        event_queue.flush()?;
        if let Some(guard) = event_queue.prepare_read() {
            let fd = guard.connection_fd();
            let mut fds = [nix::poll::PollFd::new(fd, nix::poll::PollFlags::POLLIN)];
            // 15ms: bounded latency for inbound_rx, irrelevant next to PBA's second-scale
            // ready/evidence timeouts (supervisor/src/main.rs's `PBA_TIMINGS`).
            if matches!(nix::poll::poll(&mut fds, nix::poll::PollTimeout::from(15u16)), Ok(n) if n > 0) {
                guard.read()?;
            }
            // guard drops here either way; if nothing was read, dispatch_pending above simply
            // finds nothing new next iteration.
        }
    }

    Ok(())
}

/// One `zwp_text_input_v3`'s double-buffered pending edit -- the protocol's own rule that
/// `preedit_string`/`commit_string`/`delete_surrounding_text`/`action` events only take effect on
/// the next `done` (`done`'s own description: "This event replaces the current state with the
/// pending state"). Kept separate from the real `Dispatch2` impl below so it's unit-testable
/// without a live Wayland connection -- this file has no headless Wayland test harness (see
/// `wallpaper_surface_id`'s doc comment for the same reasoning).
#[derive(Default)]
struct TextInputPending {
    commit: Option<String>,
    submit: bool,
}

impl TextInputPending {
    fn on_commit_string(&mut self, text: Option<String>) {
        self.commit = text;
    }

    fn on_action_submit(&mut self) {
        self.submit = true;
    }

    /// `done`: takes the completed edit and resets pending state for the next cycle.
    fn take_done(&mut self) -> TextInputEdit {
        TextInputEdit { commit: self.commit.take(), submit: std::mem::take(&mut self.submit) }
    }
}

/// One completed `done` cycle's edit (ADR-0027's `TextInputEdit` shape, borrowed from
/// Noctalia's `TextInputEdit`) -- this slice only threads `commit`/`submit` through to
/// `shared::SecureBuffer`; see [`apply_edit`]'s doc comment for `preedit`/delete-surrounding-text.
struct TextInputEdit {
    commit: Option<String>,
    submit: bool,
}

/// Applies one completed edit to `buffer` (ADR-0027, ADR-0009's diff-based edit model): only
/// `commit_string`'s final text is pushed. `preedit_string`'s transient composition text is
/// deliberately never pushed here -- real IME composition revises or clears a preedit before it
/// commits, and an append-only `SecureBuffer` has no way to "undo" a stale revision; pushing
/// every intermediate preedit would corrupt the secret with duplicated composition fragments.
///
/// ponytail: `delete_surrounding_text` (backspace) isn't applied either -- `shared::SecureBuffer`
/// (ADR-0014) is append-only by design, with zero production callers (and so no truncate method)
/// until this slice. Upgrade path: give `SecureBuffer` a zeroize-on-shrink truncate method
/// (tested the same allocator-hook way `push_str`'s growth path already is in
/// `shared/tests/secure_buffer_growth_zeroizes.rs`) and apply `before_length`/`after_length` here
/// once it exists.
///
/// Returns whether this edit's `action` was `ACTION_SUBMIT`.
fn apply_edit(buffer: &mut shared::SecureBuffer, edit: TextInputEdit) -> bool {
    if let Some(commit) = edit.commit {
        buffer.push_str(&commit);
    }
    edit.submit
}

/// One `wallpaper_layer` instance's surface_id: `"wallpaper_layer@{name}"` when the compositor
/// reports a real output name, `"wallpaper_layer@output-{index}"` (a stable positional fallback)
/// when it doesn't. Pure so it's directly unit-testable -- `wayland/mod.rs` otherwise has no
/// test seam (Wayland-protocol-integration code with no headless test harness in this repo).
fn wallpaper_surface_id(name: Option<&str>, index: usize) -> String {
    match name {
        Some(name) => format!("wallpaper_layer@{name}"),
        None => format!("wallpaper_layer@output-{index}"),
    }
}

/// Parameters for [`App::spawn_layer`]; bundled so the helper stays under clippy's
/// argument-count limit while still taking each of the three surfaces' divergent bits.
struct LayerSpec<'a> {
    layer_type: Layer,
    name: &'a str,
    output: Option<&'a wl_output::WlOutput>,
    anchor: Anchor,
    size: (u32, u32),
    exclusive_zone: i32,
    /// `None` for `main_bar`/`wallpaper_layer` -- neither ever hosts interactive content.
    /// `overlay_canvas` needs `OnDemand`: `zwp_text_input_v3`'s `enter` event (and so
    /// `bind_text_input`'s whole `secure_submit` path) never fires on a surface the
    /// compositor won't hand keyboard focus to in the first place, confirmed live against a
    /// real compositor (`niri msg layers` reported `Keyboard interactivity: none` on all
    /// three before this field existed). `OnDemand`, not `Exclusive`: the shell shouldn't
    /// steal focus from whatever's behind it just for existing -- ADR-0027's still-open
    /// per-`textfield` focus/hit-test system is what will eventually decide *when* to ask
    /// for it, this only makes asking possible.
    keyboard_interactivity: KeyboardInteractivity,
}

impl App {
    /// Creates and configures (but does not commit) a layer-shell surface. The three
    /// static surfaces share this skeleton; each call site handles its own divergent
    /// setup (overlay's input region, wallpaper's per-output loop) before committing.
    fn spawn_layer(&mut self, qh: &QueueHandle<App>, spec: LayerSpec) -> LayerSurface {
        let surface = self.compositor_state.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            spec.layer_type,
            Some(spec.name),
            spec.output,
        );
        layer.set_anchor(spec.anchor);
        layer.set_size(spec.size.0, spec.size.1);
        layer.set_exclusive_zone(spec.exclusive_zone);
        layer.set_keyboard_interactivity(spec.keyboard_interactivity);
        layer
    }

    fn create_main_bar(&mut self, qh: &QueueHandle<App>) {
        let layer = self.spawn_layer(
            qh,
            LayerSpec {
                layer_type: Layer::Top,
                name: "oblisk-main-bar",
                output: None,
                anchor: Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
                size: (0, 32),
                exclusive_zone: 32,
                keyboard_interactivity: KeyboardInteractivity::None,
            },
        );
        layer.commit();

        self.surfaces.push(TrackedSurface {
            role: SurfaceRole::MainBar,
            layer,
            bound: None,
            surface_id: SurfaceRole::MainBar.label().to_string(),
            null_buffered: false,
            configured_size: (0, 0),
        });
    }

    fn create_overlay_canvas(&mut self, qh: &QueueHandle<App>) {
        let layer = self.spawn_layer(
            qh,
            LayerSpec {
                layer_type: Layer::Overlay,
                name: "oblisk-overlay-canvas",
                output: None,
                anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                size: (0, 0),
                exclusive_zone: 0,
                keyboard_interactivity: KeyboardInteractivity::OnDemand,
            },
        );

        // build-steps.md Phase 3, point 4: commit an empty input region on boot so
        // clicks pass through to windows below until a Lua-authored overlay child
        // claims a bounding box (later phase). The region is destroyed immediately
        // after the request; wl_surface.set_input_region copies its contents.
        // overlay_canvas's entire purpose is this click-through guarantee, so a
        // failure here is as fatal as an EGL bind failure, not a silent no-op.
        match Region::new(&self.compositor_state) {
            Ok(region) => layer.set_input_region(Some(region.wl_region())),
            Err(e) => {
                log_bind_failure(SurfaceRole::OverlayCanvas, "wl_compositor::create_region", e);
                self.exit = true;
                return;
            }
        }

        layer.commit();

        self.surfaces.push(TrackedSurface {
            role: SurfaceRole::OverlayCanvas,
            layer,
            bound: None,
            surface_id: SurfaceRole::OverlayCanvas.label().to_string(),
            null_buffered: false,
            configured_size: (0, 0),
        });
    }

    fn create_wallpaper_layers(&mut self, qh: &QueueHandle<App>) {
        // ponytail: fixed two-roundtrip output snapshot (see run()), no dynamic
        // add/remove -- an output that appears after boot never gets a wallpaper_layer,
        // and a removed output's surface is never torn down. Upgrade path: wire
        // OutputHandler::new_output/output_destroyed to spawn/despawn wallpaper_layer
        // surfaces as outputs come and go instead of enumerating once here.
        for (index, output) in self.output_state.outputs().collect::<Vec<_>>().into_iter().enumerate() {
            let name = self.output_state.info(&output).and_then(|info| info.name);
            let surface_id = wallpaper_surface_id(name.as_deref(), index);
            let layer = self.spawn_layer(
                qh,
                LayerSpec {
                    layer_type: Layer::Background,
                    name: "oblisk-wallpaper",
                    output: Some(&output),
                    anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                    size: (0, 0),
                    exclusive_zone: 0,
                    keyboard_interactivity: KeyboardInteractivity::None,
                },
            );
            layer.commit();

            self.surfaces.push(TrackedSurface {
                role: SurfaceRole::WallpaperLayer,
                layer,
                bound: None,
                surface_id,
                null_buffered: false,
                configured_size: (0, 0),
            });
        }
    }

    /// First configure for a surface: bind its wl_egl_window to a real EGL window
    /// surface against the shared context, make it current, and prove the pipeline
    /// is live with one clear + swap. No draw loop -- that's Phase 4.
    ///
    /// PBA candidate mode (`self.is_pba_candidate`, build-steps.md Phase 14, § 15.2 points 2-3)
    /// branches here instead: a first configure commits a null buffer directly on the raw
    /// `wl_surface` rather than binding EGL at all -- the Candidate stays invisible, occupying
    /// zero on-screen coordinates, until [`App::activate_draw`] does the real EGL bind later.
    /// Non-candidate mode (today's existing behavior) is entirely unaffected by this branch.
    fn bind_and_clear(&mut self, layer: &LayerSurface, width: u32, height: u32) {
        let Some(tracked) = self.surfaces.iter_mut().find(|s| &s.layer == layer) else {
            return;
        };

        if self.is_pba_candidate {
            tracked.configured_size = (width, height);
            if !tracked.null_buffered {
                // verified against wayland_client::protocol::wl_surface::WlSurface's generated
                // API: `attach(&self, buffer: Option<&wl_buffer::WlBuffer>, x: i32, y: i32)`,
                // `commit(&self)`.
                tracked.layer.wl_surface().attach(None, 0, 0);
                tracked.layer.wl_surface().commit();
                tracked.null_buffered = true;
            }
            self.maybe_send_ready_signal();
            return;
        }

        let width = width.max(1) as i32;
        let height = height.max(1) as i32;

        if let Some(egl_surface) = tracked.bound.as_ref().map(|b| b.egl_surface) {
            // Repeat configure (e.g. a resize) on an already-bound surface. The
            // wl_egl_window/EGL surface were created once and don't need recreating,
            // but only main_bar has per-frame state (the FemtoVG canvas) that must
            // track the new size -- the other two surfaces have nothing left to do.
            if tracked.role != SurfaceRole::MainBar {
                return;
            }
            let role = tracked.role;

            // Another surface's own bind_and_clear may have made a different EGL
            // surface current on this thread since main_bar's last draw -- the
            // context is shared across all three surfaces, so it must be
            // re-established here rather than assumed still current.
            if let Err(e) = self.egl.instance.make_current(
                self.egl.display,
                Some(egl_surface),
                Some(egl_surface),
                Some(self.egl.context),
            ) {
                log_bind_failure(role, "eglMakeCurrent", e);
                self.exit = true;
                return;
            }

            if !draw_main_bar_proof_text(&self.shaping, &self.egl, &mut self.text_painter, width, height) {
                self.exit = true;
                return;
            }

            if let Err(e) = self.egl.instance.swap_buffers(self.egl.display, egl_surface) {
                log_bind_failure(role, "eglSwapBuffers", e);
                self.exit = true;
                return;
            }

            return;
        }

        let native_window = match WlEglSurface::new(layer.wl_surface().id(), width, height) {
            Ok(w) => w,
            Err(e) => {
                log_bind_failure(tracked.role, "WlEglSurface::new", e);
                self.exit = true;
                return;
            }
        };

        // SAFETY: `native_window.ptr()` is a live `wl_egl_window*` just constructed above by
        // `WlEglSurface::new`, matching `self.egl.display`/`self.egl.config`'s own platform --
        // exactly the handle `eglCreateWindowSurface` requires.
        let egl_surface = unsafe {
            self.egl.instance.create_window_surface(
                self.egl.display,
                self.egl.config,
                native_window.ptr() as *mut c_void,
                None,
            )
        };
        let egl_surface = match egl_surface {
            Ok(s) => s,
            Err(e) => {
                log_bind_failure(tracked.role, "eglCreateWindowSurface", e);
                self.exit = true;
                return;
            }
        };

        if let Err(e) = self.egl.instance.make_current(
            self.egl.display,
            Some(egl_surface),
            Some(egl_surface),
            Some(self.egl.context),
        ) {
            log_bind_failure(tracked.role, "eglMakeCurrent", e);
            self.exit = true;
            return;
        }

        // SAFETY: `glow::Context::from_loader_function`'s contract is that a GL context is
        // current on this thread for the lifetime of the returned `Context` -- guaranteed here
        // by the `eglMakeCurrent` call directly above, on this same single-threaded dispatch
        // loop, with no other context switch between the two.
        let gl = self.gl.get_or_insert_with(|| unsafe {
            glow::Context::from_loader_function(|s| {
                self.egl
                    .instance
                    .get_proc_address(s)
                    .map_or(std::ptr::null(), |f| f as *const c_void)
            })
        });

        // SAFETY: every `glow::HasContext` method call requires a current GL context matching
        // `gl`'s own loader -- the `eglMakeCurrent` above is that context, and it's the only one
        // live on this thread.
        unsafe {
            use glow::HasContext;
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }

        // Phase 4 integration proof, main_bar only: shape+draw one static string to
        // prove the cosmic-text/FemtoVG pipeline is live end to end. No draw loop or
        // Lua-driven content -- that's a future scene-graph phase.
        if tracked.role == SurfaceRole::MainBar {
            // Free function, not a `&mut self` method: `tracked` is still borrowed from
            // `self.surfaces` here, so this takes the three disjoint fields it actually
            // needs directly, rather than the whole `self` a method call would require.
            // Safe to build/use a FemtoVG `Canvas` here: dispatch is single-threaded, the
            // `eglMakeCurrent` a few lines above is the only context switch on this
            // thread, and this branch only ever runs for `main_bar`, so the context
            // that's current at this point is always the one `text_painter` was built
            // against -- no other surface's `bind_and_clear` can interleave here.
            if !draw_main_bar_proof_text(&self.shaping, &self.egl, &mut self.text_painter, width, height) {
                self.exit = true;
                return;
            }
        }

        if let Err(e) = self.egl.instance.swap_buffers(self.egl.display, egl_surface) {
            log_bind_failure(tracked.role, "eglSwapBuffers", e);
            self.exit = true;
            return;
        }

        eprintln!(
            "[oblisk-renderer] {} up: {width}x{height}, EGL context current, buffer cleared+swapped",
            tracked.role.label()
        );

        tracked.bound = Some(BoundSurface {
            egl_surface,
            native_window,
        });
    }

    /// § 15.2 points 2-3: once every tracked surface has committed its null buffer, computes
    /// the full surface_id list (in `self.surfaces`' order) and queues it once as a
    /// `ReadySignal`. A no-op if it's already been sent, or if some surface hasn't staged yet --
    /// called on every candidate-mode configure, since any of them might be the one that
    /// completes the set.
    fn maybe_send_ready_signal(&mut self) {
        if self.ready_signal_sent || !self.surfaces.iter().all(|s| s.null_buffered) {
            return;
        }
        self.ready_signal_sent = true;
        let surfaces = self.surfaces.iter().map(|s| s.surface_id.clone()).collect();
        if let Err(e) = self.outbound_tx.send(RendererFrame::ReadySignal(ReadySignal { surfaces })) {
            eprintln!("[oblisk-renderer] failed to queue ReadySignal for the socket thread: {e}");
        }
    }

    /// § 15.3: draws every tracked surface's first real frame in response to `ActivateDraw`,
    /// requesting `wp_presentation_feedback` for each. `nonce` is remembered as `active_nonce`
    /// so the later `presented` callback (this file's `PresentationTimeHandler` impl) knows
    /// which handshake attempt to tag its evidence with.
    fn activate_draw(&mut self, nonce: u64) {
        self.active_nonce = Some(nonce);
        for index in 0..self.surfaces.len() {
            self.activate_draw_one(index, nonce);
            if self.exit {
                return;
            }
        }
        // Promotion completes this process's PBA handshake -- from now on it behaves like an
        // ordinary (non-candidate) authoritative generation for the rest of its life, so a later
        // `configure` (resize, output change, a duplicate ack round trip -- all routine on a live
        // compositor) must fall through to `bind_and_clear`'s ordinary EGL-bind/resize path
        // instead of re-taking the null-buffer-staging branch forever (Correctness review: that
        // branch no-ops once `null_buffered` is already `true`, permanently disabling resize).
        // `activate_draw_one` already populated `tracked.bound` in the exact shape the
        // non-candidate path expects, so flipping this alone is enough -- no other state needs
        // adjusting.
        self.is_pba_candidate = false;
    }

    /// One tracked surface's `ActivateDraw` response: the same EGL-bind-and-clear (main_bar
    /// also draws the proof text) `bind_and_clear`'s non-candidate first-configure path does,
    /// plus a `wp_presentation_feedback` request placed immediately before `swap_buffers` so it
    /// associates with the commit `swap_buffers` performs. Indexes into `self.surfaces` rather
    /// than holding a `&mut TrackedSurface` across the whole body -- this needs `&mut self` for
    /// EGL/GL state and `self.text_painter` at several points, which a held borrow of one
    /// surface would conflict with (same reasoning `draw_main_bar_proof_text` already documents
    /// for the analogous first-configure path).
    fn activate_draw_one(&mut self, index: usize, nonce: u64) {
        let role = self.surfaces[index].role;
        let (width, height) = self.surfaces[index].configured_size;
        let width = width.max(1) as i32;
        let height = height.max(1) as i32;

        let native_window = match WlEglSurface::new(self.surfaces[index].layer.wl_surface().id(), width, height) {
            Ok(w) => w,
            Err(e) => {
                log_bind_failure(role, "WlEglSurface::new", e);
                self.exit = true;
                return;
            }
        };

        // SAFETY: `native_window.ptr()` is a live `wl_egl_window*` just constructed above by
        // `WlEglSurface::new`, matching `self.egl.display`/`self.egl.config`'s own platform --
        // exactly the handle `eglCreateWindowSurface` requires.
        let egl_surface = unsafe {
            self.egl.instance.create_window_surface(self.egl.display, self.egl.config, native_window.ptr() as *mut c_void, None)
        };
        let egl_surface = match egl_surface {
            Ok(s) => s,
            Err(e) => {
                log_bind_failure(role, "eglCreateWindowSurface", e);
                self.exit = true;
                return;
            }
        };

        if let Err(e) = self.egl.instance.make_current(self.egl.display, Some(egl_surface), Some(egl_surface), Some(self.egl.context)) {
            log_bind_failure(role, "eglMakeCurrent", e);
            self.exit = true;
            return;
        }

        // SAFETY: `glow::Context::from_loader_function`'s contract is that a GL context is
        // current on this thread for the lifetime of the returned `Context` -- guaranteed here
        // by the `eglMakeCurrent` call directly above, on this same single-threaded dispatch
        // loop, with no other context switch between the two.
        let gl = self.gl.get_or_insert_with(|| unsafe {
            glow::Context::from_loader_function(|s| self.egl.instance.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void))
        });

        // SAFETY: every `glow::HasContext` method call requires a current GL context matching
        // `gl`'s own loader -- the `eglMakeCurrent` above is that context, and it's the only one
        // live on this thread.
        unsafe {
            use glow::HasContext;
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }

        if role == SurfaceRole::MainBar && !draw_main_bar_proof_text(&self.shaping, &self.egl, &mut self.text_painter, width, height) {
            self.exit = true;
            return;
        }

        // § 15.3 point 2: request presentation feedback before swap_buffers, so the request
        // associates with the commit swap_buffers performs -- confirmed against
        // `wayland-client-0.31.15`'s own client examples' placement convention; verify with
        // `WAYLAND_DEBUG=1` during a manual smoke test that `feedback` appears on the wire
        // before the corresponding `commit`.
        if let Err(e) = self.presentation_time.feedback(self.surfaces[index].layer.wl_surface(), &self.queue_handle) {
            // Not fatal to the whole candidate -- the Supervisor's evidence_timeout is what
            // catches a surface that never presents (docs/adr/0025 item 6); don't invent a
            // second failure-reporting path here.
            log_bind_failure(role, "wp_presentation::feedback", e);
        }

        if let Err(e) = self.egl.instance.swap_buffers(self.egl.display, egl_surface) {
            log_bind_failure(role, "eglSwapBuffers", e);
            self.exit = true;
            return;
        }

        eprintln!(
            "[oblisk-renderer] {} activated: {width}x{height}, presentation feedback requested (nonce={nonce})",
            self.surfaces[index].role.label()
        );

        self.surfaces[index].bound = Some(BoundSurface { egl_surface, native_window });
    }

    /// Binds `zwp_text_input_manager_v3`, creates one `zwp_text_input_v3` for this seat, and
    /// `enable()`s it unconditionally (build-steps.md Phase 15 item 2; ADR-0009, ADR-0027).
    ///
    /// ponytail: no real per-`textfield` keyboard-focus system exists yet -- this codebase has
    /// none anywhere, matching `create_wallpaper_layers`'s own disclosed-simplification style.
    /// A real implementation would call `enable()`/`disable()` in response to the compositor's
    /// own `enter`/`leave` events on the specific surface a Lua-authored `textfield` occupies
    /// (`overlay_canvas`, since that's where interactive scene content will eventually live),
    /// and would thread that `textfield` node's own `secure_submit = { capability, action }`
    /// table through to `finish_secure_submit` instead of the fixed placeholder there. Upgrade
    /// path: build that focus system (a click/hit-test pass over the retained `Scene`, plus
    /// wiring `enter`/`leave` here) once a later phase needs it -- out of scope for this slice.
    ///
    /// A compositor with no seat, or no `zwp_text_input_manager_v3` global, leaves `text_input`
    /// `None` -- logged, not fatal, same tolerance `PresentationTimeState::bind` already applies
    /// to an optional protocol.
    ///
    /// ponytail: ADR-0009 names the Wayland-thread protocol owner a `TextInputService`; this
    /// slice inlines its state (`text_input`, `text_input_pending`, `secure_buffer`) directly
    /// onto `App` instead of extracting that type, since `App` is still this thread's only
    /// `Dispatch` target. Upgrade path: pull these fields and their `Dispatch2` impls into a real
    /// `TextInputService` type once a second consumer (or the focus system above) needs to own
    /// it independently of `App`.
    fn bind_text_input(&mut self, globals: &GlobalList, qh: &QueueHandle<App>) {
        let Some(seat) = self.seat_state.seats().next() else {
            eprintln!("[oblisk-renderer] no wl_seat advertised; secure_submit textfields will never receive input");
            return;
        };

        // Bound up to v2, not v1: the `action` event this module's whole submit detection
        // depends on (`handle_text_input_event`'s `Action::Submit` match) is `since="2"` in the
        // protocol XML. `GlobalList::bind` negotiates `min(advertised, version_end)`, so binding
        // `1..=1` silently caps every object this manager creates at v1 -- a compositor would
        // never send `action` at all, and no `textfield` would ever submit.
        let manager = match globals.bind::<ZwpTextInputManagerV3, App, TextInputManagerData>(qh, 1..=2, TextInputManagerData) {
            Ok(manager) => manager,
            Err(e) => {
                log_bind_failure(SurfaceRole::OverlayCanvas, "zwp_text_input_manager_v3::bind", e);
                return;
            }
        };

        let text_input = manager.get_text_input(&seat, qh, TextInputData);
        // ADR-0005's amendment (ADR-0009): tell the compositor/IME this field is sensitive,
        // independent of and in addition to the Lua-boundary protection -- skips logging,
        // autocorrect, and clipboard-history capture on a well-behaved IME. Set unconditionally
        // rather than gated on a specific node's `mask_character`: this slice's only consumer of
        // `zwp_text_input_v3` at all is the secure_submit path (no generic `on_change` frontend
        // exists yet), so every text-input session bound here already is one.
        text_input.set_content_type(zwp_text_input_v3::ContentHint::SensitiveData, zwp_text_input_v3::ContentPurpose::Password);
        text_input.enable();
        text_input.commit();
        self.text_input = Some(text_input);
    }

    /// Dispatches one raw `zwp_text_input_v3` event (called from [`TextInputData`]'s
    /// [`Dispatch2`] impl below): accumulates `commit_string`/`action` into
    /// [`TextInputPending`], and on `done`, applies the completed edit and finalizes a
    /// `secure_submit` if it carried `ACTION_SUBMIT`.
    fn handle_text_input_event(&mut self, event: zwp_text_input_v3::Event) {
        match event {
            zwp_text_input_v3::Event::CommitString { text } => self.text_input_pending.on_commit_string(text),
            zwp_text_input_v3::Event::Action { action, .. } => {
                if matches!(action, WEnum::Value(zwp_text_input_v3::Action::Submit)) {
                    self.text_input_pending.on_action_submit();
                }
            }
            zwp_text_input_v3::Event::Done { .. } => {
                let edit = self.text_input_pending.take_done();
                if apply_edit(&mut self.secure_buffer, edit) {
                    self.finish_secure_submit();
                }
            }
            // `preedit_string`/`delete_surrounding_text`: acknowledged but not durably applied
            // to `secure_buffer` -- see `apply_edit`'s doc comment. `enter`/`leave`/`language`
            // don't affect the accumulated secret.
            _ => {}
        }
    }

    /// A completed `secure_submit`: builds the outgoing frame out of the accumulated buffer and
    /// queues it for the socket thread. [`secure_submit_frame`] both performs the one sanctioned
    /// read and leaves `self.secure_buffer` scrubbed and empty, ready for the next entry.
    fn finish_secure_submit(&mut self) {
        let frame = secure_submit_frame(
            self.generation_id,
            PLACEHOLDER_SECURE_SUBMIT_CAPABILITY,
            PLACEHOLDER_SECURE_SUBMIT_ACTION,
            &mut self.secure_buffer,
        );
        if let Err(e) = self.outbound_tx.send(frame) {
            eprintln!("[oblisk-renderer] failed to queue SecureSubmit for the socket thread: {e}");
        }
    }
}

/// Builds one `RendererFrame::SecureSubmit` out of `buffer` (build-steps.md Phase 15 item 2;
/// ADR-0005/ADR-0027).
///
/// The one sanctioned read (`expose_secret`) and the explicit `.zeroize()` of the source buffer
/// sit on adjacent lines here, so the accumulated secret stops existing the instant it has been
/// copied into the outgoing envelope -- not left to `Drop`, and not left live while the frame
/// travels to the socket thread. The frame's own plaintext copy is the socket thread's to scrub,
/// immediately after its wire write (`crate::socket`'s `pump`); that is as close to the write as
/// this side of the channel can get, and it is where the pre-ADR-0039 code did it too.
///
/// A free function, not a `&mut self` method, for the same reason `wallpaper_surface_id` and
/// `apply_edit` are: it makes the whole read/zeroize contract directly unit-testable, which
/// nothing involving a live `wl_surface` is.
fn secure_submit_frame(generation_id: u32, capability: &str, action: &str, buffer: &mut shared::SecureBuffer) -> RendererFrame {
    let frame = RendererFrame::SecureSubmit(SecureSubmit {
        generation_id,
        capability: capability.to_string(),
        action: action.to_string(),
        secret: buffer.expose_secret().to_vec(),
    });
    buffer.zeroize();
    frame
}

/// Zero-sized user-data marker for `zwp_text_input_v3`, following
/// `smithay_client_toolkit::globals::GlobalData`'s own convention (ADR-0009: a hand-written
/// `Dispatch` for text-input that slots into the same [`delegate_dispatch2!`] blanket every
/// other SCTK subsystem in this file already uses). `GlobalData` itself is SCTK's own foreign
/// type and can't be reused here -- orphan rules block implementing the foreign [`Dispatch2`]
/// trait for it against a foreign interface type this crate didn't define -- so this crate needs
/// its own marker types.
struct TextInputData;

impl Dispatch2<ZwpTextInputV3, App> for TextInputData {
    fn event(
        &self,
        state: &mut App,
        _proxy: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _conn: &Connection,
        _qh: &QueueHandle<App>,
    ) {
        state.handle_text_input_event(event);
    }
}

/// Marker for `zwp_text_input_manager_v3`, which never sends any events (see the XML: only
/// `destroy`/`get_text_input` requests, no `<event>`) -- generic over `D` since nothing here
/// touches `App` specifically, matching SCTK's own `GlobalData` impls for similar zero-event
/// managers.
struct TextInputManagerData;

impl<D> Dispatch2<ZwpTextInputManagerV3, D> for TextInputManagerData {
    fn event(
        &self,
        _state: &mut D,
        _proxy: &ZwpTextInputManagerV3,
        _event: zwp_text_input_manager_v3::Event,
        _conn: &Connection,
        _qh: &QueueHandle<D>,
    ) {
        // No `<event>` in this interface's XML at all -- the generated `Event` enum is
        // `#[non_exhaustive]` (future protocol versions might add one), not truly uninhabited,
        // so this can't be an empty `match event {}`; this can never actually fire at version 1.
        unreachable!("zwp_text_input_manager_v3 (version 1) has no events to dispatch")
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    // No pointer/keyboard/touch object is ever created from any capability -- `bind_text_input`
    // only needs the bare `wl_seat` itself to call `get_text_input(seat)` (ADR-0009: SCTK's
    // `seat` module is standard infrastructure here, not a design point).
    fn new_capability(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat, _capability: Capability) {}
    fn remove_capability(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat, _capability: Capability) {}

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}
}

impl PresentationTimeHandler for App {
    fn presentation_time_state(&mut self) -> &mut PresentationTimeState {
        &mut self.presentation_time
    }

    /// § 15.3 point 4: the compositor confirmed `surface`'s committed frame physically hit the
    /// screen. Queues a `shared::PresentationEvidence` frame for the socket thread to write.
    fn presented(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _feedback: &wp_presentation_feedback::WpPresentationFeedback,
        surface: &wl_surface::WlSurface,
        _outputs: Vec<wl_output::WlOutput>,
        _time: PresentTime,
        _refresh: u32,
        _seq: u64,
        _flags: WEnum<wp_presentation_feedback::Kind>,
    ) {
        let Some(nonce) = self.active_nonce else {
            eprintln!("[oblisk-renderer] presented event arrived with no active ActivateDraw nonce; dropping");
            return;
        };
        let Some(surface_id) = self.surface_id_for(surface).map(str::to_string) else {
            eprintln!("[oblisk-renderer] presented event for an untracked surface; dropping");
            return;
        };
        if let Err(e) = self.outbound_tx.send(RendererFrame::PresentationEvidence(PresentationEvidence { nonce, surface_id })) {
            eprintln!("[oblisk-renderer] failed to queue PresentationEvidence for the socket thread: {e}");
        }
    }

    /// The content update was never displayed. Logged only -- not a distinct fast-fail signal;
    /// the Supervisor's `evidence_timeout` is what catches this surface never presenting
    /// (docs/adr/0025 item 6). Deliberately does **not** queue any `PresentationEvidence`.
    fn discarded(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _feedback: &wp_presentation_feedback::WpPresentationFeedback,
        surface: &wl_surface::WlSurface,
    ) {
        let label = self.surface_id_for(surface).unwrap_or("<untracked surface>");
        eprintln!("[oblisk-renderer] presentation feedback discarded for {label}");
    }
}

impl App {
    /// Resolves a raw `wl_surface` (as handed back by a `wp_presentation_feedback` callback)
    /// to its `surface_id` -- shared by `presented`/`discarded`, which both used to inline this
    /// same lookup independently (Standards review).
    fn surface_id_for(&self, surface: &wl_surface::WlSurface) -> Option<&str> {
        self.surfaces.iter().find(|s| s.layer.wl_surface() == surface).map(|s| s.surface_id.as_str())
    }
}

/// Phase 4 integration proof: shape a static string off-thread via cosmic-text, then
/// rasterize+draw it with FemtoVG, snapping its box to physical pixels. A free
/// function taking each field it needs directly (see the one call site in
/// `bind_and_clear`) rather than a `&mut self` method, so it doesn't need the whole
/// `App` borrowed while a `TrackedSurface` from `self.surfaces` is still live there.
/// Returns `false` on a FemtoVG init failure, so the caller can treat it exactly like
/// every other EGL/GL bind failure in this file (fatal, not logged-and-ignored).
fn draw_main_bar_proof_text(
    shaping: &ShapingHandle,
    egl: &egl::EglState,
    text_painter: &mut Option<TextPainter>,
    width: i32,
    height: i32,
) -> bool {
    const PROOF_TEXT: &str = "Oblisk";
    const FONT_SIZE: f32 = 14.0;

    let shaped = shaping.shape(ShapeRequest {
        text: PROOF_TEXT.into(),
        font_size: FONT_SIZE,
        line_height: FONT_SIZE * 1.2,
        max_width: None,
    });

    if text_painter.is_none() {
        let font_chain_bytes = shaping.font_chain_bytes();
        let painter = TextPainter::new(
            |s| egl.instance.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void),
            width as u32,
            height as u32,
            &font_chain_bytes,
        );
        match painter {
            Ok(p) => *text_painter = Some(p),
            Err(e) => {
                log_bind_failure(SurfaceRole::MainBar, "FemtoVG init", e);
                return false;
            }
        }
    }

    if let Some(painter) = text_painter.as_mut() {
        // The surface can resize after the painter was first built; refresh the
        // canvas's viewport every frame rather than trusting the size from init.
        painter.resize(width as u32, height as u32);
        painter.draw_line(
            PROOF_TEXT,
            LogicalRect { x: 8.0, y: 0.0, width: shaped.width, height: shaped.height },
            FONT_SIZE,
            1.0,
            // White: the exact color `draw_line` used to hardcode internally, unchanged now that
            // it takes one -- this proof-of-wiring call has no `layout::node` property to read a
            // real `foreground` from.
            crate::layout::node::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
        );
        // `draw_line` no longer flushes for itself (build-steps.md Phase 19 item 6): a real tree
        // walk flushes once for a whole surface's worth of nodes, and this lone proof-of-wiring
        // call is its own whole walk, so it flushes right here instead.
        painter.canvas_mut().flush();
        eprintln!(
            "[oblisk-renderer] main_bar: shaped \"{PROOF_TEXT}\" to {}x{} (logical), drew+flushed via FemtoVG",
            shaped.width, shaped.height
        );
    }

    true
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (width, height) = configure.new_size;
        self.bind_and_clear(layer, width, height);
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(App);
smithay_client_toolkit::delegate_dispatch2!(App);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallpaper_surface_id_uses_the_real_output_name_when_present() {
        assert_eq!(wallpaper_surface_id(Some("DP-1"), 0), "wallpaper_layer@DP-1");
        assert_eq!(wallpaper_surface_id(Some("eDP-1"), 3), "wallpaper_layer@eDP-1");
    }

    #[test]
    fn wallpaper_surface_id_falls_back_to_a_stable_index_when_name_is_none() {
        assert_eq!(wallpaper_surface_id(None, 0), "wallpaper_layer@output-0");
        assert_eq!(wallpaper_surface_id(None, 2), "wallpaper_layer@output-2");
    }

    #[test]
    fn text_input_pending_take_done_returns_and_resets_the_pending_commit() {
        let mut pending = TextInputPending::default();
        pending.on_commit_string(Some("h".to_string()));

        let edit = pending.take_done();
        assert_eq!(edit.commit.as_deref(), Some("h"));
        assert!(!edit.submit);

        let next = pending.take_done();
        assert_eq!(next.commit, None, "done must reset pending state for the next cycle");
        assert!(!next.submit);
    }

    #[test]
    fn text_input_pending_take_done_reports_a_pending_submit_action() {
        let mut pending = TextInputPending::default();
        pending.on_action_submit();

        let edit = pending.take_done();
        assert!(edit.submit);

        let next = pending.take_done();
        assert!(!next.submit, "done must reset the pending submit flag for the next cycle");
    }

    #[test]
    fn apply_edit_pushes_commit_text_into_the_secure_buffer_and_reports_submit() {
        let mut buffer = shared::SecureBuffer::new();
        let submit = apply_edit(&mut buffer, TextInputEdit { commit: Some("hunter2".to_string()), submit: true });
        assert_eq!(buffer.expose_secret(), b"hunter2");
        assert!(submit);
    }

    #[test]
    fn apply_edit_accumulates_across_multiple_commit_string_batches() {
        let mut buffer = shared::SecureBuffer::new();
        apply_edit(&mut buffer, TextInputEdit { commit: Some("hunter".to_string()), submit: false });
        apply_edit(&mut buffer, TextInputEdit { commit: Some("2".to_string()), submit: false });
        assert_eq!(buffer.expose_secret(), b"hunter2");
    }

    #[test]
    fn secure_submit_frame_carries_the_accumulated_secret_and_zeroizes_the_buffer_it_read() {
        // build-steps.md Phase 15 item 2 / ADR-0005/ADR-0027: the frame carries the exact secret
        // this thread accumulated, tagged with this process's own generation_id, and the source
        // buffer is scrubbed in the same breath as the read rather than left live.
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("hunter2");

        let frame = secure_submit_frame(4, "polkit", "authenticate", &mut buffer);

        assert_eq!(
            frame,
            RendererFrame::SecureSubmit(SecureSubmit {
                generation_id: 4,
                capability: "polkit".to_string(),
                action: "authenticate".to_string(),
                secret: b"hunter2".to_vec(),
            })
        );
        assert!(buffer.is_empty(), "the source SecureBuffer must be zeroized as soon as it has been read");
    }

    #[test]
    fn apply_edit_with_no_commit_text_leaves_the_buffer_unchanged_and_reports_no_submit() {
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("existing");

        let submit = apply_edit(&mut buffer, TextInputEdit { commit: None, submit: false });

        assert_eq!(buffer.expose_secret(), b"existing");
        assert!(!submit);
    }
}
