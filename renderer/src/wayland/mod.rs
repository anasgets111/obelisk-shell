pub mod egl;

use std::collections::HashMap;
use std::error::Error;
use std::ffi::c_void;

use khronos_egl::Surface as EglSurface;
use mlua::{Function, Lua, Table, Value};
use shared::{
    LockOutcome, LockReport, PresentationEvidence, ReadySignal, RendererFrame, SecureSubmit, SupervisorFrame, Zeroize,
};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
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
use wayland_protocols::wp::presentation_time::client::wp_presentation_feedback;
use wayland_protocols::xdg::shell::client::{xdg_positioner, xdg_surface};

use crate::image::ImageCache;
use crate::layout;
use crate::layout::instance::{OutputGeometry, SurfaceInstance, expand_instances, is_instance_of, reconcile_instances};
use crate::layout::node::{
    self, ConstraintAdjustment, LayerKind, PanelSpec, PopupAnchor, PopupSpec, SizeHint, SizeMode, SurfaceSpec,
    WindowSpec,
};
use crate::socket::{FrameOutcome, RendererClient};
use crate::text::atlas::TextPainter;
use crate::text::shaping::ShapingHandle;
use crate::text::snap::LogicalRect;

mod idle_profile;
mod input;
mod layer;
mod lock;
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
    /// `xdg_wm_base`, plus the `zxdg_decoration_manager_v1` `XdgShell::bind` picks up alongside it.
    /// `None` on a compositor advertising no xdg-shell: a `panel`-only config still works there,
    /// and a declared `window` says so once instead of taking the process down.
    xdg_shell: Option<XdgShell>,
    /// `ext_session_lock_manager_v1`, or its absence (ADR-0042). Unlike `xdg_shell`, not an
    /// `Option`: SCTK wraps it in a `GlobalProxy`, so absence surfaces only as
    /// `GlobalError::MissingGlobal` from `lock` (ADR-0052 decision 4), not a startup bind
    /// failure. Also not a `RegistryHandler`: it binds once from the `GlobalList` in [`run`].
    session_lock_state: SessionLockState,
    /// The live `ext_session_lock_v1`, from the moment `lock` is sent until the lock ends: an
    /// unlock the Supervisor ordered, a denial, or a compositor teardown. `Some` with
    /// `is_locked()` still false is the in-flight window between request and answer, which is
    /// why `finished` is two different events (ADR-0042); see `lock::finished_outcome`.
    session_lock: Option<SessionLock>,
    /// The shared EGL display, config and GLES3 context, or `None` until a surface needs one.
    /// Lazy because `eglInitialize` loads Mesa's driver, `libgallium` plus the LLVM it links here:
    /// 125 MB of mapped pages and 13-35 ms. A config declaring no surfaces (ADR-0070 decision 7)
    /// never pays; a PBA Candidate pays after `ActivateDraw`, not its ready window, since
    /// `activate_draw_one` is its only bind (ADR-0071).
    egl: Option<egl::EglState>,
    gl: Option<glow::Context>,
    /// Kept for the `wl_display` pointer [`App::ensure_egl`] needs, and kept as the whole
    /// `Connection` rather than that raw pointer so the refcount is what guarantees `egl::init`'s
    /// SAFETY precondition: the display outlives the EGL state built against it.
    conn: Connection,
    /// The one `ShapingHandle` for the process; `client` holds a clone, so content-sizing and
    /// painting share one worker thread and one `FontSystem` (ADR-0039 decision 3).
    shaping: ShapingHandle,
    text_painter: Option<TextPainter>,
    /// One image cache for the process, keyed by file path and pixel size, so an icon drawn on
    /// the bar and again in a popup is one upload, not one per surface (`CONTEXT.md`, **Image
    /// cache**).
    image_cache: ImageCache,
    /// The Lua VM, `Loader`, retained `Scene`, live signals and reload bookkeeping (ADR-0039).
    /// `mlua::Lua` is `!Send`, so `App` is too: fine, since `wayland-client` puts no `Send` bound
    /// on the dispatch state.
    client: RendererClient,
    surfaces: Vec<TrackedSurface>,
    exit: bool,
    /// `OBLISK_PBA_CANDIDATE` is set (§ 15.2), read once in [`run`], not re-read per configure.
    is_pba_candidate: bool,
    /// Set once [`App::maybe_send_ready_signal`] has sent `ReadySignal`: a one-time signal, never
    /// resent even if a later spurious configure re-triggers the check.
    ready_signal_sent: bool,
    /// Set once [`run`]'s startup sequence has evaluated the config and built its surfaces: the
    /// initial `wl_output` burst dispatches inside `run`'s own two roundtrips, before the
    /// evaluation that seeds `screens` (ADR-0041 decision 2), so [`App::handle_output_change`]
    /// must not run its full job that early, since no evaluation exists yet to expand.
    startup_complete: bool,
    /// Every frame this thread sends the Supervisor goes here; the socket thread's `pump` drains
    /// it and writes it to the wire. `UnboundedSender::send` is synchronous and non-blocking, so
    /// it's safe to call from inside a `Dispatch` callback.
    outbound_tx: tokio::sync::mpsc::UnboundedSender<RendererFrame>,
    /// This Renderer's own generation id, stamped into every `SecureSubmit` it writes, read once
    /// in `main` from `OBLISK_GENERATION_ID`.
    generation_id: u32,
    presentation_time: PresentationTimeState,
    /// Cloned once in [`run`] so [`App::activate_draw`], called from the poll loop rather than a
    /// `Dispatch` callback, can still request `wp_presentation_feedback`.
    queue_handle: QueueHandle<App>,
    /// The `ActivateDraw` nonce currently being drawn, if any, tags every
    /// `wp_presentation_feedback` `presented` event while in flight. PBA drives one handshake at
    /// a time, so one field, not a per-surface map, is enough.
    active_nonce: Option<u64>,
    /// The seat's pointer, once advertised. Kept alive: dropping the proxy destroys the protocol
    /// object and every `enter`/`press`/`release` with it. One, not one per seat:
    /// [`SeatHandler::new_capability`] puts whichever seat announces the capability in this slot.
    pointer: Option<ThemedPointer>,
    /// The shape the pointer was last given over one of this process's surfaces (ADR-0107).
    /// `None` between surfaces: `wp_cursor_shape_v1` wants the shape re-sent on every `enter`,
    /// so `Leave` clears this and the next `Enter` always sends.
    cursor_shown: Option<cursor_icon::CursorIcon>,
    /// Where the pointer last was, and on which of this process's surfaces (ADR-0112 amendment):
    /// set by `Enter` and `Motion`, cleared by `Leave`. What a re-resolve rewrites the hover signals
    /// against, since a list that scrolled under a still pointer moved other rows under it and no
    /// `Motion` is coming to say so.
    pointer_at: Option<(String, (f64, f64))>,
    /// `wl_shm`, bound only so a compositor without `wp_cursor_shape_v1` can still be handed a
    /// cursor image from the XCursor theme. This process draws through EGL and puts nothing else
    /// in shared memory.
    shm: Shm,
    /// The seat's keyboard, once advertised, kept alive and single-seat for the same reason as
    /// `pointer`. This shell reads no keys off it directly; it's bound only for `enter`/`leave`,
    /// the only way a client learns which surface `keyboard_interactivity` actually won focus for.
    keyboard: Option<wl_keyboard::WlKeyboard>,
    /// The instance id of the surface holding keyboard focus, if any (ADR-0050's consequences).
    /// `input::focus_is_still_armed` reads it on every keystroke: a `secure_submit` field is
    /// armed only while the surface that declared it is the one this names.
    ///
    /// ponytail: nothing else consumes it (§ 5.2 has no `on_key`; ADR-0050 declines to invent
    /// one). Upgrade path: an IDL key-handler property, dispatching into this surface's tree.
    keyboard_focus: Option<String>,
    /// The press waiting for its release, if any (ADR-0050 decision 2, [`ArmedClick`]).
    armed: Option<ArmedClick>,
    /// The left press held on an `on_drag` button, if any (ADR-0116 decision 1,
    /// [`input::ActiveDrag`]). Every `Motion` reports to it until the release or a `Leave`.
    drag: Option<input::ActiveDrag>,
    /// The serial `xdg_popup.grab` needs, for the length of one poll turn (ADR-0049's
    /// amendment, [`ArmedSerial`]).
    input_serial: Option<ArmedSerial>,
    /// Every `BTN_LEFT` press and release this process has seen, counted and never reset
    /// (ADR-0051's first amendment): since `input_serial` clears each poll turn, counting both
    /// press and release gives a later turn something to compare "has the user asked again"
    /// against, whatever order the compositor batches a dismissal in relative to `popup_done`.
    pointer_input_count: u64,
    /// The focused `secure_submit` field and the surface it lives on, set by the press that
    /// focused a `textfield` (ADR-0050 decision 4, `input::focused_target`) or by keyboard focus
    /// landing on a surface with a sole one (`input::sole_secure_submit`). `None` means no frame
    /// at all; see `input::submit_frame_for`. Written only through [`App::focus_secure_submit`].
    focused_secure_submit: Option<FocusedField>,
    /// The focused *plain* `textfield` -- the unmasked half of § 5.2 item 8 -- and the text typed
    /// into it so far (ADR-0092). Set only by a press landing on one (`input::focused_field`);
    /// unlike the masked half there is no arm-on-`enter` fallback, because "the sole field in
    /// scope" is a rule that cannot serve a list of reply boxes.
    ///
    /// Mutually exclusive with `focused_secure_submit` by construction: one press decides, and the
    /// innermost `textfield` it lands on is one kind or the other.
    focused_text_field: Option<FocusedTextField>,
    /// Accumulates the focused field's keystrokes until Enter completes them (ADR-0005/ADR-0009/
    /// ADR-0027), never surfaced to Lua. Lifetime belongs to `focused_secure_submit`: every write
    /// goes through [`App::focus_secure_submit`], which zeroizes this on any destination change;
    /// see `input::retarget_secure_submit` for the leak that rule closes.
    secure_buffer: shared::SecureBuffer,
    /// A keystroke or focus change moved what the focused `textfield` should draw, with no
    /// capability push yet to mark the scene dirty. Typing changes no property in the retained tree
    /// -- a masked field's bytes live in `secure_buffer` and a plain one's in `focused_text_field`,
    /// both outside the scene (ADR-0005, ADR-0092) -- so `re_resolve_if_dirty` misses it;
    /// otherwise the mask or the caret would appear only on an unrelated repaint, once a second on
    /// a lock screen clock. A repaint, never a re-resolve: the tree is unchanged, only the display
    /// list differs (`layout::paint::FieldFocus` is an input to `build`, not part of the tree),
    /// narrowed by `App::paint_surface`'s comparison to the one surface holding the field.
    field_input_changed: bool,
    /// How many surfaces `App::paint_surface` actually drew and swapped since the loop last took
    /// this, for `OBLISK_PROFILE_IDLE`. Counted here rather than returned because the paint walks
    /// every mapped surface and declines most of them, so the interesting number is the count
    /// across the walk, not any one surface's answer; taken and reset like `field_input_changed`.
    surfaces_drawn: usize,
}

/// The Renderer's main thread: Wayland dispatch, EGL, and (since ADR-0039) the Lua VM, the
/// retained `Scene`, and the live signals. `inbound_rx` carries `SupervisorFrame`s decoded by the
/// socket thread; `outbound_tx` carries every frame this thread sends back.
pub fn run(
    generation_id: u32,
    inbound_rx: std::sync::mpsc::Receiver<SupervisorFrame>,
    outbound_tx: tokio::sync::mpsc::UnboundedSender<RendererFrame>,
    waker: crate::wake::Waker,
) -> Result<(), Box<dyn Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init::<App>(&conn)?;
    let qh = event_queue.handle();

    let compositor_state = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    // Optional, unlike layer-shell's: a compositor with no `xdg_wm_base` is legal, and a
    // panel-only config works fine there. `create_surfaces` is what says which `window` went
    // unbuilt, since only it knows there was one.
    let xdg_shell =
        XdgShell::bind(&globals, &qh).inspect_err(|err| log_bind_failure("<xdg-shell>", "xdg_wm_base::bind", err)).ok();
    let output_state = OutputState::new(&globals, &qh);
    let seat_state = SeatState::new(&globals, &qh);
    // Stable and mandatory: every compositor advertises `wl_shm`, so `?` here is right.
    let shm = Shm::bind(&globals, &qh)?;
    // Not `?`, not logged: `SessionLockState::new` cannot fail. It stores a `GlobalProxy`, so a
    // missing `ext_session_lock_manager_v1` surfaces only when something asks for a lock
    // (ADR-0052 decision 4).
    let session_lock_state = SessionLockState::new(&globals, &qh);
    let registry_state = RegistryState::new(&globals);
    // Stable protocol. `PresentationTimeState::bind` tolerates a compositor that doesn't
    // advertise it: later `feedback()` calls fail with `GlobalError::MissingGlobal` instead.
    let presentation_time = PresentationTimeState::bind(&globals, &qh);

    let is_pba_candidate = std::env::var("OBLISK_PBA_CANDIDATE").is_ok();

    // One `ShapingHandle` for the process: `App` keeps this one, `RendererClient` gets a clone
    // (ADR-0039 decision 3). `Loader::new()` runs on this thread because `mlua::Lua` is
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
        egl: None,
        gl: None,
        conn: conn.clone(),
        shaping,
        text_painter: None,
        image_cache: ImageCache::with_waker(waker.clone()),
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
        cursor_shown: None,
        pointer_at: None,
        shm,
        keyboard: None,
        keyboard_focus: None,
        armed: None,
        drag: None,
        input_serial: None,
        pointer_input_count: 0,
        focused_secure_submit: None,
        focused_text_field: None,
        secure_buffer: shared::SecureBuffer::new(),
        field_input_changed: false,
        surfaces_drawn: 0,
    };

    // Outputs (and the seat) arrive as a burst of registry + wl_seat/wl_output events after
    // binding; two roundtrips is enough for both the full initial output list (`expand_instances`
    // below turns a `monitor = "All"` declaration into one surface per monitor from) and the
    // keyboard `SeatHandler::new_capability` gets this process from the seat.
    event_queue.roundtrip(&mut app)?;
    event_queue.roundtrip(&mut app)?;

    // `oblisk-supervisor-services-dbus.md` § 15.2's Candidate order made literal: evaluate
    // shell.lua, bind the layer-shell surfaces it declared (ADR-0038 decision 1), commit null
    // buffers (`bind_and_clear`'s candidate branch), signal ready (`maybe_send_ready_signal`).
    //
    // ponytail: runs inside the PBA ready window (Supervisor `ready_timeout` 2s,
    // `supervisor/src/main.rs`'s `PBA_TIMINGS`); the first `text` node's shaping blocks on
    // `FontSystem::new()`, eating into that budget. Accepted cost: § 15.2 requires
    // evaluate-before-bind regardless.
    //
    // `screens` is seeded before the evaluation, not after (ADR-0041 decision 2): a config's
    // top-level `for _, screen in ipairs(screens:get())` loop runs during this evaluation, so
    // seeding afterwards would declare no per-monitor panels on the first pass.
    let screens = app.screens(None);
    let outputs = geometries_from(&screens);
    app.client.set_screens(screens_payload(&screens));
    let specs = app.client.run_startup_evaluation().unwrap_or_default();
    // After the evaluation, since `fonts { ... }` is a global the config calls; before any surface
    // paints, since `TextPainter` loads the chain lazily on first paint and so picks this up
    // unprompted (`ShapingHandle::set_chain` says why that ordering makes a rebuild beat a
    // respawn). A config declaring nothing leaves the default chain standing.
    app.shaping.set_chain(&crate::lua::fonts::declared_chain(app.client.lua()));
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
    // size instead, never painted. Both failure modes (evaluation, apply) already logged their
    // own error and set `oblisk.rescue` inside `RendererClient`; this line only adds the
    // consequence.
    if !app.client.apply_instances() {
        eprintln!(
            "[oblisk-renderer] no scene was applied at startup; surfaces still bind, and paint nothing until a reload or a push produces one"
        );
    }

    app.create_surfaces(&qh, &specs, &instances);
    if app.is_pba_candidate {
        // `bind_and_clear`'s configure-driven check misses a generation whose every surface is a
        // `window` with `visible = false`: no `xdg_toplevel` exists to configure (ADR-0049
        // decision 1), so without this call such a Candidate never announces itself and dies on
        // `ready_timeout`. A no-op otherwise, since the gate refuses this early.
        app.maybe_send_ready_signal();
    }
    // From here on an output event owns the whole job: there is an evaluation to expand and
    // surfaces to reconcile against it (see `App::startup_complete`).
    app.startup_complete = true;

    // `None` unless `OBLISK_PROFILE_IDLE` is set; see that module for what it answers and why
    // this loop is the thing worth asking.
    let mut profile = idle_profile::IdleProfile::from_env();

    // A real Wayland event might not arrive for long after `ActivateDraw` is sent, since nothing
    // else happens on these mostly-static surfaces once staged, so this loop checks `inbound_rx`
    // on bounded latency instead of blocking indefinitely on the connection's fd. Non-candidate
    // mode's immediate draw on first configure is unaffected: still synchronous inside
    // `dispatch_pending`'s `configure` handler.
    loop {
        let dispatched = event_queue.dispatch_pending(&mut app)? > 0;
        if app.exit {
            break;
        }
        // Drain, not one-per-pass: every `SupervisorFrame` reaches this thread through this
        // channel (ADR-0039), so a burst of `StateSnapshot` pushes must not spread one per
        // wakeup. `Disconnected` is a separate answer from `Empty` (ADR-0059 decision 1):
        // treating a dead socket thread as idle would leave this process blocked in `poll` with
        // no capability data and no way to reach one. An `ActivateDraw`
        // nonce is collected here, not serviced in place, since drawing inside the loop body
        // would paint the pre-push layout when a `StateSnapshot` and `ActivateDraw` share a
        // drain; a `Vec`, not one nonce, since two `ActivateDraw`s in one drain each owe their
        // own `PresentationEvidence`.
        let mut draw_nonces: Vec<u64> = Vec::new();
        loop {
            let frame = match inbound_rx.try_recv() {
                Ok(frame) => frame,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                // `std::process::exit`, not `app.exit = true`: breaking the loop drops `App`, and
                // SCTK's `SessionLockInner::Drop` sends a bare `ext_session_lock_v1.destroy`,
                // `invalid_destroy` once `locked` has been sent, the one error ADR-0052 exists to
                // avoid; skipping the destructor closes the connection instead, logged as an
                // ordinary lock client death. `is_some()`, not SCTK's `is_locked()`: the two
                // disagree for a few milliseconds around `locked`'s dispatch, so `is_some()` may
                // send someone to a VT unnecessarily while `is_locked()` may miss an undispatched
                // `locked` and claim the shell merely died behind a lock screen they can't get
                // past. Over-reporting is the safer half.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // Flush first, load-bearing not tidiness: `SessionLockState::lock` only
                    // enqueues, and this turn's only `event_queue.flush()` is below the drain, so
                    // a `SetSessionLock { locked: true }` serviced earlier would leave its `lock`
                    // request dying in the write buffer while `session_lock` is already `Some`,
                    // falsely claiming below a locked session the compositor was never asked for.
                    // This sends only decided requests, never `ext_session_lock_v1.destroy`:
                    // that's SCTK's `Drop`, which `std::process::exit` skips.
                    if let Err(err) = event_queue.flush() {
                        eprintln!(
                            "[oblisk-renderer] the last flush before exiting failed ({err}); a session lock requested in this same turn may never have reached the compositor"
                        );
                    }
                    eprintln!("[oblisk-renderer] {}", supervisor_gone_report(app.session_lock.is_some()));
                    std::process::exit(EXIT_SUPERVISOR_GONE);
                }
            };
            match app.client.handle_frame(frame) {
                FrameOutcome::Handled => {}
                FrameOutcome::ActivateDraw(nonce) => draw_nonces.push(nonce),
                // Serviced here, not collected like a draw nonce: a lock reads nothing a
                // re-resolve produces, since whether this config declares a `lock` surface is a
                // fact about the tracked surface set (ADR-0052 decision 3) that no capability push
                // changes. Deferring would cost a poll turn on the one command whose whole point
                // is that the screen goes secure now.
                FrameOutcome::SetSessionLock(locked) => {
                    // A round trip before an unlock only: SCTK gates `SessionLock::unlock` on
                    // `ext_session_lock_v1::locked` being dispatched, not sent, and this drain
                    // runs a turn after `dispatch_pending`, so `locked` may be undispatched.
                    // `unlock()` would then no-op, and the `Drop` right after sends the plain
                    // `destroy` the protocol forbids once `locked` was sent: `invalid_destroy`,
                    // killing the connection with the session still locked (ADR-0052).
                    // `roundtrip` closes this gap. The acquire path needs none of it: its inputs,
                    // the tracked surface set and `session_lock.is_some()`, are owned by this
                    // thread, answered `Nothing` by [`lock_command`] when already `Some`.
                    // Deliberately not `?`: returning here would strand the client mid-unlock,
                    // locked forever, since the compositor never unlocks a dead lock client. A
                    // broken `DispatchError` here means trying below costs at most one failed
                    // flush.
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
        // Once per turn, after the drain empties `inbound_rx`, not inside that loop's body
        // (ADR-0044 decision 2): `DirtyFlag::take` only reports the flag once, coalescing a burst
        // of `StateSnapshot` pushes into one `Scene::apply` per turn. Not frame-pending gating
        // (`wl_surface::frame()`, blocks the loop when idle instead of waking on the 15ms poll):
        // this carries a capability push to the screen instead of stopping at a resolved tree,
        // and since ADR-0124 the push itself is the wakeup.
        // Two statements, the two halves of one commit: the first stages what the re-resolve
        // changed (layer-shell fields, the input region, whether mapped, ADR-0038 decision 2),
        // double-buffered `wl_surface` state the second statement's `swap_buffers` commits;
        // per-field commits would show the compositor a half-updated surface.
        // Off unless the profile is on, in which case three `clock_gettime` calls a turn buy the
        // split between re-resolving the tree and drawing it -- the difference between knowing the
        // loop woke and knowing what it spent the wake on.
        let mut phases = idle_profile::Phases::start(profile.is_some());
        let re_resolved = app.client.re_resolve_if_dirty();
        phases.mark_resolve();
        // Taken unconditionally so a keystroke that arrived alongside a capability push does not
        // stay pending: the repaint below covers both, and leaving the flag set would repaint
        // again next turn for nothing.
        let typed = std::mem::take(&mut app.field_input_changed);
        // A background decode landing changes no property in the retained tree and no display
        // list either, since a list names the file and not the texture, so it is its own repaint
        // cue and its own invalidation (ADR-0122): the surfaces whose last list draws a landed
        // file forget that list, and the repaint below stops skipping them.
        let landed = app.image_cache.poll();
        if !landed.is_empty() {
            app.forget_painted_lists_drawing(&landed);
        }
        if re_resolved {
            app.apply_resolved_surface_state();
            // The tree under the pointer may have moved without the pointer doing so: hover
            // signals follow the layout, `on_hover` follows the pointer (ADR-0112 amendment).
            app.refresh_hover_after_layout();
        }
        phases.mark_surface_state();
        if re_resolved || typed || !landed.is_empty() {
            app.repaint_mapped_surfaces();
        }
        phases.mark_repaint();
        // Whether anything at all happened this turn. A turn with no Wayland event, no frame, no
        // keystroke and no landed decode changed nothing the three checks below read, so they are
        // skipped (ADR-0124): `arm_autofocus_if_nothing_is_typing` in particular copies the
        // focused surface's whole tree out of the scene to look for a field, which at sixty-six
        // turns a second on an open picker was most of what the process did.
        let active = dispatched || re_resolved || typed || !landed.is_empty() || !draw_nonces.is_empty();
        if let Some(profile) = profile.as_mut() {
            // Before the `draw_nonces` loop below consumes the `Vec`, and before any `break`, so a
            // turn that exits still reports the work it did.
            profile.turn(
                idle_profile::Turn {
                    dispatched,
                    re_resolved,
                    typed,
                    decoded: !landed.is_empty(),
                    draws: draw_nonces.len(),
                    painted: re_resolved || typed || !landed.is_empty(),
                    drawn: std::mem::take(&mut app.surfaces_drawn),
                },
                phases,
            );
        }
        // The disarm half of ADR-0049's amendment; must be here, not inside the `if` above.
        // `dispatch_pending` armed `input_serial` on a `BTN_LEFT` press or release this turn, and
        // `apply_resolved_surface_state` is the only reader, since it's the only thing that
        // creates a popup. Clearing unconditionally makes "a popup may only open in response to
        // real user input" fall out of the mechanism, rather than leaking a click's serial across
        // turns.
        app.input_serial = None;
        // Once a turn, so a focused `secure_submit` field whose surface this process tore down (a
        // lock screen the compositor `finished`, a `window` whose `visible` went false) doesn't
        // sit holding a half-typed password until a later keystroke notices. The load-bearing
        // check is in `App::apply_secure_key`; this is the narrower residency ceiling.
        if active {
            app.drop_secure_focus_if_its_surface_is_gone();
            // Its opposite: a `secure_submit` field that became visible under a keyboard focus
            // that had already arrived gets no `enter` of its own to arm it.
            app.arm_secure_focus_if_the_scope_now_declares_one();
            app.arm_autofocus_if_nothing_is_typing();
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
        event_queue.flush()?;
        if let Some(guard) = event_queue.prepare_read() {
            let fd = guard.connection_fd();
            // No timeout (ADR-0124): everything that gives this loop work arrives on one of these
            // two fds. Wayland events on the connection; a Supervisor frame, a landed decode or
            // the socket thread's exit through the waker. Nothing in the loop body keeps time.
            let mut fds = [
                nix::poll::PollFd::new(fd, nix::poll::PollFlags::POLLIN),
                nix::poll::PollFd::new(waker.fd(), nix::poll::PollFlags::POLLIN),
            ];
            let woke = matches!(nix::poll::poll(&mut fds, nix::poll::PollTimeout::NONE), Ok(n) if n > 0);
            let wayland_ready = woke && fds[0].any().unwrap_or(false);
            if let Some(profile) = profile.as_mut() {
                profile
                    .wake(idle_profile::Wake { wayland: wayland_ready, waker: woke && fds[1].any().unwrap_or(false) });
            }
            if woke {
                if wayland_ready {
                    guard.read()?;
                }
                // Before the turn, not after: a wake that lands while the turn runs must survive
                // it, and the eventfd's count does once this one is cleared.
                waker.drain();
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
// `SessionLockSurfaceData` each carry a blanket `Dispatch2` impl, which the line below turns into
// the `Dispatch` half every bind/create call needs. So `PointerHandler`, `KeyboardHandler`,
// `WindowHandler`, `PopupHandler` and `SessionLockHandler` are implemented above with no
// `delegate_pointer!`/`delegate_keyboard!`/`delegate_xdg_shell!`/`delegate_xdg_popup!`/
// `delegate_session_lock!` call to match: none of those macros exist in this SCTK.
delegate_registry!(App);
smithay_client_toolkit::delegate_dispatch2!(App);

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}
