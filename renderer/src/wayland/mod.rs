pub mod egl;

use std::collections::HashMap;
use std::error::Error;
use std::ffi::c_void;

use khronos_egl::Surface as EglSurface;
use mlua::{Function, Lua, Table, Value};
use shared::{
    LockOutcome, LockReport, PresentationEvidence, ReadySignal, RendererFrame, SecureSubmit, SupervisorFrame, Zeroize,
};
use smithay_client_toolkit::background_effect::{BackgroundEffectHandler, BackgroundEffectState};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, FrameCallbackData, Region};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::presentation_time::{PresentTime, PresentationTimeHandler, PresentationTimeState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers};
use smithay_client_toolkit::seat::pointer::{
    BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec, ThemedPointer,
};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::session_lock::{
    SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface, SessionLockSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::xdg::popup::{Popup, PopupConfigure, PopupHandler};
use smithay_client_toolkit::shell::xdg::window::{
    DecorationMode, Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shell::xdg::{XdgPositioner, XdgShell, XdgSurface};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_registry, registry_handlers};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface};
use wayland_client::{Connection, Proxy, QueueHandle, WEnum};
use wayland_egl::WlEglSurface;
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_manager_v1;
use wayland_protocols::wp::presentation_time::client::wp_presentation_feedback;
use wayland_protocols::xdg::shell::client::{xdg_positioner, xdg_surface};

use crate::image::ImageCache;
use crate::layout;
use crate::layout::instance::{OutputGeometry, SurfaceInstance, expand_instances, is_instance_of, reconcile_instances};
use crate::layout::node::{
    self, ConstraintAdjustment, LayerKind, PanelSpec, PopupAnchor, PopupSpec, SizeHint, SizeMode, SurfaceSpec,
    WindowSpec,
};
use crate::lua::signal::thread_cpu_time;
use crate::socket::{FrameOutcome, RendererClient};
use crate::text::atlas::TextPainter;
use crate::text::shaping::ShapingHandle;
use crate::text::snap::LogicalRect;

mod idle_profile;
mod input;
mod layer;
mod lock;
mod memory_profile;
mod output;
mod surface;
mod xdg_shell;

use input::{ArmedClick, ArmedSerial, FocusedField, FocusedTextField};
use lock::{EXIT_SUPERVISOR_GONE, supervisor_gone_report};
use output::{geometries_from, screens_payload};
use surface::{TrackedSurface, log_bind_failure};

pub struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor_state: CompositorState,
    seat_state: SeatState,
    layer_shell: LayerShell,
    /// `ext_background_effect_manager_v1` through SCTK's `GlobalProxy` (ADR-0195). A compositor
    /// without the global is not an error: `blur = true` there is silently nothing, the same answer
    /// every other unavailable compositor feature gets here.
    background_effect: BackgroundEffectState,
    /// Whether the manager announces `blur`. [`BackgroundEffectState`] holds the same bit, but only
    /// the current one, and `update_capabilities` needs the previous value to tell a change from a
    /// repeat. Only a change invalidates every surface's pushed region. A compositor may advertise
    /// the global and still not blur, and it may withdraw the bit later.
    blur_supported: bool,
    /// `xdg_wm_base`, plus the `zxdg_decoration_manager_v1` that `XdgShell::bind` picks up beside
    /// it, or `None`; panels still work without xdg-shell, while a declared window logs the missing
    /// global once.
    xdg_shell: Option<XdgShell>,
    /// `ext_session_lock_manager_v1` through SCTK's `GlobalProxy` (ADR-0042). Missing support
    /// surfaces as `GlobalError::MissingGlobal` from `lock` (ADR-0052 decision 4), not startup;
    /// it binds once from the `GlobalList` in [`run`].
    session_lock_state: SessionLockState,
    /// Live `ext_session_lock_v1` from request until Supervisor unlock, denial, or compositor
    /// teardown. `Some` with `is_locked() == false` is the in-flight window; `finished` therefore
    /// has two meanings (ADR-0042).
    session_lock: Option<SessionLock>,
    /// Shared EGL display/config/GLES3 context, lazy because `eglInitialize` loads Mesa,
    /// `libgallium`, and LLVM: 125 MB mapped and 13-35 ms. No-surface configs avoid it
    /// (ADR-0070 decision 7); Candidates pay after `ActivateDraw`, their only bind
    /// (ADR-0071).
    egl: Option<egl::EglState>,
    gl: Option<glow::Context>,
    /// Config shaders compiled against `gl`, kept for the context's lifetime rather than a
    /// generation's: a swap replaces the scene, not the GL objects (ADR-0184).
    shader_stage: crate::layout::image_shader::ShaderStage,
    /// Owns the `wl_display` pointer [`App::ensure_egl`] passes to EGL. Keeping the whole
    /// `Connection` refcounted guarantees `egl::init`'s SAFETY precondition: the display outlives
    /// every EGL object built from it.
    conn: Connection,
    /// Process-wide shaping handle; `client` clones it, so content sizing and painting share one
    /// worker and `FontSystem` (ADR-0039 decision 3).
    shaping: ShapingHandle,
    text_painter: Option<TextPainter>,
    /// Process-wide image cache keyed by file path and pixel size, so repeated icons upload once
    /// (`CONTEXT.md`, **Image cache**).
    image_cache: ImageCache,
    /// Lua VM, `Loader`, retained `Scene`, live signals, and reload state (ADR-0039). `mlua::Lua`
    /// is `!Send`; `wayland-client` imposes no `Send` bound on dispatch state.
    client: RendererClient,
    surfaces: Vec<TrackedSurface>,
    exit: bool,
    /// Whether `OBELISK_SWAP_CANDIDATE` was set (the generation swap), read once in [`run`].
    is_swap_candidate: bool,
    /// Set after [`App::maybe_send_ready_signal`] sends its one-time `ReadySignal`.
    ready_signal_sent: bool,
    /// Set after startup evaluation and surface creation. The initial `wl_output` burst occurs in
    /// [`run`]'s two roundtrips before `screens` seeds evaluation (ADR-0041 decision 2), so output
    /// changes must not reconcile before a spec exists.
    startup_complete: bool,
    /// Frames for the socket thread's `pump`; `UnboundedSender::send` is synchronous and
    /// non-blocking, so dispatch callbacks can use it.
    outbound_tx: tokio::sync::mpsc::UnboundedSender<RendererFrame>,
    /// This Renderer's generation id, stamped into every `SecureSubmit`; read in `main` from
    /// `OBELISK_GENERATION_ID`.
    generation_id: u32,
    presentation_time: PresentationTimeState,
    /// Clone used by poll-loop [`App::activate_draw`] to request `wp_presentation_feedback`.
    queue_handle: QueueHandle<App>,
    /// In-flight `ActivateDraw` nonce for every `presented` event. The generation swap has one
    /// handshake at a time, so one field replaces a per-surface map.
    active_nonce: Option<u64>,
    /// Advertised seat pointer, kept alive because dropping it destroys pointer events. One slot;
    /// [`SeatHandler::new_capability`] stores whichever seat announces the capability.
    pointer: Option<ThemedPointer>,
    /// Last pointer shape over this process's surfaces (ADR-0107). `Leave` clears it because
    /// `wp_cursor_shape_v1` requires a shape on every `Enter`.
    cursor_shown: Option<cursor_icon::CursorIcon>,
    /// Last pointer surface and position (ADR-0112 amendment), set by `Enter` and `Motion`, cleared
    /// by `Leave`, and rewritten against hover signals after a re-resolve because scrolling moves
    /// rows under a still pointer and no `Motion` arrives to say so.
    pointer_at: Option<(String, (f64, f64))>,
    /// `wl_shm` only for SCTK's XCursor fallback when `wp_cursor_shape_v1` is absent; all other
    /// rendering uses EGL.
    shm: Shm,
    /// Advertised keyboard, kept alive and single-seat. Used only for `enter`/`leave`, the only
    /// client-visible result of `keyboard_interactivity`.
    keyboard: Option<wl_keyboard::WlKeyboard>,
    /// Focused surface instance id (ADR-0050); `input::keyboard::focus_is_still_armed` requires a
    /// `secure_submit` field's declaring surface to match it.
    ///
    /// ponytail: nothing else consumes it (there is no `on_key` property; ADR-0050 declines to
    /// invent one). Upgrade path: an IDL key-handler property, dispatching into this surface's
    /// tree.
    keyboard_focus: Option<String>,
    /// Press waiting for release (ADR-0050 decision 2, [`ArmedClick`]).
    armed: Option<ArmedClick>,
    /// Held left press on an `on_drag` button (ADR-0116 decision 1); `Motion` reports until release
    /// or `Leave`.
    drag: Option<input::ActiveDrag>,
    /// The serial for `xdg_popup.grab`, valid for one poll turn (ADR-0049 amendment).
    input_serial: Option<ArmedSerial>,
    /// Never-reset count of every `BTN_LEFT` press and release (ADR-0051 amendment). Count both
    /// edges so a later turn has something to compare, whatever order the compositor batches
    /// dismissal relative to `popup_done`. It survives `input_serial`'s per-turn lifetime and
    /// tells whether the user asked again.
    pointer_input_count: u64,
    /// Serial for `xdg_popup.reposition`, returned on the configure it causes
    /// (`ConfigureKind::Reposition`). One counter for the process rather than one per popup: it
    /// only has to tell two requests apart, and wrapping is harmless because nothing here waits on
    /// a specific token.
    reposition_token: u32,
    /// Focused `secure_submit` field and declaring surface, set by a textfield press or sole-field
    /// keyboard focus (ADR-0050 decision 4). `None` means no frame; writes go through
    /// [`App::focus_secure_submit`].
    focused_secure_submit: Option<FocusedField>,
    /// [`App::focus_key`] as end-of-turn arming last examined it, for the profiler's `redundant`
    /// column. Written only while `OBELISK_PROFILE_IDLE` is set; nothing gates on it yet.
    ///
    /// Starts as an empty dead scope rather than the first key observed, so the first focused turn
    /// reads as a change and is not silently classed as removable.
    last_focus_key: (Vec<String>, bool),
    /// Focused plain `textfield` and its draft, the unmasked half (ADR-0092). Only a
    /// press selects it; sole-field `enter` fallback cannot serve multiple reply boxes.
    ///
    /// Mutually exclusive with `focused_secure_submit`; the innermost textfield is one kind.
    focused_text_field: Option<FocusedTextField>,
    /// Native, Lua-invisible keystroke buffer until Enter (ADR-0005/ADR-0009/ADR-0027). Its
    /// lifetime follows `focused_secure_submit`; destination changes zeroize it.
    secure_buffer: shared::SecureBuffer,
    /// A keystroke/focus change changed field rendering without dirtying the retained tree: masked
    /// bytes and plain text live outside it (ADR-0005, ADR-0092), so re-resolve misses the update.
    /// Repaint only; `FieldFocus` changes the display list, and `paint_surface` narrows it to the
    /// field's surface. Without this flag, the mask or caret appears only on an unrelated repaint,
    /// once a second on a lock-screen clock.
    field_input_changed: bool,
    /// A compositor frame callback landed for a surface whose tree was mid-tween (ADR-0145). The
    /// poll loop takes it once per turn and advances every tween; `paint_surface` asks for the
    /// next one while anything is still moving, which is what keeps the chain alive and lets it
    /// die on its own when nothing is (ADR-0130 decision 3).
    animation_frame_due: bool,
    /// Surfaces actually drawn and swapped since the last idle-profile sample; paint walks all
    /// mapped surfaces and declines most, so the aggregate count matters.
    surfaces_drawn: usize,
}

/// Renderer main thread: Wayland, EGL, Lua, the retained `Scene`, and live signals (ADR-0039).
/// `inbound_rx` carries socket-decoded `SupervisorFrame`s; `outbound_tx` carries every frame this
/// thread sends back, including replies, readiness, presentation evidence, and lock reports. Ends
/// the process on a dead Wayland connection, like the `EXIT_SUPERVISOR_GONE` arm below and for the
/// same reason: `std::process::exit` skips destructors. Returning an error instead unwinds `App`,
/// whose EGL surfaces and `wl_surface`s talk to the compositor that just left, which is how a log
/// out became a `khronos-egl` `unwrap()` panic and exit code 101.
fn exit_because_the_compositor_is_gone(what_failed: &str, err: &dyn std::fmt::Display) -> ! {
    eprintln!("[obelisk-renderer] {what_failed} failed ({err}); there is no compositor to talk to, so exiting");
    std::process::exit(shared::EXIT_COMPOSITOR_GONE);
}

pub fn run(
    generation_id: u32,
    mut inbound_rx: tokio::sync::mpsc::Receiver<SupervisorFrame>,
    outbound_tx: tokio::sync::mpsc::UnboundedSender<RendererFrame>,
    waker: crate::wake::Waker,
) -> Result<(), Box<dyn Error>> {
    // A missing socket is not a failure to report up: at session end the Supervisor outlives the
    // compositor briefly and respawns into a session that is already gone, which is where the three
    // `Error: NoCompositor` generations came from. Nothing is built yet, so there is nothing to
    // skip unwinding past; this is the same answer for the same reason.
    let conn = match Connection::connect_to_env() {
        Ok(conn) => conn,
        Err(err) => exit_because_the_compositor_is_gone("connecting to the Wayland display", &err),
    };
    let (globals, mut event_queue) = registry_queue_init::<App>(&conn)?;
    let qh = event_queue.handle();

    let compositor_state = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    // Optional: panel-only configs work without `xdg_wm_base`; `create_surfaces` logs any window
    // left unbuilt.
    let xdg_shell =
        XdgShell::bind(&globals, &qh).inspect_err(|err| log_bind_failure("<xdg-shell>", "xdg_wm_base::bind", err)).ok();
    // Optional, and quiet when absent: a compositor with no blur is not a broken session. Its
    // `GlobalProxy` reports a missing global only when a blur region is actually asked for.
    // Version 1 is the only version; `capabilities` arrives on the queue right after this.
    let background_effect = BackgroundEffectState::new(&globals, &qh);
    let output_state = OutputState::new(&globals, &qh);
    let seat_state = SeatState::new(&globals, &qh);
    // Mandatory: every compositor advertises `wl_shm`.
    let shm = Shm::bind(&globals, &qh)?;
    // Cannot fail: its `GlobalProxy` reports a missing lock global only when a lock is requested
    // (ADR-0052 decision 4).
    let session_lock_state = SessionLockState::new(&globals, &qh);
    let registry_state = RegistryState::new(&globals);
    // `bind` tolerates a missing presentation-time global; later `feedback()` reports
    // `GlobalError::MissingGlobal`.
    let presentation_time = PresentationTimeState::bind(&globals, &qh);

    let is_swap_candidate = std::env::var("OBELISK_SWAP_CANDIDATE").is_ok();

    // One process-wide shaping handle; `RendererClient` gets a clone (ADR-0039 decision 3).
    // `Loader::new()` stays here because `mlua::Lua` is `!Send`.
    let shaping = ShapingHandle::spawn();
    let client = RendererClient::start(shaping.clone(), outbound_tx.clone(), generation_id)?;

    let mut app = App {
        registry_state,
        output_state,
        shader_stage: crate::layout::image_shader::ShaderStage::default(),
        compositor_state,
        seat_state,
        layer_shell,
        background_effect,
        blur_supported: false,
        xdg_shell,
        session_lock_state,
        session_lock: None,
        egl: None,
        gl: None,
        conn: conn.clone(),
        shaping,
        text_painter: None,
        image_cache: ImageCache::with_waker(waker.clone()),
        client,
        surfaces: Vec::new(),
        exit: false,
        is_swap_candidate,
        ready_signal_sent: false,
        startup_complete: false,
        outbound_tx,
        generation_id,
        presentation_time,
        queue_handle: qh.clone(),
        active_nonce: None,
        pointer: None,
        cursor_shown: None,
        pointer_at: None,
        shm,
        keyboard: None,
        keyboard_focus: None,
        armed: None,
        drag: None,
        input_serial: None,
        pointer_input_count: 0,
        reposition_token: 0,
        focused_secure_submit: None,
        last_focus_key: (Vec::new(), false),
        focused_text_field: None,
        secure_buffer: shared::SecureBuffer::new(),
        field_input_changed: false,
        animation_frame_due: false,
        surfaces_drawn: 0,
    };

    // Binding delivers outputs and seat capabilities as a burst; two roundtrips populate the
    // initial output list (`monitor = "All"` expands per monitor) and keyboard capability.
    event_queue.roundtrip(&mut app)?;
    event_queue.roundtrip(&mut app)?;

    // Candidate order from the generation swap: evaluate shell.lua, bind declared layer surfaces
    // (ADR-0038 decision 1), commit null buffers (`bind_and_clear`'s candidate branch), and signal
    // ready (`maybe_send_ready_signal`).
    //
    // ponytail: this runs inside the swap's ready window (`ready_timeout` 2s,
    // `supervisor/src/main.rs`'s `SWAP_TIMINGS`); first `text` shaping blocks on
    // `FontSystem::new()`. Accepted because the generation swap requires evaluate-before-bind.
    //
    // Seed `screens` before evaluation (ADR-0041 decision 2): configs loop over it during the
    // first pass, so seeding after evaluation would declare no per-monitor panels.
    let screens = app.screens(None);
    let outputs = geometries_from(&screens);
    app.image_cache.set_texture_budget(output::texture_budget(&screens));
    app.client.set_screens(screens_payload(&screens));
    let specs = app.client.run_startup_evaluation().unwrap_or_default();
    // Set the declared font chain after evaluation but before first paint; `TextPainter` loads it
    // lazily, and `set_chain` rebuilds instead of respawning. No declaration keeps the default.
    // A family a node names by hand is not resolved here: it lands on first sight (ADR-0144).
    app.shaping.set_chain(&crate::lua::fonts::declared_chain(app.client.lua()));
    let instances = expand_instances(&specs, &outputs);
    for spec in &specs {
        let SurfaceSpec::Panel(panel) = spec else {
            // Only a `panel` names a monitor; toplevels are compositor-placed and popups use
            // a parent.
            continue;
        };
        if panel.topology.monitor != "All" && !outputs.iter().any(|output| output.name == panel.topology.monitor) {
            // `expand_instances` returns nothing for a miss; log against the real startup output
            // list so an unplugged monitor is explained once.
            eprintln!(
                "[obelisk-renderer] surface {:?} targets monitor {:?}, which is not connected; no surface created for it",
                panel.topology.id, panel.topology.monitor
            );
        }
    }
    app.client.set_instances(instances.clone());
    // The first resolve validates only: the generation swap requires evaluate-before-bind, so
    // instances use output logical sizes and are never painted. Evaluation/apply already log and
    // set rescue; this adds the consequence.
    if !app.client.apply_instances() {
        eprintln!(
            "[obelisk-renderer] no scene was applied at startup; surfaces still bind, and paint nothing until a reload or a push produces one"
        );
    }

    app.create_surfaces(&qh, &specs, &instances);
    if app.is_swap_candidate {
        // Hidden-only windows have no `xdg_toplevel` configure (ADR-0049 decision 1), so without
        // this gate such a Candidate never sends `ReadySignal` and hits `ready_timeout`.
        app.maybe_send_ready_signal();
    }
    // Output events can now reconcile against an evaluated scene.
    app.startup_complete = true;

    // `None` unless `OBELISK_PROFILE_IDLE` is set; see `idle_profile`.
    let mut profile = idle_profile::IdleProfile::from_env();
    // `None` unless `OBELISK_PROFILE_MEMORY` is set; see `memory_profile`.
    let mut memory = memory_profile::MemoryProfile::from_env();

    // Mostly-static surfaces may receive no Wayland event after `ActivateDraw`, so poll
    // `inbound_rx` with bounded latency instead of blocking on the Wayland fd. Non-Candidates still
    // draw synchronously in the first configure handler.
    loop {
        // `then` leaves the clock unread while the profile is off, as `idle_profile` promises.
        let dispatch_started = profile.is_some().then(thread_cpu_time).flatten();
        let dispatched = match event_queue.dispatch_pending(&mut app) {
            Ok(count) => count > 0,
            // Only an I/O failure means the connection itself is gone. A `BadMessage` or a
            // `Protocol` error is this Renderer's own bug against a compositor that is still there,
            // so it keeps propagating and stays a reportable crash.
            Err(wayland_client::DispatchError::Backend(wayland_client::backend::WaylandError::Io(err))) => {
                exit_because_the_compositor_is_gone("dispatching Wayland events", &err)
            }
            Err(err) => return Err(err.into()),
        };
        if let Some(started) = dispatch_started
            && let Some(ended) = thread_cpu_time()
            && let Some(profile) = profile.as_mut()
        {
            profile.dispatch(ended.saturating_sub(started));
        }
        if app.exit {
            break;
        }
        // Drain every `SupervisorFrame` (ADR-0039), coalescing snapshot bursts into one wake. A
        // dead socket is distinct from an empty one (ADR-0059 decision 1), or the Renderer could
        // block in `poll` with no capability source. Collect draw nonces until after the drain so
        // a paired snapshot paints first; keep a `Vec` because each nonce owes evidence.
        let mut draw_nonces: Vec<u64> = Vec::new();
        loop {
            let frame = match inbound_rx.try_recv() {
                Ok(frame) => frame,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                // `std::process::exit` skips `SessionLockInner::Drop`, whose bare destroy is
                // `invalid_destroy` after `locked`; skipping the destructor closes the connection
                // instead, logged as an ordinary lock client death. Dropping `App` would kill it.
                // Use `is_some()`, not SCTK's dispatch-lagging `is_locked()`: over-reporting a VT
                // is safer than claiming the shell died behind an inaccessible lock screen.
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    // Flush decided requests before exit: `lock` only enqueues, and the normal
                    // flush is below this drain. Otherwise `session_lock = Some` could outlive an
                    // unsent request. `std::process::exit` skips SCTK's destructor.
                    if let Err(err) = event_queue.flush() {
                        eprintln!(
                            "[obelisk-renderer] the last flush before exiting failed ({err}); a session lock requested in this same turn may never have reached the compositor"
                        );
                    }
                    eprintln!("[obelisk-renderer] {}", supervisor_gone_report(app.session_lock.is_some()));
                    std::process::exit(EXIT_SUPERVISOR_GONE);
                }
            };
            match app.client.handle_frame(frame) {
                FrameOutcome::Handled => {}
                FrameOutcome::ActivateDraw(nonce) => draw_nonces.push(nonce),
                // Service immediately: lock declaration is tracked-surface state, not a
                // capability-push result (ADR-0052 decision 3), and deferring weakens "secure now".
                FrameOutcome::SetSessionLock(locked) => {
                    // Round-trip only before unlock: SCTK gates `unlock` on dispatched
                    // `locked`, not sent. Without it, `unlock` can no-op and `Drop` sends forbidden
                    // `destroy` (`invalid_destroy`) (ADR-0052). Acquire needs no round-trip because
                    // this thread owns its inputs. Do not return on a failed round-trip: that would
                    // strand the session locked; trying the unlock costs at most one failed flush.
                    if !locked && let Err(err) = event_queue.roundtrip(&mut app) {
                        eprintln!(
                            "[obelisk-renderer] the round trip before an unlock failed ({err}); attempting the unlock anyway rather than exiting with the session locked"
                        );
                    }
                    app.set_session_lock(&qh, locked);
                }
            }
            if app.exit {
                break;
            }
        }
        if app.exit {
            break;
        }
        // Once after the drain, `DirtyFlag::take` coalesces snapshot bursts into one `Scene::apply`
        // (ADR-0044 decision 2). Do not use `wl_surface::frame()`: it would block idle instead of
        // waking on the 15 ms poll, while ADR-0124 makes the push itself the wakeup. Staging and
        // repainting are one commit: the latter's `swap_buffers` carries layer/input/map changes;
        // per-field commits would show the compositor a half-updated surface.
        // Profiling adds three `clock_gettime` calls per turn for the resolve/repaint split.
        let mut phases = idle_profile::Phases::start(profile.is_some());
        app.client.fire_due_timers();
        app.client.wake_due_signals();
        // Kept as its own name, not folded into `re_resolved` below: "a pass ran" and "something
        // changed" answer different questions. Only a pass can change any tree, so only a pass
        // rules out the narrowed repaint, and only a pass makes every surface's protocol state
        // worth re-deriving.
        let passed = app.client.re_resolve_if_dirty();
        // A frame callback is the tween clock (ADR-0145). Taken every turn so a callback that
        // arrives with a push is answered by this repaint, not repeated next turn. A turn that
        // re-resolved skips the tick: the pass's `retarget` already advanced every visible tween
        // to its own instant, and the repaint asks for the next callback either way.
        let ticked = if std::mem::take(&mut app.animation_frame_due) && !passed {
            app.client.tick_animations(std::time::Instant::now())
        } else {
            Vec::new()
        };
        let re_resolved = passed || !ticked.is_empty();
        phases.mark_resolve();
        // Take unconditionally so a keystroke arriving with a push is covered by this repaint, not
        // repeated next turn.
        let typed = std::mem::take(&mut app.field_input_changed);
        // A decode changes neither retained properties nor a list that names the file, so it is
        // its own repaint/invalidation (ADR-0122).
        let landed = app.image_cache.poll();
        if !landed.is_empty() {
            // The cue to repaint, and nothing more: what each node is now showing is settled by
            // the paint that follows, which is the only thing holding the exact cache keys
            // (ADR-0183).
            app.forget_painted_lists_drawing(&landed);
        }
        // What protocol state this turn owes; `surface_state_for_turn` carries the reasoning.
        let state = surface::surface_state_for_turn(passed, !ticked.is_empty(), app.input_serial.is_some());
        match state.scope {
            surface::StateScope::Everything => app.apply_resolved_surface_state(),
            surface::StateScope::Ticked => app.apply_resolved_surface_state_for(&ticked),
            surface::StateScope::Nothing => {}
        }
        if state.popup_latch {
            app.apply_popup_visibility_for_armed_input();
        }
        if re_resolved {
            // Hover signals follow layout; `on_hover` follows the pointer (ADR-0112 amendment).
            app.refresh_hover_after_layout();
        }
        phases.mark_surface_state();
        // Which surfaces this turn owes the screen; `repaint_for_turn` carries the reasoning.
        match surface::repaint_for_turn(surface::TurnChanges {
            passed,
            ticked: !ticked.is_empty(),
            stale: app.has_stale_surfaces(),
            typed,
            landed: !landed.is_empty(),
        }) {
            surface::Repaint::Narrowed => app.repaint_surfaces_with_instance_ids(&ticked),
            surface::Repaint::Everything => app.repaint_mapped_surfaces(),
            surface::Repaint::Nothing => {}
        }
        phases.mark_repaint();
        // Skip focus maintenance on a truly idle turn (ADR-0124). It clones the focused tree to
        // find fields; at 66 turns/s on an open picker, that was most of the process's work.
        let active = dispatched || re_resolved || typed || !landed.is_empty() || !draw_nonces.is_empty();
        // Disarm after the turn, not only when active: `dispatch_pending` armed this serial and
        // `apply_resolved_surface_state` is its only reader. This enforces ADR-0049's one-turn
        // real-input window.
        app.input_serial = None;
        // Once per active turn, scrub a secure field whose surface was torn down before a later
        // keystroke notices. `App::apply_secure_key` remains the load-bearing check.
        if active {
            let focus_started = profile.is_some().then(thread_cpu_time).flatten();
            app.drop_secure_focus_if_its_surface_is_gone();
            // Sample after the cleanup above: clearing secure focus can enable a search. Failing
            // these guards excludes the turn from `searched`, not from `focus_turns` or its CPU.
            let searched = app.keyboard_focus.is_some() && app.focused_secure_submit.is_none();
            // Shadow the candidate gate rather than apply it: sample what it would compare, before
            // arming runs, and let the `redundant` column say how many turns it would have skipped.
            // Sampling pre-arm is what makes the stored key mean "what maintenance examined".
            let unchanged = focus_started.is_some_and(|_| {
                let key = app.focus_key();
                let same = key == app.last_focus_key;
                app.last_focus_key = key;
                same
            });
            // Also arm fields that appeared under already-arrived keyboard focus.
            app.arm_secure_focus_if_the_scope_now_declares_one();
            app.arm_autofocus_if_nothing_is_typing();
            if let Some(started) = focus_started
                && let Some(ended) = thread_cpu_time()
                && let Some(profile) = profile.as_mut()
            {
                let removable = searched && unchanged && !re_resolved && !typed;
                profile.focus(ended.saturating_sub(started), searched, removable);
            }
        }
        if let Some(profile) = profile.as_mut() {
            // After focus maintenance so a turn's focus cost reports in its own window, and
            // still before consuming nonces or breaking, so an exiting turn is reported.
            profile.turn(
                idle_profile::Turn {
                    dispatched,
                    re_resolved,
                    ticked: !ticked.is_empty(),
                    typed,
                    decoded: !landed.is_empty(),
                    draws: draw_nonces.len(),
                    painted: re_resolved || typed || !landed.is_empty(),
                    drawn: std::mem::take(&mut app.surfaces_drawn),
                },
                phases,
            );
        }
        if let Some(memory) = memory.as_mut() {
            // The closure keeps the scene walk and the cache locks off every turn but the one
            // that reports; see `memory_profile`.
            memory.maybe_report(|| census(&app));
        }
        for nonce in draw_nonces {
            app.activate_draw(nonce);
            if app.exit {
                break;
            }
        }
        if app.exit {
            break;
        }
        // This is the flush that actually caught the log out: it propagated, `run` returned, and
        // `App`'s destructor then drove EGL into a compositor that was gone.
        if let Err(wayland_client::backend::WaylandError::Io(err)) = event_queue.flush() {
            exit_because_the_compositor_is_gone("flushing the Wayland queue", &err);
        }
        if let Some(guard) = event_queue.prepare_read() {
            let fd = guard.connection_fd();
            // No timeout while idle (ADR-0124): Wayland events use the connection fd; Supervisor
            // frames, landed decodes, and socket-thread exit use the waker. The one timeout is a
            // pending `delay(signal, ms)` or an open `pulse(signal, ms)` window (ADR-0146,
            // ADR-0153), armed only while one is running, the way a frame callback is requested
            // only while a tween is.
            let mut fds = [
                nix::poll::PollFd::new(fd, nix::poll::PollFlags::POLLIN),
                nix::poll::PollFd::new(waker.fd(), nix::poll::PollFlags::POLLIN),
            ];
            let timeout = app.client.next_wake_deadline().map_or(nix::poll::PollTimeout::NONE, |due| {
                // Rounded up: `as_millis` on the last fraction of a hold is 0, and a zero timeout
                // returns at once to a turn that finds the deadline still a few hundred
                // microseconds away, hundreds of times over.
                let millis = due.saturating_duration_since(std::time::Instant::now()).as_micros().div_ceil(1000);
                nix::poll::PollTimeout::try_from(millis.min(i32::MAX as u128) as i32)
                    .unwrap_or(nix::poll::PollTimeout::NONE)
            });
            let woke = matches!(nix::poll::poll(&mut fds, timeout), Ok(n) if n > 0);
            let wayland_ready = woke && fds[0].any().unwrap_or(false);
            if let Some(profile) = profile.as_mut() {
                profile
                    .wake(idle_profile::Wake { wayland: wayland_ready, waker: woke && fds[1].any().unwrap_or(false) });
            }
            if woke {
                if wayland_ready && let Err(wayland_client::backend::WaylandError::Io(err)) = guard.read() {
                    // The read side, and the one a killed compositor actually reaches first: `poll`
                    // reports the fd readable because the peer closed it, and the read that follows
                    // is what sees the broken pipe.
                    exit_because_the_compositor_is_gone("reading from the Wayland connection", &err);
                }
                // Drain before the turn; a wake arriving during the turn remains counted.
                waker.drain();
            }
            // The guard drops here; an unread guard yields no events next iteration.
        }
    }

    // While a context is still current, and only here: a config shader's program outlives every
    // generation, so nothing earlier owns its end (ADR-0184). A context already gone took its
    // objects with it, which is why this is an orderly teardown and not a recovery.
    if let Some(gl) = app.gl.as_ref() {
        // SAFETY: this is the context every paint bound, on the one thread that ever bound it.
        unsafe { app.shader_stage.destroy(gl) };
    }
    Ok(())
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

// This SCTK (`smithay-client-toolkit-0.21.1`, checked against `src/`) ships exactly two
// `delegate_*` macros: `delegate_dispatch2!` and `delegate_registry!`. `PointerData`,
// `KeyboardData`, `WindowData`, `PopupData`, `GlobalData` and `SessionLockData`/
// `SessionLockSurfaceData` each carry a blanket `Dispatch2` impl, which the line below turns into
// the `Dispatch` half every bind/create call needs. So `PointerHandler`, `KeyboardHandler`,
// `WindowHandler`, `PopupHandler` and `SessionLockHandler` are implemented above with no
// `delegate_pointer!`/`delegate_keyboard!`/`delegate_xdg_shell!`/`delegate_xdg_popup!`/
// `delegate_session_lock!` call to match: none of those macros exist in this SCTK.
/// `ext_background_effect_manager_v1` (ADR-0195). The manager's one event is `capabilities`, a
/// bitfield the compositor sends on bind and again whenever it changes; the `blur` bit going away
/// means the compositor has stopped applying blur even for regions already set, so this tracks the
/// current value rather than the one at startup.
///
/// SCTK does the decode, which is why the manager is routed through it rather than dispatched
/// here. A compositor announcing `blur` beside a bit this build does not know sends a value
/// `Capability::from_bits` rejects. Reading that rejection as unsupported switches blur off over a
/// capability that has nothing to do with blur; `from_bits_retain` keeps the bit instead.
impl BackgroundEffectHandler for App {
    fn background_effect_state(&mut self) -> &mut BackgroundEffectState {
        &mut self.background_effect
    }

    fn update_capabilities(&mut self) {
        let blur = self
            .background_effect
            .capabilities()
            .is_some_and(|caps| caps.contains(ext_background_effect_manager_v1::Capability::Blur));
        if self.blur_supported == blur {
            return;
        }
        self.blur_supported = blur;
        // Every surface's last pushed region is now a lie in both directions: while support was
        // off nothing was sent, and when it goes off the compositor drops what it holds. Clearing
        // the record makes the next resolve push again rather than compare equal and skip.
        for surface in &mut self.surfaces {
            surface.last_blur_region.clear();
        }
    }
}

delegate_registry!(App);
smithay_client_toolkit::delegate_dispatch2!(App);

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

/// Reads every subsystem that owns heap into one [`memory_profile::Census`], at one instant so the
/// columns are comparable. `malloc` is left default: `MemoryProfile` reads `mallinfo2` itself,
/// after this returns, so the arena totals include whatever this walk allocated rather than
/// missing it.
fn census(app: &App) -> (memory_profile::Census, memory_profile::Surfaces) {
    let (image_bytes, ready, pending, failed, evicted, landed) = app.image_cache.census();
    let (shape_entries, shape_bytes) = app.shaping.census();
    let (surfaces, nodes, properties) = app.client.scene().census();
    let census = memory_profile::Census {
        image_bytes: image_bytes as u64,
        image_ready: ready as u64,
        image_pending: pending as u64,
        image_failed: failed as u64,
        image_evicted: evicted as u64,
        image_landed: landed as u64,
        shape_entries: shape_entries as u64,
        shape_bytes: shape_bytes as u64,
        lua_bytes: app.client.lua().used_memory() as u64,
        scene_surfaces: surfaces as u64,
        scene_nodes: nodes as u64,
        scene_properties: properties as u64,
        malloc: memory_profile::Malloc::default(),
    };
    (census, memory_profile::Surfaces(app.client.scene().census_by_surface()))
}
