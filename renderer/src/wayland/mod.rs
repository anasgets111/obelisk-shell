pub mod egl;

use std::collections::HashMap;
use std::error::Error;
use std::ffi::c_void;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::presentation_time::{PresentTime, PresentationTimeHandler, PresentationTimeState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers};
use smithay_client_toolkit::seat::pointer::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::session_lock::{
    SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface, SessionLockSurfaceConfigure,
};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::xdg::{XdgPositioner, XdgShell, XdgSurface};
use smithay_client_toolkit::shell::xdg::popup::{Popup, PopupConfigure, PopupHandler};
use smithay_client_toolkit::shell::xdg::window::{DecorationMode, Window, WindowConfigure, WindowDecorations, WindowHandler};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::{delegate_registry, registry_handlers};
use khronos_egl::Surface as EglSurface;
use mlua::{Function, Lua, Table, Value};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface};
use wayland_client::{Connection, Proxy, QueueHandle, WEnum};
use wayland_egl::WlEglSurface;
use wayland_protocols::wp::presentation_time::client::wp_presentation_feedback;
use wayland_protocols::xdg::shell::client::{xdg_positioner, xdg_surface};
use shared::{LockOutcome, LockReport, PresentationEvidence, ReadySignal, RendererFrame, SecureSubmit, SupervisorFrame, Zeroize};

use crate::layout;
use crate::layout::instance::{OutputGeometry, SurfaceInstance, expand_instances, is_instance_of, reconcile_instances};
use crate::layout::node::{
    self, ConstraintAdjustment, LayerKind, PanelSpec, PopupAnchor, PopupSpec, SizeHint, SizeMode, SurfaceSpec, WindowSpec,
};
use crate::image::ImageCache;
use crate::socket::{FrameOutcome, RendererClient};
use crate::text::atlas::TextPainter;
use crate::text::shaping::ShapingHandle;
use crate::text::snap::LogicalRect;

mod input;
// `socket.rs` asks this one question of the keyboard's own rule (docs/adr/0052 decision 2),
// so it is re-exported rather than making the whole module crate-visible for it.
pub(crate) use input::tree_can_authenticate;
mod layer;
mod lock;
mod output;
mod surface;
mod xdg_shell;

use input::{ArmedClick, ArmedSerial, FocusedField};
use lock::{EXIT_SUPERVISOR_GONE, supervisor_gone_report};
use output::{geometries_from, screens_payload};
use surface::{TrackedSurface, log_bind_failure};

pub struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor_state: CompositorState,
    seat_state: SeatState,
    layer_shell: LayerShell,
    /// `xdg_wm_base`, plus the `zxdg_decoration_manager_v1` `XdgShell::bind` picks up alongside it.
    /// `None` on a compositor advertising no xdg-shell: a `panel`-only config still works there,
    /// and a declared `window` says so once instead of taking the process down.
    xdg_shell: Option<XdgShell>,
    /// `ext_session_lock_manager_v1`, or the knowledge that the compositor advertises none
    /// (docs/adr/0042). Not an `Option` like `xdg_shell`: SCTK wraps the global in a
    /// `GlobalProxy`, so the absent case surfaces as `GlobalError::MissingGlobal` from `lock`
    /// itself, a refusal of a lock command rather than a startup bind failure (docs/adr/0052
    /// decision 4). Not in `registry_handlers![OutputState, SeatState]`: `SessionLockState` is
    /// not a `RegistryHandler`; it binds once from the `GlobalList` in [`run`].
    session_lock_state: SessionLockState,
    /// The live `ext_session_lock_v1`, from the moment `lock` is sent until the lock ends: an
    /// unlock the Supervisor ordered, a denial, or a compositor teardown.
    ///
    /// `Some` with `is_locked()` still false is the in-flight window between request and answer,
    /// which is why `finished` is two different events (docs/adr/0042) -- see
    /// `lock::finished_outcome`.
    session_lock: Option<SessionLock>,
    egl: egl::EglState,
    gl: Option<glow::Context>,
    /// The one `ShapingHandle` for the process; `client` holds a clone, so content-sizing and
    /// painting share one worker thread and one `FontSystem` (docs/adr/0039 decision 3).
    shaping: ShapingHandle,
    text_painter: Option<TextPainter>,
    /// One image cache for the process, keyed by file path and pixel size, so an icon drawn on
    /// the bar and the same icon in a popup are one upload, not one per surface (`CONTEXT.md`,
    /// **Image cache**).
    image_cache: ImageCache,
    /// The Lua VM, `Loader`, retained `Scene`, live signals and reload bookkeeping (docs/adr/0039).
    /// `mlua::Lua` is `!Send`, so `App` is too -- fine, since `wayland-client` puts no `Send`
    /// bound on the dispatch state.
    client: RendererClient,
    surfaces: Vec<TrackedSurface>,
    exit: bool,
    /// `OBLISK_PBA_CANDIDATE` is set (§ 15.2) -- read once in [`run`], not re-read per configure.
    is_pba_candidate: bool,
    /// Set once [`App::maybe_send_ready_signal`] has sent `ReadySignal`: a one-time signal, never
    /// resent even if a later spurious configure re-triggers the check.
    ready_signal_sent: bool,
    /// Set once [`run`]'s startup sequence has evaluated the config and built its surfaces. The
    /// initial `wl_output` burst dispatches inside `run`'s own two roundtrips, before the
    /// evaluation that seeds `screens` from it (docs/adr/0041 decision 2), so
    /// [`App::handle_output_change`] must not run its full job that early: there is no
    /// evaluation to expand yet.
    startup_complete: bool,
    /// Every frame this thread sends the Supervisor goes here; the socket thread's `pump` drains
    /// it and writes it to the wire. `UnboundedSender::send` is synchronous and non-blocking, so
    /// it's safe to call from inside a `Dispatch` callback.
    outbound_tx: tokio::sync::mpsc::UnboundedSender<RendererFrame>,
    /// This Renderer's own generation id, stamped into every `SecureSubmit` it writes -- read
    /// once in `main` from `OBLISK_GENERATION_ID`.
    generation_id: u32,
    presentation_time: PresentationTimeState,
    /// Cloned once in [`run`] so [`App::activate_draw`], called from the poll loop rather than a
    /// `Dispatch` callback, can still request `wp_presentation_feedback`.
    queue_handle: QueueHandle<App>,
    /// The `ActivateDraw` nonce currently being drawn, if any -- tags every
    /// `wp_presentation_feedback` `presented` event while in flight. PBA drives one handshake at
    /// a time, so one field, not a per-surface map, is enough.
    active_nonce: Option<u64>,
    /// The seat's pointer, once advertised. Kept alive because dropping the proxy destroys the
    /// protocol object and with it every `enter`/`press`/`release`. One, not one per seat:
    /// [`SeatHandler::new_capability`] takes whichever seat announced the capability into this
    /// one slot, so this file is single-seat.
    pointer: Option<wl_pointer::WlPointer>,
    /// The seat's keyboard, once advertised. Kept alive and single-seat for the same reason
    /// `pointer` is. This shell reads no keys off it directly; it is bound for `enter`/`leave`
    /// alone, the only way a client learns which surface `keyboard_interactivity` actually won
    /// focus for.
    keyboard: Option<wl_keyboard::WlKeyboard>,
    /// The instance id of the surface holding keyboard focus, if any (docs/adr/0050's
    /// consequences). `input::focus_is_still_armed` reads it on every keystroke: a `secure_submit`
    /// field is armed only while the surface that declared it is the one this names.
    ///
    /// ponytail: nothing *else* consumes it, because § 5.2 has no `on_key` for a keysym to route to
    /// and docs/adr/0050 explicitly declines to invent one. Upgrade path: an IDL key-handler
    /// property, at which point this is the surface whose tree the keysym gets dispatched into.
    keyboard_focus: Option<String>,
    /// The press waiting for its release, if any (docs/adr/0050 decision 2, [`ArmedClick`]).
    armed: Option<ArmedClick>,
    /// The serial `xdg_popup.grab` needs, for the length of one poll turn (docs/adr/0049's
    /// amendment, [`ArmedSerial`]).
    input_serial: Option<ArmedSerial>,
    /// Every `BTN_LEFT` press and release this process has seen, counted (docs/adr/0051's first
    /// amendment). Monotonic and never reset. Makes the dismissal latch clearable: `input_serial`
    /// is cleared at the end of each poll turn, so a later turn has nothing to compare "has the
    /// user asked again" against. Counting both press and release, not just press, means the
    /// reopen works whatever order the compositor batches a dismissal in relative to `popup_done`.
    pointer_input_count: u64,
    /// The focused `secure_submit` field and the surface it lives on, set by the press that focused
    /// a `textfield` (docs/adr/0050 decision 4, `input::focused_target`) or by keyboard focus landing on
    /// a surface with a sole one (`input::sole_secure_submit`). `None` means no frame at all -- see
    /// `input::submit_frame_for`. Written only through [`App::focus_secure_submit`].
    focused_secure_submit: Option<FocusedField>,
    /// Accumulates the focused field's keystrokes until Enter completes them (ADR-0005/
    /// ADR-0009/ADR-0027) -- never surfaced to Lua. Its lifetime belongs to `focused_secure_submit`,
    /// not to any transport event: every write goes through [`App::focus_secure_submit`], which
    /// zeroizes this on any change of destination -- see `input::retarget_secure_submit` for the leak
    /// that rule closes.
    secure_buffer: shared::SecureBuffer,
    /// A keystroke or a focus change has moved what the focused `secure_submit` field should
    /// draw, and no capability push has marked the scene dirty to carry it to the screen.
    ///
    /// Needed because typing changes no property in the retained tree: the bytes live in
    /// `secure_buffer`, outside the scene entirely (ADR-0005), so `re_resolve_if_dirty` has
    /// nothing to notice. Without this the mask would appear only when something unrelated
    /// happened to repaint -- on a lock screen with a clock, once a second, which is worse than
    /// not drawing it at all.
    ///
    /// A repaint, never a re-resolve: the tree is genuinely unchanged, and only the display list
    /// differs (`layout::paint::SecureField` is an input to `build`, not part of the tree). The
    /// list comparison in `App::paint_surface` then narrows this to the one surface holding the
    /// field, so a keystroke repaints the lock screen and nothing else.
    secure_input_changed: bool,
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
    // Optional, unlike layer-shell's: a compositor with no `xdg_wm_base` is legal, and a
    // panel-only config works fine there. `create_surfaces` is what says which `window` went
    // unbuilt, since only it knows there was one.
    let xdg_shell = XdgShell::bind(&globals, &qh)
        .inspect_err(|err| log_bind_failure("<xdg-shell>", "xdg_wm_base::bind", err))
        .ok();
    let output_state = OutputState::new(&globals, &qh);
    let seat_state = SeatState::new(&globals, &qh);
    // Not `?`, not logged: `SessionLockState::new` cannot fail. It stores a `GlobalProxy`, so a
    // missing `ext_session_lock_manager_v1` surfaces only when something asks for a lock
    // (docs/adr/0052 decision 4).
    let session_lock_state = SessionLockState::new(&globals, &qh);
    let registry_state = RegistryState::new(&globals);
    // Stable protocol. `PresentationTimeState::bind` tolerates a compositor that doesn't
    // advertise it -- later `feedback()` calls fail with `GlobalError::MissingGlobal` instead.
    let presentation_time = PresentationTimeState::bind(&globals, &qh);

    let egl_state = egl::init(conn.backend().display_ptr() as *mut c_void)?;

    let is_pba_candidate = std::env::var("OBLISK_PBA_CANDIDATE").is_ok();

    // One `ShapingHandle` for the process: `App` keeps this one, `RendererClient` gets a clone
    // (docs/adr/0039 decision 3). `Loader::new()` runs on this thread because `mlua::Lua` is
    // `!Send`.
    let shaping = ShapingHandle::spawn();
    let client = RendererClient::start(shaping.clone(), outbound_tx.clone(), generation_id)?;

    let mut app = App {
        registry_state,
        output_state,
        compositor_state,
        seat_state,
        layer_shell,
        xdg_shell,
        session_lock_state,
        session_lock: None,
        egl: egl_state,
        gl: None,
        shaping,
        text_painter: None,
        image_cache: ImageCache::new(),
        client,
        surfaces: Vec::new(),
        exit: false,
        is_pba_candidate,
        ready_signal_sent: false,
        startup_complete: false,
        outbound_tx,
        generation_id,
        presentation_time,
        queue_handle: qh.clone(),
        active_nonce: None,
        pointer: None,
        keyboard: None,
        keyboard_focus: None,
        armed: None,
        input_serial: None,
        pointer_input_count: 0,
        focused_secure_submit: None,
        secure_buffer: shared::SecureBuffer::new(),
        secure_input_changed: false,
    };

    // Outputs (and the seat) arrive as a burst of registry + wl_seat/wl_output events after
    // binding; two roundtrips is enough to have both the full initial output list (which
    // `expand_instances` below turns a `monitor = "All"` declaration into one surface per monitor
    // from) and the seat `SeatHandler::new_capability` gets this process's keyboard from.
    event_queue.roundtrip(&mut app)?;
    event_queue.roundtrip(&mut app)?;

    // `oblisk-supervisor-services-dbus.md` § 15.2's Candidate order made literal, which on one
    // thread is just the order of these statements: evaluate shell.lua, bind the layer-shell
    // surfaces the evaluation declared (docs/adr/0038 decision 1), commit null buffers (in
    // `bind_and_clear`'s candidate branch), signal ready (`maybe_send_ready_signal`).
    //
    // ponytail: this runs inside the PBA ready window -- no layer surface exists until it
    // returns, so `maybe_send_ready_signal` cannot fire until after this call, and the
    // Supervisor's `ready_timeout` is 2s (`supervisor/src/main.rs`'s `PBA_TIMINGS`). The first
    // `text` node's shaping blocks on `ShapingHandle::shape` until `FontSystem::new()` finishes,
    // eating into that budget -- accepted cost, not a fix, since § 15.2 requires evaluate-before-
    // bind ordering.
    //
    // `screens` is seeded before the evaluation, not after (docs/adr/0041 decision 2): a config's
    // top-level `for _, screen in ipairs(screens:get())` loop runs during this evaluation, so a
    // list seeded afterwards would declare no per-monitor panels on the first pass.
    let screens = app.screens(None);
    let outputs = geometries_from(&screens);
    app.client.set_screens(screens_payload(&screens));
    let specs = app.client.run_startup_evaluation().unwrap_or_default();
    let instances = expand_instances(&specs, &outputs);
    for spec in &specs {
        let SurfaceSpec::Panel(panel) = spec else {
            // Only a `panel` names a monitor (§ 6.2, § 6.3): the compositor places a toplevel and a
            // popup positions against its parent, so neither can miss one.
            continue;
        };
        if panel.topology.monitor != "All" && !outputs.iter().any(|output| output.name == panel.topology.monitor) {
            // `expand_instances` is pure and returns nothing for a miss; the log belongs here,
            // with the real output list, so an unplugged monitor says so once at startup.
            eprintln!(
                "[oblisk-renderer] surface {:?} targets monitor {:?}, which is not connected; no surface created for it",
                panel.topology.id, panel.topology.monitor
            );
        }
    }
    app.client.set_instances(instances.clone());
    // The first resolve is validation, not anything anyone sees. § 15.2 forces evaluate-before-
    // bind, so no surface is configured yet; each instance resolves against its output's logical
    // size instead. Nothing paints this: a Candidate null-buffers first, and non-candidate mode's
    // first draw happens on the first configure, after `set_instance_size` replaces the size with
    // the compositor's own. A bar is briefly resolved at full screen height here and never painted
    // that way.
    //
    // Both failure modes (evaluation, apply) already logged their own error and set
    // `oblisk.rescue` inside `RendererClient`; this line only adds the consequence.
    if !app.client.apply_instances() {
        eprintln!("[oblisk-renderer] no scene was applied at startup; surfaces still bind, and paint nothing until a reload or a push produces one");
    }

    app.create_surfaces(&qh, &specs, &instances);
    if app.is_pba_candidate {
        // `bind_and_clear`'s configure-driven check misses a generation whose every surface is a
        // `window` with `visible = false`: no `xdg_toplevel` exists to be configured
        // (docs/adr/0049 decision 1), so without this call such a Candidate never announces
        // itself and dies on `ready_timeout`. A no-op otherwise, since the gate refuses this early.
        app.maybe_send_ready_signal();
    }
    // From here on an output event owns the whole job: there is an evaluation to expand and
    // surfaces to reconcile against it (see `App::startup_complete`).
    app.startup_complete = true;

    // A real Wayland event might not arrive for a long time after `ActivateDraw` is sent, since
    // nothing else happens on these mostly-static surfaces once staged, so this loop checks
    // `inbound_rx` on a bounded latency instead of blocking indefinitely on the connection's fd
    // alone. Non-candidate mode's immediate draw on first configure is unaffected: it still
    // happens synchronously inside the `configure` handler, which `dispatch_pending` still calls.
    loop {
        event_queue.dispatch_pending(&mut app)?;
        if app.exit {
            break;
        }
        // Drain, not one-per-pass: every `SupervisorFrame` reaches this thread through this
        // channel (docs/adr/0039), so a burst of `StateSnapshot` pushes must not be spread one
        // per 15ms poll tick.
        //
        // `Disconnected` is a separate answer from `Empty` here (docs/adr/0059 decision 1). It
        // used to be one: `while let Ok(frame)` treated a dead socket thread the same as an idle
        // one, so killing the Supervisor left this process spinning its 15ms poll forever at
        // 17.8% of a core, painting a shell with no capability data and no way to reach one.
        //
        // An `ActivateDraw` nonce is collected here rather than serviced in place: drawing inside
        // the loop body painted whatever layout the scene held at that instant, so a
        // `StateSnapshot` and an `ActivateDraw` arriving in the same drain painted the pre-push
        // layout and only then re-resolved -- the stale frame is what the Supervisor accepted as
        // presentation evidence. A `Vec`, not one nonce: two `ActivateDraw`s in one drain each owe
        // their own `PresentationEvidence`, so none may be dropped by coalescing.
        let mut draw_nonces: Vec<u64> = Vec::new();
        loop {
            let frame = match inbound_rx.try_recv() {
                Ok(frame) => frame,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                // `std::process::exit`, not `app.exit = true`: breaking the loop returns from `run`
                // and drops `App`, and SCTK's `SessionLockInner::Drop` sends a bare
                // `ext_session_lock_v1.destroy`, which is `invalid_destroy` once `locked` has been
                // sent -- the one error docs/adr/0052 exists to avoid. Skipping the destructor
                // closes the connection instead, which the compositor treats as the same lock
                // client death and logs as nothing.
                //
                // `is_some()`, not SCTK's `is_locked()`: the two disagree for the few milliseconds
                // between the `lock` request and the `locked` event being dispatched. `is_some()`
                // can claim a lock not yet granted, sending someone to a VT unnecessarily.
                // `is_locked()` can miss a `locked` that is on the wire but undispatched, telling
                // someone their shell merely died while looking at a lock screen they cannot get
                // past. Over-reporting is the safe half.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // Flush first -- load-bearing, not tidiness. A `SetSessionLock { locked: true }`
                    // serviced earlier in this same drain left an `ext_session_lock_manager_v1.lock`
                    // request sitting in the write buffer: `SessionLockState::lock` only enqueues,
                    // and the turn's only `event_queue.flush()` is below the drain. Exiting from
                    // here would skip it, so the request dies in the buffer while `session_lock` is
                    // already `Some` -- the message below would then claim a locked session the
                    // compositor was never asked for, which this path must never say.
                    //
                    // Flushing sends only requests already decided on. It does not send
                    // `ext_session_lock_v1.destroy`: that lives in SCTK's `Drop`, which
                    // `std::process::exit` skips, keeping the exit clean of `invalid_destroy`.
                    if let Err(err) = event_queue.flush() {
                        eprintln!("[oblisk-renderer] the last flush before exiting failed ({err}); a session lock requested in this same turn may never have reached the compositor");
                    }
                    eprintln!("[oblisk-renderer] {}", supervisor_gone_report(app.session_lock.is_some()));
                    std::process::exit(EXIT_SUPERVISOR_GONE);
                }
            };
            match app.client.handle_frame(frame) {
                FrameOutcome::Handled => {}
                FrameOutcome::ActivateDraw(nonce) => draw_nonces.push(nonce),
                // Serviced here, not collected like a draw nonce: a draw must land after the
                // re-resolve below or it paints the pre-push layout, but a lock reads nothing a
                // re-resolve produces -- whether this config declares a `lock` surface is a fact
                // about the tracked surface set (docs/adr/0052 decision 3) that no capability push
                // changes. Deferring would cost a poll turn on the one command whose whole point is
                // that the screen goes secure now.
                FrameOutcome::SetSessionLock(locked) => {
                    // A round trip before an unlock, and only before an unlock. SCTK gates
                    // `SessionLock::unlock` on `ext_session_lock_v1::locked` being dispatched, not
                    // sent, and this drain runs in a different turn from `dispatch_pending` above,
                    // so `locked` may already be on the wire but undispatched. `unlock()` would then
                    // be a silent no-op and the `Drop` right after would send the plain `destroy`
                    // the protocol XML forbids once `locked` was sent -- `invalid_destroy`, which
                    // kills the connection with the session still locked, the state docs/adr/0052
                    // exists to prevent. `roundtrip` closes it: a `wl_callback` cannot arrive before
                    // everything sent earlier.
                    //
                    // The acquire path pays none of this: its inputs are the tracked surface set
                    // and `session_lock.is_some()`, both owned by this thread, and an undispatched
                    // `locked` can only make `session_lock` already `Some`, which [`lock_command`]
                    // answers `Nothing`.
                    //
                    // Deliberately not `?`: propagating here would return from `run` between the
                    // correct password and `unlock_and_destroy`, killing the client with the session
                    // still locked, and the compositor does not unlock when a lock client dies. Every
                    // `DispatchError` this can raise means the connection is already broken, so the
                    // unlock attempt below may send nothing -- but attempting it costs one failed
                    // flush and beats exiting without trying.
                    if !locked && let Err(err) = event_queue.roundtrip(&mut app) {
                        eprintln!(
                            "[oblisk-renderer] the round trip before an unlock failed ({err}); attempting the unlock anyway rather than exiting with the session locked"
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
        // Once per turn, after the drain above empties `inbound_rx`, not inside that loop's body
        // (ADR-0044 decision 2). A burst of `StateSnapshot` pushes marks the dirty flag repeatedly
        // while draining, but `DirtyFlag::take` only reports it once, so this coalesces the burst
        // into one `Scene::apply` per poll turn, landing before this turn's draw.
        //
        // A re-resolve that changed the retained scene is repainted immediately. This is not
        // frame-pending gating (`wl_surface::frame()`, which blocks the loop when idle instead of
        // waking on the 15ms poll) -- it is the other half, the one that makes a capability push
        // reach the screen at all rather than stopping at a resolved tree in memory.
        // Two statements, the two halves of one commit. The first stages everything the
        // re-resolve changed about each surface -- layer-shell fields permitted to change in
        // place, the input region, whether it is mapped (docs/adr/0038 decision 2). All of that
        // is double-buffered `wl_surface` state, so none of it takes effect until the second
        // statement's `swap_buffers` commits it. Committing per field would show the compositor a
        // half-updated surface between requests.
        let re_resolved = app.client.re_resolve_if_dirty();
        // Taken unconditionally so a keystroke that arrived alongside a capability push does not
        // stay pending: the repaint below covers both, and leaving the flag set would repaint
        // again next turn for nothing.
        let typed = std::mem::take(&mut app.secure_input_changed);
        if re_resolved {
            app.apply_resolved_surface_state();
        }
        if re_resolved || typed {
            app.repaint_mapped_surfaces();
        }
        // The disarm half of docs/adr/0049's amendment, and it must be here, not inside the `if`
        // above. `dispatch_pending` armed `input_serial` if a `BTN_LEFT` press or release arrived
        // this turn; `apply_resolved_surface_state` above is the only reader, since it is the only
        // thing that creates a popup. Clearing unconditionally makes "a popup may only open in
        // response to real user input" fall out of the mechanism: a D-Bus notification marking the
        // scene dirty finds nothing armed on its later re-resolve, and a `grab = true` popup it
        // tries to open is refused. Clearing inside the `if` would leak a click's serial across
        // every turn until the next re-resolve.
        app.input_serial = None;
        // Once a turn, so a focused `secure_submit` field whose surface this process tore down --
        // a lock screen the compositor `finished`, a `window` whose `visible` went false -- doesn't
        // sit holding a half-typed password until a later keystroke notices. The load-bearing
        // check is in `App::apply_secure_key`; this is the narrower residency ceiling.
        app.drop_secure_focus_if_its_surface_is_gone();
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

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

// This SCTK (`smithay-client-toolkit-0.21.1`, checked against `src/`) ships exactly two
// `delegate_*` macros: `delegate_dispatch2!` and `delegate_registry!`. `PointerData`,
// `KeyboardData`, `WindowData`, `PopupData`, `GlobalData` and `SessionLockData`/
// `SessionLockSurfaceData` each carry their own blanket `Dispatch2` impl, which the line below
// turns into the `Dispatch` half every bind/create call needs -- so `PointerHandler`,
// `KeyboardHandler`, `WindowHandler`, `PopupHandler` and `SessionLockHandler` are implemented
// above with no matching `delegate_pointer!`/`delegate_keyboard!`/`delegate_xdg_shell!`/
// `delegate_xdg_popup!`/`delegate_session_lock!` call: none of those macros exist in this SCTK to
// add, and each trait is the only half left to supply.
delegate_registry!(App);
smithay_client_toolkit::delegate_dispatch2!(App);
