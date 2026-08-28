pub mod egl;

use std::collections::HashMap;
use std::error::Error;
use std::ffi::c_void;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::dispatch2::Dispatch2;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::presentation_time::{PresentTime, PresentationTimeHandler, PresentationTimeState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::pointer::{BTN_LEFT, PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::{delegate_registry, registry_handlers};
use khronos_egl::Surface as EglSurface;
use mlua::{Function, Lua, Table, Value};
use wayland_client::globals::{registry_queue_init, GlobalList};
use wayland_client::protocol::{wl_output, wl_pointer, wl_seat, wl_surface};
use wayland_client::{Connection, Proxy, QueueHandle, WEnum};
use wayland_egl::WlEglSurface;
use wayland_protocols::wp::presentation_time::client::wp_presentation_feedback;
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3::{self, ZwpTextInputManagerV3},
    zwp_text_input_v3::{self, ZwpTextInputV3},
};
use shared::{PresentationEvidence, ReadySignal, RendererFrame, SecureSubmit, SupervisorFrame, Zeroize};

use crate::layout;
use crate::layout::instance::{OutputGeometry, SurfaceInstance, expand_instances, reconcile_instances};
use crate::layout::node::{self, LayerKind, PanelSpec, SizeMode};
use crate::socket::RendererClient;
use crate::text::atlas::TextPainter;
use crate::text::shaping::ShapingHandle;
use crate::text::snap::LogicalRect;

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
///
/// That order is a necessary condition, not a sufficient one. `khronos_egl::Surface` is a plain
/// copyable handle with no `Drop` of its own, so dropping this struct destroys the
/// `wl_egl_window` and nothing else; the matching `eglDestroySurface` is
/// [`App::destroy_surface_by_id`]'s job, which is why that is the only sanctioned way to retire
/// a bound surface.
struct BoundSurface {
    egl_surface: EglSurface,
    #[allow(dead_code)]
    native_window: WlEglSurface,
}

/// Logs an EGL/Wayland bind-time failure in a consistent shape across `bind_and_clear`'s
/// fallible steps. Takes the surface id rather than a role since docs/adr/0038 deleted the roles:
/// the id is `"{id}@{output}"`, which names the config's own surface *and* the monitor it failed
/// on, where a role could name neither.
fn log_bind_failure(surface_id: &str, stage: &str, err: impl std::fmt::Display) {
    eprintln!("[oblisk-renderer] {surface_id}: {stage} failed: {err}");
}

/// § 6.1's `visible`, as the compositor currently sees it (docs/adr/0038 decision 2: within a live
/// generation `visible` maps and unmaps a surface without destroying it, so toggling a launcher
/// costs a commit rather than a process spawn).
///
/// Three states rather than two, and the middle one is the protocol's, not a convenience.
/// `zwlr_layer_surface_v1`'s own description spells the re-map procedure out: "The client can
/// re-map the surface by performing a commit without any buffer attached, waiting for a configure
/// event and handling it as usual." Attaching a buffer before that configure arrives would break
/// that rule, and the same wait applies to a surface's very first map, so both share this state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MapState {
    /// The resolved root's `visible` is `false`. Either no buffer was ever attached (a panel
    /// declared invisible at startup) or a null buffer unmapped it. Nothing may be painted here,
    /// and -- see [`App::unmap`] -- nothing may be *committed* here either, since a commit with no
    /// buffer attached is exactly how the protocol says a client re-maps.
    Unmapped,
    /// The map (or re-map) commit is out and the compositor has not configured the surface yet.
    AwaitingConfigure,
    /// Configured: [`App::paint_surface`] may attach a buffer, and its `swap_buffers` is the
    /// commit every other staged request rides on.
    Mapped,
}

impl MapState {
    /// Whether this surface will put a frame on screen, which is the one predicate PBA's expected
    /// set and PBA's drawn set must agree on -- see [`presenting_surface_ids`].
    fn presents(self) -> bool {
        self != MapState::Unmapped
    }
}

struct TrackedSurface {
    layer: LayerSurface,
    bound: Option<BoundSurface>,
    /// § 15's "surface_id", and since docs/adr/0038 the *instance* id
    /// (`layout::instance::SurfaceInstance::instance_id`): `"{id}@{output}"`, built from the
    /// config's own `id`. This is the one id space Lua, the retained `Scene`, this `wl_surface`,
    /// and the PBA handshake all share -- before this it was a fixed Rust-owned role's label, which
    /// overlapped none of them, which is why `layout::paint::paint_tree` had no caller.
    surface_id: String,
    /// The `panel` spec this surface's layer-shell state was last set from -- the diff baseline
    /// [`spec_update`] compares a freshly resolved root against, so a re-resolve pushes only the
    /// fields that actually moved (docs/adr/0038 decision 2, build-steps.md Phase 20 item 1).
    ///
    /// Also the standing answer to "what is this surface's anchor and is it exclusive", which
    /// [`App::apply_exclusive_zone`] needs once a `configure` says how large the surface is.
    applied_spec: PanelSpec,
    /// This surface's output's logical size, the basis a `SizeMode::Percent` resolves against.
    ///
    /// Kept per surface rather than read back from `SurfaceInstance::available`, which is the same
    /// number only until the first `configure`: `set_instance_size` then replaces `available` with
    /// the size the compositor granted, so resolving a percent against it on a later re-resolve
    /// would take a percentage of a percentage and shrink the surface on every push.
    output_size: layout::LogicalSize,
    map_state: MapState,
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
    /// Set once [`run`]'s startup sequence has evaluated the config and built its surfaces.
    ///
    /// The initial `wl_output` burst is dispatched inside `run`'s own two roundtrips, so
    /// `OutputHandler` fires *before* any of that -- which is exactly what seeds the `screens`
    /// signal in time for the evaluation to loop over it (docs/adr/0041 decision 2), and equally
    /// exactly why [`App::handle_output_change`] must not do the rest of its job that early:
    /// there is no evaluation to expand, no surface to reconcile, and asking the Supervisor to
    /// reload a generation that has not applied anything yet buys one whole redundant evaluate/
    /// report/apply round trip on every boot.
    startup_complete: bool,
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
    /// The seat's pointer, once it advertised one (build-steps.md Phase 21 item 1). Kept alive
    /// for the same reason `text_input` is -- dropping the proxy destroys the protocol object,
    /// and with it every `enter`/`press`/`release` this shell is interactive because of.
    ///
    /// One, not one per seat: `bind_text_input` already takes `seats().next()`, so this whole
    /// file is single-seat, and a second seat's pointer would need a second `armed` beside it
    /// rather than sharing this one.
    pointer: Option<wl_pointer::WlPointer>,
    /// The press waiting for its release, if any (docs/adr/0050 decision 2, [`ArmedClick`]).
    armed: Option<ArmedClick>,
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
        startup_complete: false,
        outbound_tx,
        generation_id,
        presentation_time,
        queue_handle: qh.clone(),
        active_nonce: None,
        text_input: None,
        pointer: None,
        armed: None,
        text_input_pending: TextInputPending::default(),
        secure_buffer: shared::SecureBuffer::new(),
    };

    // Outputs (and the seat) arrive as a burst of registry + wl_seat/wl_output events after
    // binding; two roundtrips is enough to have both the full initial output list (which
    // `expand_instances` below turns a `monitor = "All"` declaration into one surface per monitor
    // from) and the seat `bind_text_input` needs.
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
    // `text` node's shaping blocks on `ShapingHandle::shape` until the worker's `FontSystem::new()`
    // finishes, eating into that same 2s budget. The § 15.2 ordering (evaluate before bind) is
    // required, not incidental, so this stays sequential -- not a fix, just the accepted cost.
    //
    // The `screens` seed goes *before* the evaluation, not after, and that ordering is the whole
    // point of the signal (docs/adr/0041 decision 2): a config's top-level `for _, screen in
    // ipairs(screens:get())` loop runs during this evaluation, so a list seeded afterwards would
    // declare no per-monitor panels at all on the first pass. The two roundtrips above are what
    // make the real list available this early.
    let screens = app.screens(None);
    let outputs = geometries_from(&screens);
    app.client.set_screens(screens_payload(&screens));
    let specs = app.client.run_startup_evaluation().unwrap_or_default();
    let instances = expand_instances(&specs, &outputs);
    for spec in &specs {
        if spec.topology.monitor != "All" && !outputs.iter().any(|output| output.name == spec.topology.monitor) {
            // `expand_instances` is pure and returns nothing for a miss; the log belongs here,
            // where the real output list is, so a config naming an unplugged monitor says so once
            // at startup rather than silently producing no surface.
            eprintln!(
                "[oblisk-renderer] surface {:?} targets monitor {:?}, which is not connected; no surface created for it",
                spec.topology.id, spec.topology.monitor
            );
        }
    }
    app.client.set_instances(instances.clone());
    // The first resolve, and it is validation rather than anything anyone sees. § 15.2 forces
    // evaluation before binding, so no surface has been configured yet and there is no configured
    // size to resolve against -- each instance uses its *output's* logical size instead, which the
    // two roundtrips above already know. Nothing paints this: a Candidate null-buffers before it
    // draws anything, and non-candidate mode's first draw happens on first configure, which is
    // after `set_instance_size` has replaced the size with the one the compositor chose. So a bar
    // is briefly resolved at full screen height here and never once painted that way.
    //
    // Both ways this can fail -- the evaluation itself, or the apply -- already logged their own
    // specific error and set `oblisk.rescue` inside `RendererClient`, so this line only adds the
    // consequence a reader needs from out here.
    if !app.client.apply_instances() {
        eprintln!("[oblisk-renderer] no scene was applied at startup; surfaces still bind, and paint nothing until a reload or a push produces one");
    }

    app.create_panels(&qh, &specs, &instances);
    // From here on an output event owns the whole job: there is an evaluation to expand and
    // surfaces to reconcile against it (see `App::startup_complete`).
    app.startup_complete = true;
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
        //
        // A re-resolve that actually changed the retained scene is repainted immediately. This is
        // *not* build-steps.md Phase 19 item 9's frame gating, and the two must not be confused:
        // item 9 is `wl_surface::frame()` plus a `frame_pending` flag, so the loop blocks when
        // nothing is happening instead of waking on the 15ms poll below. This is the other half --
        // the one that makes a capability push actually reach the screen at all, rather than
        // stopping at a resolved tree in memory. Item 9 still has to be built on top of it.
        // Two statements, in this order, because they are the two halves of one commit. The first
        // *stages* everything the re-resolve changed about each surface itself -- the layer-shell
        // fields layer-shell permits changing in place, the input region, and whether the surface
        // is mapped at all (docs/adr/0038 decision 2, build-steps.md Phase 20 items 1 and 5). All
        // of that is double-buffered `wl_surface` state, so none of it takes effect until a
        // commit, and the second statement's `swap_buffers` is that commit. Committing per field
        // instead would show the compositor a half-updated surface between requests.
        if app.client.re_resolve_if_dirty() {
            app.apply_resolved_surface_state();
            app.repaint_mapped_surfaces();
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
/// without a live Wayland connection -- this file has no headless Wayland test harness, which is
/// why the config-facing enums live in `layout` and only pure functions (the `layer_for` through
/// `exclusive_zone_for` block below, and docs/adr/0050's click-decision functions beside them)
/// live here.
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

/// `layout`'s `LayerKind` to the protocol's own stacking level. Pure, and one of the
/// `wayland/mod.rs` seams that is unit-testable at all -- everything around it needs a live
/// compositor, which is exactly why the config-facing enums live in `layout` and only the mapping
/// lives here.
fn layer_for(kind: LayerKind) -> Layer {
    match kind {
        LayerKind::Background => Layer::Background,
        LayerKind::Bottom => Layer::Bottom,
        LayerKind::Top => Layer::Top,
        LayerKind::Overlay => Layer::Overlay,
    }
}

/// § 6.1's four `anchor` edge booleans to the protocol's bitflags.
fn anchor_for(anchor: node::Anchor) -> Anchor {
    let mut flags = Anchor::empty();
    flags.set(Anchor::TOP, anchor.top);
    flags.set(Anchor::BOTTOM, anchor.bottom);
    flags.set(Anchor::LEFT, anchor.left);
    flags.set(Anchor::RIGHT, anchor.right);
    flags
}

/// § 6.1's `keyboard_interactivity` to the protocol's own field. Note what this replaced: every
/// surface used to take a hardcoded mode per Rust-owned role, so a launcher wanting `Exclusive`
/// and an OSD wanting `None` could not coexist (docs/adr/0038's rejected-alternative list names
/// this as the second reason the fixed role set had to go).
fn keyboard_interactivity_for(mode: node::KeyboardInteractivity) -> KeyboardInteractivity {
    match mode {
        node::KeyboardInteractivity::None => KeyboardInteractivity::None,
        node::KeyboardInteractivity::OnDemand => KeyboardInteractivity::OnDemand,
        node::KeyboardInteractivity::Exclusive => KeyboardInteractivity::Exclusive,
    }
}

/// One axis of `zwlr_layer_surface_v1::set_size`, resolved against that axis of the output.
///
/// `0` is the protocol's own "the anchors decide this axis" convention, which is what both
/// `SizeMode::Fill` and `SizeMode::Content` mean here. `Content` reaching this is the ordinary
/// case rather than an edge one -- it is `parse_size_mode`'s answer for an omitted `width`/
/// `height`, and a surface has no content size at creation time anyway, since nothing has been
/// measured and no output has been configured. A percent is the one form that needs the output,
/// which is why this takes it.
fn layer_extent_for(mode: SizeMode, output_extent: f32) -> u32 {
    match mode {
        SizeMode::Fill | SizeMode::Content => 0,
        SizeMode::Pixels(px) => px.max(0.0) as u32,
        SizeMode::Percent(fraction) => (output_extent * fraction).max(0.0) as u32,
    }
}

/// The axis, if any, on which this surface's `set_size` would be a protocol error.
///
/// `zwlr_layer_surface_v1::set_size`: "If you pass 0 for either value, the compositor will assign
/// it... You must set your anchor to opposite edges in the dimensions you omit; not doing so is a
/// protocol error." A protocol error kills the whole Wayland connection, and therefore the whole
/// shell -- so a config writing `panel { anchor = { top = true }, height = "Fill" }` would take
/// the Renderer down with no recoverable failure and nothing on screen to say why.
///
/// Nothing checked this before docs/adr/0038, because every size was a Rust constant chosen to be
/// valid. Now the config picks it, which makes this a trust boundary. The answer is to refuse the
/// one surface with a log naming the axis, not to invent a size for it: guessing the output extent
/// would silently give a config author a full-screen bar where they asked for an auto-sized one,
/// and they would have no idea why.
fn ambiguous_zero_axis(size: (u32, u32), anchor: node::Anchor) -> Option<&'static str> {
    if size.0 == 0 && !(anchor.left && anchor.right) {
        return Some("width");
    }
    if size.1 == 0 && !(anchor.top && anchor.bottom) {
        return Some("height");
    }
    None
}

/// The exclusive zone for a surface the config marked `exclusive`, derived from the size the
/// compositor actually configured rather than guessed at creation time (build-steps.md Phase 20
/// item 4). That is the whole reason this is a configure-time computation: at `get_layer_surface`
/// time a `"Fill"`-sized bar has no height yet, so any zone set there would be a guess the
/// compositor then contradicts.
///
/// One rule, on whichever axis the anchor pins the surface to a single edge: anchored top or
/// bottom but not both reserves its configured height; left or right but not both reserves its
/// width. Everything else is `0`, and that covers three shapes for the same reason -- a surface
/// anchored on all four edges, one anchored on none, and one anchored to a single *corner* all
/// leave the edge to reserve against genuinely ambiguous, and the protocol's own exclusive-zone
/// wording only defines the strip cases. A bar (`top`, `left`, `right`) is the common case and
/// lands on the height branch: it is pinned vertically to one edge and spans horizontally.
fn exclusive_zone_for(anchor: node::Anchor, configured_size: (u32, u32)) -> i32 {
    let (width, height) = configured_size;
    let one_vertical_edge = anchor.top != anchor.bottom;
    let one_horizontal_edge = anchor.left != anchor.right;
    match (one_vertical_edge, one_horizontal_edge) {
        (true, false) => height as i32,
        (false, true) => width as i32,
        _ => 0,
    }
}

/// One press waiting for its release (docs/adr/0050 decision 2): a click is a press *and* a
/// release on the same node, so that a user who presses a button, notices the mistake and drags
/// off it releases harmlessly.
///
/// "Same node" is this pair and not a node identity, because a `ResolvedNode` has none --
/// `NodeId` lives on `RetainedNode` and does not survive `to_resolved`. The rect is the proxy, and
/// the case it gets "wrong" it gets right anyway: a re-resolve between press and release that
/// moves the button cancels the click, which is what a real identity would also answer for a
/// button that moved out from under the pointer.
#[derive(Debug, Clone, PartialEq)]
struct ArmedClick {
    instance_id: String,
    rect: LogicalRect,
}

/// The innermost `button` in a [`layout::hit::hit_path`] result carrying a callable `on_click`, as
/// that button's absolute rect and its function (docs/adr/0050 decision 1).
///
/// Scans from the deep end, which is the whole reason hit-testing returns a path: the deepest node
/// under the pointer is normally the `button`'s `text` child, and it has no `on_click`. A `button`
/// without one is transparent to this scan rather than a barrier, so a plain `button` nested inside
/// a handled one still lets the outer one fire.
///
/// `on_click` must be a `Value::Function`. Anything else the config wrote under that key -- a
/// string, a table -- is simply not a click handler; `layout::node` has no parser for the key
/// (§ 5.2 leaves it opaque, docs/adr/0021 item 2), so this predicate is the only place its type is
/// ever checked.
fn clickable_button<'a>(path: &[&'a layout::ResolvedNode]) -> Option<(LogicalRect, &'a Function)> {
    path.iter().enumerate().rev().find_map(|(depth, node)| {
        if node.kind != "button" {
            return None;
        }
        let Some(Value::Function(on_click)) = node.properties.get("on_click") else {
            return None;
        };
        Some((layout::hit::absolute_rect(&path[..=depth])?, on_click))
    })
}

/// Whether a release on `instance_id`, over the button at `released_on`, completes `armed`
/// (docs/adr/0050 decision 2).
///
/// Both halves have to be a *button* hit, not merely the same coordinates: a release that lands in
/// the armed rect but on something that is no longer a handled button (the config re-resolved and
/// put a plain `rect` there) is not the click the press started. `released_on` is therefore
/// [`clickable_button`]'s answer for the release, not the raw pointer position.
fn release_completes_click(armed: Option<&ArmedClick>, instance_id: &str, released_on: Option<LogicalRect>) -> bool {
    match (armed, released_on) {
        (Some(armed), Some(rect)) => armed.instance_id == instance_id && armed.rect == rect,
        _ => false,
    }
}

/// `on_click`'s single argument: the button's rect as `{ x, y, width, height }` in its surface's
/// logical coordinates (docs/adr/0050 decision 3).
///
/// The rect travels *to* the callback, not back from it. § 6's `popup` entry and docs/adr/0040
/// both say the anchor rect is "passed straight from the rect `button`'s `on_click` hands back",
/// which read as a Rust-side return value would be unimplementable -- the engine does not know
/// which `popup` a click was meant to open, and it already has the button's rect. "Hands back"
/// is the round trip through the config: `on_click = function(rect) menu_anchor:set(rect) end`,
/// with the `popup` declaring `anchor_rect = menu_anchor` (build-steps.md Phase 22 item 2).
fn rect_table(lua: &Lua, rect: LogicalRect) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("x", rect.x)?;
    table.set("y", rect.y)?;
    table.set("width", rect.width)?;
    table.set("height", rect.height)?;
    Ok(table)
}

/// The surface ids a PBA Candidate both announces in its `ReadySignal` and then draws on
/// `ActivateDraw` -- one function, called from [`App::maybe_send_ready_signal`], and the same
/// [`MapState::presents`] predicate [`App::activate_draw`] skips on.
///
/// The two sets have to be *identical*, and both directions of a mismatch are fatal in
/// `supervisor/src/reload.rs`'s `drive_handshake`. Announcing a surface that never draws leaves
/// `while collected.len() < expected.len()` waiting for evidence that cannot arrive, until
/// `evidence_timeout` fires. Drawing one that was not announced trips
/// `!expected.contains(&surface_id)` and aborts the Candidate as `PbaFailure::UnexpectedEvidence`.
///
/// A panel declared `visible = false` is what forces the filter: docs/adr/0038 decision 2 still
/// *creates* it, so it exists as a `TrackedSurface` and stages like every other surface, but it
/// never presents a frame, so it must not be in the expected set. An empty result is legal, not a
/// degenerate case -- `drive_handshake`'s collection loop exits immediately on an empty expected
/// set, so a generation whose every panel starts hidden completes its handshake.
fn presenting_surface_ids<'a>(surfaces: impl Iterator<Item = (&'a str, MapState)>) -> Vec<String> {
    surfaces
        .filter(|(_, state)| state.presents())
        .map(|(id, _)| id.to_string())
        .collect()
}

/// The layer-shell requests one *live* surface needs after a re-resolve changed its `panel`
/// properties -- `margin`, `keyboard_interactivity`, size, and the `exclusive` flag the zone is
/// derived from (docs/adr/0038 decision 2, § 6.1, build-steps.md Phase 20 item 1). `None` per
/// field means "unchanged, send nothing": these are all double-buffered, so re-sending an
/// unchanged value is not wrong, just noise on the wire that the diff exists to avoid.
///
/// [`SurfaceTopology`](node::SurfaceTopology)'s five fields -- `id`, `layer`, `anchor`, `monitor`,
/// `namespace` -- are deliberately absent, and that is a statement rather than an omission. The
/// protocol cannot change a surface's namespace or output at all (`get_layer_surface` consumes
/// both), and an edit to any of the five is a topology change `crate::socket`'s `handle_reevaluate`
/// routes to a generation swap, where the Candidate builds its own surfaces from its own
/// evaluation. So they cannot legitimately differ between `applied` and `fresh` here: every one of
/// them is `is_structural_property`, copied through raw and refused a `Signal`
/// (`layout::node::reject_signal_in_structural_field`), precisely so a live surface can never be
/// asked to move.
///
/// `output` is the surface's *output's* logical size, not its configured size -- see
/// [`TrackedSurface::output_size`] for why the two must not be confused.
#[derive(Debug, Default, PartialEq)]
struct SpecUpdate {
    margin: Option<node::EdgeInsets>,
    keyboard_interactivity: Option<node::KeyboardInteractivity>,
    size: Option<(u32, u32)>,
    /// § 6.1's `exclusive` boolean. The zone itself is not here because it is not a spec field:
    /// it is derived from the size the compositor configured (see [`exclusive_zone_for`]), so
    /// this only reports that the derivation's *input* flipped.
    exclusive: Option<bool>,
}

fn spec_update(applied: &PanelSpec, fresh: &PanelSpec, output: layout::LogicalSize) -> SpecUpdate {
    // Compared as the pixel pair that actually goes on the wire, not as the two `SizeMode`s: a
    // percent and an equivalent pixel count are the same request, and `Fill` and `Content` are
    // both the protocol's `0`.
    let extent = |spec: &PanelSpec| {
        (
            layer_extent_for(spec.width, output.width),
            layer_extent_for(spec.height, output.height),
        )
    };
    SpecUpdate {
        margin: (fresh.margin != applied.margin).then_some(fresh.margin),
        keyboard_interactivity: (fresh.keyboard_interactivity != applied.keyboard_interactivity)
            .then_some(fresh.keyboard_interactivity),
        size: (extent(fresh) != extent(applied)).then(|| extent(fresh)),
        exclusive: (fresh.exclusive != applied.exclusive).then_some(fresh.exclusive),
    }
}

/// Parameters for [`App::spawn_layer`]; bundled so the helper stays under clippy's
/// argument-count limit while still taking each surface's divergent bits.
struct LayerSpec<'a> {
    layer_type: Layer,
    /// The compositor-visible namespace (§ 6.1's `namespace`, defaulting to `"oblisk-{id}"`),
    /// which is what a `layerrule` matches on.
    namespace: &'a str,
    /// Always `Some` since docs/adr/0038 decision 3: one surface is created per
    /// `(surface, output)` pair, so the output is never the compositor's to pick.
    output: &'a wl_output::WlOutput,
    anchor: Anchor,
    size: (u32, u32),
    margin: node::EdgeInsets,
    keyboard_interactivity: KeyboardInteractivity,
}

/// One connected output, exactly as `wl_output` reports it (docs/adr/0041 decision 2). This is the
/// single source both consumers read: the `screens` Lua signal a config loops over to declare
/// per-monitor panels, and the [`OutputGeometry`] list `layout::instance::expand_instances` matches
/// `monitor` against. Two sources would let a config's own arithmetic and the engine's layout
/// disagree about how large a monitor is.
#[derive(Debug, Clone, PartialEq)]
struct Screen {
    name: String,
    width: i32,
    height: i32,
    scale: i32,
    /// Hz. `wl_output`'s `mode` event reports millihertz, which is not the unit anyone writes a
    /// config against, so the division happens once here rather than in every config.
    refresh: f64,
}

/// The `smithay_client_toolkit::output::OutputInfo` fields [`screen_entry`] reads, lifted off it
/// by [`App::screens`].
///
/// A separate struct rather than the real thing because `OutputInfo` is `#[non_exhaustive]` with
/// no public constructor: a function taking one could never be built in a unit test, and every
/// decision in this conversion (the `logical_size` fallback, millihertz to Hz, the positional name
/// fallback) is exactly what wants testing in a file with no headless Wayland harness.
struct OutputFacts {
    name: Option<String>,
    logical_size: Option<(i32, i32)>,
    /// The *current* `Mode`'s `(dimensions, refresh_rate)`, or `None` for an output advertising no
    /// current mode. Both fields come from the same mode, so they travel together rather than as
    /// two `Option`s that could disagree about which mode they describe.
    current_mode: Option<((i32, i32), i32)>,
    scale_factor: i32,
}

/// One output's `screens` entry, or `None` for an output whose size cannot be known.
///
/// `logical_size` first (`xdg_output`/`wl_output` v4's compositor-space size, which is what a layer
/// surface's own coordinates are in), falling back to the current `Mode`'s `dimensions` for a
/// compositor that reports no logical size. An output with neither yields nothing rather than a
/// default, since a made-up size would resolve every surface on that monitor against a fiction --
/// the caller logs the miss, the same split `layout::instance::expand_instances` already uses.
///
/// ponytail: a nameless output (a compositor below `wl_output` v4) takes a positional
/// `"output-{index}"` id, carried over from the deleted `wallpaper_surface_id`. It keeps the shell
/// working there, at the cost that `monitor = "DP-1"` can never match on such a compositor -- the
/// config has no name to write. Upgrade path: none available client-side; the name genuinely does
/// not exist.
fn screen_entry(index: usize, facts: &OutputFacts) -> Option<Screen> {
    let (width, height) = facts.logical_size.or_else(|| facts.current_mode.map(|(dimensions, _)| dimensions))?;
    Some(Screen {
        name: facts.name.clone().unwrap_or_else(|| format!("output-{index}")),
        width,
        height,
        scale: facts.scale_factor,
        // `Mode`'s own docs already allow a zero refresh rate ("if an output has no correct
        // refresh rate, such as a virtual output"), so an output with no current mode reads the
        // same way rather than needing a nil case every config would have to guard.
        refresh: facts.current_mode.map_or(0.0, |(_, rate)| f64::from(rate) / 1000.0),
    })
}

/// The `screens` signal's payload: § 2.9's per-output fields as a JSON array, pushed into Lua
/// through the same `Loader::to_lua_value` every capability's `StateSnapshot` goes through
/// (docs/adr/0041 decision 2 -- Renderer-sourced, but not a second marshalling path).
fn screens_payload(screens: &[Screen]) -> serde_json::Value {
    serde_json::Value::Array(
        screens
            .iter()
            .map(|screen| {
                serde_json::json!({
                    "name": screen.name,
                    "width": screen.width,
                    "height": screen.height,
                    "scale": screen.scale,
                    "refresh": screen.refresh,
                })
            })
            .collect(),
    )
}

/// The same screen list as `layout::instance` needs it: a name to match `monitor` against and a
/// logical size to seed each instance's `available` with.
fn geometries_from(screens: &[Screen]) -> Vec<OutputGeometry> {
    screens
        .iter()
        .map(|screen| OutputGeometry {
            name: screen.name.clone(),
            size: layout::LogicalSize { width: screen.width as f32, height: screen.height as f32 },
        })
        .collect()
}

impl App {
    /// Every connected output as [`Screen`] describes it, skipping (with a log) any whose size
    /// `wl_output` cannot answer for.
    ///
    /// `departing` is the output an `output_destroyed` event is announcing, which must be excluded
    /// by hand: `smithay_client_toolkit`'s `remove_global` calls `OutputHandler::output_destroyed`
    /// *before* removing the output from its own `OutputState`, so a plain read of `outputs()` from
    /// inside that callback still lists the monitor that just went away. `None` everywhere else.
    fn screens(&self, departing: Option<&wl_output::WlOutput>) -> Vec<Screen> {
        let mut screens = Vec::new();
        for (index, output) in self.output_state.outputs().enumerate() {
            if departing == Some(&output) {
                continue;
            }
            let Some(info) = self.output_state.info(&output) else {
                eprintln!("[oblisk-renderer] output {index} advertised no info yet; no surface created on it");
                continue;
            };
            let facts = OutputFacts {
                name: info.name.clone(),
                logical_size: info.logical_size,
                current_mode: info.modes.iter().find(|mode| mode.current).map(|mode| (mode.dimensions, mode.refresh_rate)),
                scale_factor: info.scale_factor,
            };
            match screen_entry(index, &facts) {
                Some(screen) => screens.push(screen),
                None => eprintln!(
                    "[oblisk-renderer] output {:?} reports neither a logical size nor a current mode; no surface created on it",
                    info.name.as_deref().unwrap_or("<unnamed>")
                ),
            }
        }
        screens
    }

    /// Creates and configures (but does not commit) a layer-shell surface.
    fn spawn_layer(&mut self, qh: &QueueHandle<App>, spec: LayerSpec) -> LayerSurface {
        let surface = self.compositor_state.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            spec.layer_type,
            Some(spec.namespace),
            Some(spec.output),
        );
        layer.set_anchor(spec.anchor);
        layer.set_size(spec.size.0, spec.size.1);
        layer.set_keyboard_interactivity(spec.keyboard_interactivity);
        layer.set_margin(
            spec.margin.top as i32,
            spec.margin.right as i32,
            spec.margin.bottom as i32,
            spec.margin.left as i32,
        );
        // No `set_exclusive_zone` here: it is derived from the size the compositor picks, at
        // configure time -- see `exclusive_zone_for`.
        layer
    }

    /// One `zwlr_layer_surface_v1` per surface instance, built from the evaluation that declared
    /// it (docs/adr/0038 decision 1, build-steps.md Phase 20 items 1 and 2). This replaced
    /// `create_main_bar`/`create_overlay_canvas`/`create_wallpaper_layers`, which ran *before* any
    /// Lua had been evaluated and discarded every field the config wrote.
    ///
    /// `instances` and `specs` come from the same evaluation, so an instance whose declared id has
    /// no spec cannot happen; it is skipped with a log rather than panicking, on the same
    /// "keep the shell up" principle as every other failure in this file.
    ///
    /// Called with the whole instance set at startup and with only the *added* instances on a
    /// monitor hotplug (see [`App::handle_output_change`]) -- the same function either way, since
    /// "build a layer surface for this instance" is the same job in both.
    fn create_panels(&mut self, qh: &QueueHandle<App>, specs: &[PanelSpec], instances: &[SurfaceInstance]) {
        // Re-read per call rather than snapshotted once at startup: this now also runs from an
        // output event, where the whole point is that the output list has just changed.
        let outputs: HashMap<String, wl_output::WlOutput> = self
            .output_state
            .outputs()
            .enumerate()
            .filter_map(|(index, output)| {
                let info = self.output_state.info(&output)?;
                Some((info.name.clone().unwrap_or_else(|| format!("output-{index}")), output))
            })
            .collect();

        for instance in instances {
            let Some(spec) = specs.iter().find(|spec| spec.topology.id == instance.declared_id) else {
                eprintln!("[oblisk-renderer] instance {:?} has no matching declaration; skipping", instance.instance_id);
                continue;
            };
            let Some(output) = outputs.get(&instance.output) else {
                eprintln!("[oblisk-renderer] instance {:?} names an output that has since gone; skipping", instance.instance_id);
                continue;
            };
            let size = (
                layer_extent_for(spec.width, instance.available.width),
                layer_extent_for(spec.height, instance.available.height),
            );
            if let Some(axis) = ambiguous_zero_axis(size, spec.topology.anchor) {
                eprintln!(
                    "[oblisk-renderer] surface {:?} leaves its {axis} to the compositor without anchoring both {axis} edges, \
                     which layer-shell rejects as a protocol error; no surface created. Give it an explicit {axis}, or anchor both edges.",
                    instance.instance_id
                );
                continue;
            }
            let layer = self.spawn_layer(
                qh,
                LayerSpec {
                    layer_type: layer_for(spec.topology.layer),
                    namespace: &spec.topology.namespace,
                    output,
                    anchor: anchor_for(spec.topology.anchor),
                    size,
                    margin: spec.margin,
                    keyboard_interactivity: keyboard_interactivity_for(spec.keyboard_interactivity),
                },
            );
            layer.commit();

            // § 6.1's `visible` at its starting value. A panel declared `visible = false` is still
            // created (docs/adr/0038 decision 2: `visible` maps and unmaps, it does not create and
            // destroy). It still performs the initial commit directly above, which
            // `get_layer_surface` requires before any configure arrives and which does not map
            // anything on its own; what makes it invisible is that no buffer is ever attached, and
            // `MapState::Unmapped` is what keeps `paint_surface` from attaching one. No *unmap*
            // commit is needed or wanted here, since on an already-bufferless surface that is the
            // protocol's re-map procedure rather than an unmap -- see [`App::remap`], which
            // measured both sides of this distinction against a real compositor.
            //
            // A surface whose instance has no resolved tree (the startup apply failed) is treated
            // as visible, matching every other "keep the shell up" fallback in this file.
            let visible = self
                .client
                .scene()
                .surface(&instance.instance_id)
                .is_none_or(|tree| tree.visible);

            self.surfaces.push(TrackedSurface {
                layer,
                bound: None,
                surface_id: instance.instance_id.clone(),
                applied_spec: spec.clone(),
                output_size: instance.available,
                map_state: if visible { MapState::AwaitingConfigure } else { MapState::Unmapped },
                null_buffered: false,
                configured_size: (0, 0),
            });
        }
    }

    /// One `wl_output` appeared, changed, or went away (build-steps.md Phase 20 items 2 and 6).
    /// Two jobs, one handler, because one event owes both.
    ///
    /// First, the `screens` signal (docs/adr/0041 decision 2). Everything below is gated on that
    /// push reporting a real change: `update_output` also fires for things `screens` does not
    /// carry, and re-running the rest for one of those would rebuild nothing and ask the
    /// Supervisor for a reload cycle no output change justifies.
    ///
    /// Second, the instance set (docs/adr/0038 decision 3). A `monitor = "All"` declaration expands
    /// to one instance per output, so an output appearing adds an instance and one going away
    /// removes it, **in place with no generation swap** -- plugging in a monitor is not a config
    /// edit, and the set of *declared* surfaces has not moved.
    ///
    /// Finally [`crate::socket::RendererClient::request_reload`], which is the other half and the
    /// one this cannot do itself: a config that loops over `screens` declares genuinely different
    /// surface ids before and after, which is a topology change and so a generation swap
    /// (docs/adr/0041 decision 3). Only the Supervisor decides that. The two do not conflict --
    /// a candidate builds its own surface set from its own evaluation, so whatever this reconciled
    /// here is discarded along with the rest of this generation if a swap does happen.
    ///
    /// ponytail: an output change landing inside a PBA Candidate's own ready window is not
    /// handled. `maybe_send_ready_signal` announces the surfaces this process will present exactly
    /// once, so a surface added after that point would present evidence the Supervisor never
    /// expected (`PbaFailure::UnexpectedEvidence`), and a `RequestReload` sent while a handshake
    /// is draining `inbound_frames` is logged and skipped by `SocketCandidateLink::recv_matching`.
    /// The window is the seconds of `PBA_TIMINGS`, and the fix is a Candidate deferring output
    /// changes the way `apply_visibility` already defers `visible`; not built until a hotplug
    /// during a swap is something anyone has actually hit.
    fn handle_output_change(&mut self, qh: &QueueHandle<App>, departing: Option<&wl_output::WlOutput>) {
        let screens = self.screens(departing);
        if !self.client.set_screens(screens_payload(&screens)) || !self.startup_complete {
            // The signal is pushed either way -- seeding it from the initial output burst is the
            // point (see `App::startup_complete`) -- but nothing below it applies yet.
            return;
        }
        eprintln!("[oblisk-renderer] outputs changed: {:?}", screens.iter().map(|s| s.name.as_str()).collect::<Vec<_>>());

        let specs = self.client.applied_panel_specs();
        let fresh = expand_instances(&specs, &geometries_from(&screens));
        let reconcile = reconcile_instances(self.client.instances(), &fresh);

        for instance_id in &reconcile.removed {
            self.destroy_surface_by_id(instance_id);
        }
        // A surviving surface's `output_size` is the basis a `SizeMode::Percent` resolves against,
        // so a mode change that resized the monitor under it has to move it -- `fresh` carries the
        // output's *current* logical size, while the instance set deliberately keeps the size the
        // compositor configured each surface to (see `reconcile_instances`).
        for instance in &fresh {
            if let Some(tracked) = self.surfaces.iter_mut().find(|s| s.surface_id == instance.instance_id) {
                tracked.output_size = instance.available;
            }
        }
        // Before `create_panels`, which reads the scene by instance id to decide a new surface's
        // starting `visible`.
        self.client.set_instances(reconcile.instances);
        self.create_panels(qh, &specs, &reconcile.added);
        self.client.request_reload();
    }

    /// Destroys one surface instance: its `zwlr_layer_surface_v1`, its `wl_surface`, its
    /// `wl_egl_window`, and its EGL surface (docs/adr/0038 decision 3's removal half). A no-op for
    /// an id this process has no surface for, which is the normal case for the second of the two
    /// events an unplugged monitor produces -- `zwlr_layer_surface_v1::closed` and
    /// `OutputHandler::output_destroyed` both arrive, in either order, and whichever comes first
    /// does the work.
    ///
    /// Teardown runs outermost-first, and the two explicit `drop`s below are what make that so
    /// rather than leaving it to `TrackedSurface`'s field order (which declares `layer` before
    /// `bound`, so a plain drop would destroy the `wl_surface` out from under the
    /// `wl_egl_window` still pointing at it):
    ///
    /// 1. `eglDestroySurface`, by hand, because `khronos_egl::Surface` is a plain copyable handle
    ///    with no `Drop` -- without this every unplugged monitor leaks one EGL surface. It has to
    ///    be first, since [`BoundSurface`]'s own contract is that the `WlEglSurface` outlives the
    ///    EGL surface built from it.
    /// 2. `BoundSurface`'s drop, which is `wl_egl_window_destroy`.
    /// 3. `LayerSurface`'s drop, which destroys the `zwlr_layer_surface_v1` and then the
    ///    `wl_surface` (in that order, which is the layer-shell protocol's own requirement and
    ///    `smithay_client_toolkit`'s job, not this function's).
    fn destroy_surface_by_id(&mut self, instance_id: &str) {
        let Some(index) = self.surfaces.iter().position(|s| s.surface_id == instance_id) else {
            return;
        };
        let TrackedSurface { layer, bound, surface_id, .. } = self.surfaces.remove(index);
        if let Some(bound) = bound.as_ref()
            && let Err(err) = self.egl.instance.destroy_surface(self.egl.display, bound.egl_surface)
        {
            log_bind_failure(&surface_id, "eglDestroySurface", err);
        }
        drop(bound);
        drop(layer);
        eprintln!("[oblisk-renderer] {surface_id} destroyed: its output is gone");
    }

    /// One `configure`: record the size the compositor chose, tell the retained scene about it,
    /// derive the exclusive zone from it, bind EGL if this surface has not been bound yet, and
    /// paint.
    ///
    /// PBA candidate mode (`self.is_pba_candidate`, build-steps.md Phase 14, § 15.2 points 2-3)
    /// stops after the null buffer instead: a first configure commits a null buffer directly on
    /// the raw `wl_surface` rather than binding EGL at all -- the Candidate stays invisible,
    /// occupying zero on-screen coordinates, until [`App::activate_draw`] does the real EGL bind
    /// later.
    fn bind_and_clear(&mut self, layer: &LayerSurface, width: u32, height: u32) {
        let Some(index) = self.surfaces.iter().position(|s| &s.layer == layer) else {
            return;
        };

        self.surfaces[index].configured_size = (width, height);
        // Only here does a real size for this instance exist (build-steps.md Phase 20 item 4,
        // closing docs/adr/0023 item 6): the startup resolve used the whole output's size, and
        // this replaces it with what the compositor actually granted, marking the scene dirty so
        // the next poll turn re-resolves against it.
        //
        // So the `paint_surface` at the bottom of this function draws the *previous* resolve, and
        // `run`'s loop repaints with the corrected one on the very next turn -- `dispatch_pending`
        // and `re_resolve_if_dirty` are two statements apart, so that is sub-frame, not a visible
        // lag. Re-resolving here instead would run one whole `Scene::apply` per configure in a
        // startup burst rather than one for the burst, which is the coalescing ADR-0044 decision 2
        // built the flag for.
        self.client.set_instance_size(
            &self.surfaces[index].surface_id,
            layout::LogicalSize { width: width as f32, height: height as f32 },
        );
        // The configure the protocol requires before any buffer may be attached, whether this is
        // the surface's first one or the one completing a re-map (`zwlr_layer_surface_v1`'s
        // description: "waiting for a configure event and handling it as usual"). Everything below
        // this line is allowed to draw; nothing above it was.
        if self.surfaces[index].map_state == MapState::AwaitingConfigure {
            self.surfaces[index].map_state = MapState::Mapped;
        }
        self.apply_exclusive_zone(index);
        // The other half of the poll loop's `re_resolve_if_dirty` hook, reached from the other
        // direction. It matters most on the *first* configure: no input region has ever been set
        // at that point, and a fullscreen transparent panel whose configured size happens to equal
        // its output's marks the scene clean, so no later re-resolve would arrive to set one and
        // the surface would swallow every click meant for the window behind it.
        self.apply_resolved_state(index);

        if self.is_pba_candidate {
            if self.surfaces[index].map_state.presents() {
                if !self.surfaces[index].null_buffered {
                    // verified against wayland_client::protocol::wl_surface::WlSurface's generated
                    // API: `attach(&self, buffer: Option<&wl_buffer::WlBuffer>, x: i32, y: i32)`,
                    // `commit(&self)`.
                    self.surfaces[index].layer.wl_surface().attach(None, 0, 0);
                    self.surfaces[index].null_buffered = true;
                }
                // Committed on every candidate-mode configure rather than only the first: a
                // Candidate has no `swap_buffers` to ride on until `ActivateDraw`, so this is the
                // only commit that can carry the state staged directly above.
                self.surfaces[index].layer.wl_surface().commit();
            } else {
                // `visible = false`: no buffer was ever attached, so this surface is *already* in
                // the invisible state § 15.2 point 3 asks a Candidate to reach, and committing it
                // is exactly the protocol's re-map procedure. Marked staged without touching the
                // wire, so `maybe_send_ready_signal`'s "every surface has staged" gate still
                // completes -- the surface is then filtered out of the announced set itself, by
                // `presenting_surface_ids`.
                self.surfaces[index].null_buffered = true;
            }
            self.maybe_send_ready_signal();
            return;
        }

        // `!= Mapped` rather than "is unmapped": `apply_resolved_state` directly above may have
        // just issued a *re-map* commit, and the protocol's own wait applies to that too -- the
        // configure answering it has not arrived yet, so no buffer may be attached in this pass.
        // The unmapped case additionally takes deliberately no commit here: see [`App::unmap`].
        if self.surfaces[index].map_state != MapState::Mapped {
            return;
        }

        if !self.ensure_bound(index) {
            return;
        }
        // A repeat configure carrying a new size (a mode change, an exclusive zone shifting a
        // neighbour) has to move the `wl_egl_window` too, or the surface keeps rendering into a
        // buffer sized at its first configure while the canvas draws at the new one. This is
        // `wayland-egl`'s own resize request, not a rebind: the `WlEglSurface` and the EGL surface
        // built from it both stay valid. It went unnoticed before docs/adr/0038 only because the
        // one surface that drew anything drew a fixed proof string; the resized frame is real
        // content now.
        if let Some(bound) = self.surfaces[index].bound.as_ref() {
            bound.native_window.resize(width.max(1) as i32, height.max(1) as i32, 0, 0);
        }
        self.paint_surface(index);
    }

    /// `set_exclusive_zone`, computed from the size the compositor configured (see
    /// [`exclusive_zone_for`]) for a surface the config marked `exclusive`, and an explicit `0`
    /// for one it did not.
    ///
    /// The explicit `0` is what changed with build-steps.md Phase 20 item 1. This used to leave a
    /// non-exclusive surface alone entirely, on the correct-at-the-time reasoning that the
    /// protocol's default zone is already 0 -- true only while `exclusive` could never change.
    /// It is a `Signal`-bindable property (docs/adr/0038 decision 2 lists the exclusive zone among
    /// the fields layer-shell accepts on a live surface), so a dock turning `exclusive = false`
    /// has to *take back* the zone it previously reserved, and the default is no help once a real
    /// value has been sent.
    ///
    /// Stages only; the caller's commit carries it. Committing here would have been wrong in two
    /// separate ways once `visible` landed: it would split one surface update across several
    /// commits, and on an unmapped surface a commit with no buffer attached is the protocol's own
    /// re-map procedure (see [`App::unmap`]).
    fn apply_exclusive_zone(&mut self, index: usize) {
        let tracked = &self.surfaces[index];
        let zone = if tracked.applied_spec.exclusive {
            exclusive_zone_for(tracked.applied_spec.topology.anchor, tracked.configured_size)
        } else {
            0
        };
        tracked.layer.set_exclusive_zone(zone);
    }

    /// [`App::apply_resolved_state`] for every tracked surface, which is what the poll loop calls
    /// after a re-resolve actually changed the retained scene. Every surface, not the changed
    /// ones, for exactly the reason [`App::repaint_bound_surfaces`] gives: ADR-0044 decision 2's
    /// dirty flag is one flag for the whole scene.
    fn apply_resolved_surface_state(&mut self) {
        for index in 0..self.surfaces.len() {
            self.apply_resolved_state(index);
        }
    }

    /// Pushes one surface's freshly resolved root back to the compositor: the layer-shell fields
    /// layer-shell permits changing on a live surface, the input region, and whether the surface
    /// is mapped at all (docs/adr/0038 decision 2, § 6.1's `visible` and `margin` rows,
    /// build-steps.md Phase 20 items 1 and 5).
    ///
    /// All three are double-buffered `wl_surface` state and are therefore *staged* here, not
    /// committed: the caller's commit -- `paint_surface`'s `swap_buffers` on a mapped surface, the
    /// candidate branch's own commit on a staging Candidate -- carries the whole update at once.
    /// The two exceptions are the map and unmap transitions, which are commits by definition and
    /// perform their own.
    fn apply_resolved_state(&mut self, index: usize) {
        let surface_id = self.surfaces[index].surface_id.clone();
        // Owned, so the immutable borrow of `self.client` ends before the `&mut self` calls below.
        let Some(tree) = self.client.scene().surface(&surface_id) else {
            // No resolved tree for this instance: a startup whose apply failed, or a re-resolve
            // that rolled back (`Scene::apply` restores its pre-call state on error). Every field
            // stays at what was last applied, which is the same "keep the last good frame"
            // principle `re_resolve_if_dirty` already follows -- pushing protocol defaults here
            // would resize and un-anchor a working surface over a transient bad capability value.
            return;
        };

        match node::panel_spec(&tree.properties) {
            Ok(fresh) => self.apply_spec_change(index, fresh),
            Err(err) => eprintln!(
                "[oblisk-renderer] {surface_id}: re-resolved panel properties are invalid, keeping the last applied ones: {err}"
            ),
        }
        self.apply_input_region(index, &tree);
        self.apply_visibility(index, tree.visible);
    }

    /// Diffs one surface's freshly resolved `panel` spec against the one its layer-shell state was
    /// last set from and sends only what moved (see [`spec_update`] for which fields, and for why
    /// the topology ones are not among them).
    fn apply_spec_change(&mut self, index: usize, mut fresh: PanelSpec) {
        let tracked = &self.surfaces[index];
        let update = spec_update(&tracked.applied_spec, &fresh, tracked.output_size);

        if let Some(margin) = update.margin {
            self.surfaces[index].layer.set_margin(
                margin.top as i32,
                margin.right as i32,
                margin.bottom as i32,
                margin.left as i32,
            );
        }
        if let Some(mode) = update.keyboard_interactivity {
            self.surfaces[index]
                .layer
                .set_keyboard_interactivity(keyboard_interactivity_for(mode));
        }
        if let Some(size) = update.size {
            // The same guard `create_panels` runs, and it has to run again here rather than only
            // at creation: `width`/`height` are ordinary resolvable properties, so a `Signal` can
            // turn a fixed height into `"Fill"` at runtime, and a `set_size` of 0 on a singly
            // anchored axis is a protocol error that kills the connection and the whole shell with
            // it (see [`ambiguous_zero_axis`]).
            if let Some(axis) = ambiguous_zero_axis(size, fresh.topology.anchor) {
                eprintln!(
                    "[oblisk-renderer] surface {:?} resolved to a {axis} of 0 without anchoring both {axis} edges, \
                     which layer-shell rejects as a protocol error; keeping its previous size. Give it an explicit {axis}, or anchor both edges.",
                    self.surfaces[index].surface_id
                );
                // The refused size must not enter the baseline, or the next re-resolve would see
                // no change and never retry the size the config eventually settles on.
                fresh.width = self.surfaces[index].applied_spec.width;
                fresh.height = self.surfaces[index].applied_spec.height;
            } else {
                self.surfaces[index].layer.set_size(size.0, size.1);
            }
        }

        // Before `apply_exclusive_zone`, which reads `exclusive` and the anchor off it.
        self.surfaces[index].applied_spec = fresh;
        if update.exclusive.is_some() || update.size.is_some() {
            self.apply_exclusive_zone(index);
        }
    }

    /// `wl_surface::set_input_region` from this surface's own resolved tree (§ 5.1,
    /// docs/adr/0038 decision 5, build-steps.md Phase 20 item 5), closing docs/adr/0023 item 5.
    ///
    /// Per surface, not for one overlay. Three cases fall out of the same code rather than needing
    /// three branches, which is the generalization the ADR asks for: a root with no visible
    /// children yields an empty region and every click passes through to whatever is behind it
    /// (the boot-time empty region the deleted `create_overlay_canvas` set, now the ordinary
    /// answer for any surface with nothing drawn in it); a root whose child fills it yields a
    /// region covering the surface, which is what the protocol default already is, so a tightly
    /// sized bar is a no-op and deliberately gets no special case; and anything in between -- a
    /// fullscreen transparent panel holding one small OSD -- gets exactly its visible content.
    ///
    /// The scale is `1.0`, matching `paint_surface`'s for the same reason its `ponytail:` gives:
    /// nothing calls `set_buffer_scale`, so surface-local coordinates and the framebuffer are both
    /// at scale 1, and passing a real scale here alone would put the input region on a physical
    /// grid the drawn content is not on.
    ///
    /// Not diffed against the last region pushed, unlike the spec fields: this only runs when the
    /// scene actually re-resolved, and the full GPU repaint that follows on the same turn costs
    /// orders of magnitude more than one `wl_region` round of requests.
    fn apply_input_region(&mut self, index: usize, tree: &layout::ResolvedNode) {
        let region = match Region::new(&self.compositor_state) {
            Ok(region) => region,
            Err(e) => {
                // Not fatal, unlike `create_overlay_canvas`'s version of this: the only failure
                // `Region::new` reports is a missing `wl_compositor`, which cannot happen here
                // because `CompositorState::bind` in `run` already succeeded against it. Killing
                // a working shell over an unreachable branch is the worse trade.
                log_bind_failure(&self.surfaces[index].surface_id.clone(), "wl_compositor::create_region", e);
                return;
            }
        };
        for rect in layout::overlay_input_regions(tree, 1.0) {
            region.add(rect.x0, rect.y0, rect.x1 - rect.x0, rect.y1 - rect.y0);
        }
        self.surfaces[index].layer.set_input_region(Some(region.wl_region()));
        // `region` drops here, destroying the `wl_region` -- `wl_surface::set_input_region` copies
        // its contents, so the object has no reason to outlive the request. Same shape the deleted
        // `create_overlay_canvas` used.
    }

    /// Applies § 6.1's `visible` to a live surface (docs/adr/0038 decision 2).
    ///
    /// **Frozen for a PBA Candidate**, and that is the one line in this file where a mistake hangs
    /// the shell rather than failing a test. `maybe_send_ready_signal` announces the surfaces this
    /// process will present and `activate_draw` draws exactly that set; if `visible` could move
    /// between those two points -- and it can, since § 15.2 point 2 hydrates a Candidate with
    /// cached capability state precisely in that window -- the announced set and the drawn set
    /// would disagree, which is either an `evidence_timeout` hang or a `PbaFailure::
    /// UnexpectedEvidence` abort (see [`presenting_surface_ids`]). Freezing makes them agree by
    /// construction rather than by two functions being kept in step by hand. The deferred change
    /// applies on the first re-resolve after promotion clears `is_pba_candidate`, which is the
    /// next capability push; a Candidate's whole life is the handshake, so there is nothing else
    /// that window could be for.
    fn apply_visibility(&mut self, index: usize, visible: bool) {
        if self.is_pba_candidate {
            return;
        }
        match (self.surfaces[index].map_state, visible) {
            (MapState::Unmapped, true) => self.remap(index),
            (MapState::AwaitingConfigure | MapState::Mapped, false) => self.unmap(index),
            _ => {}
        }
    }

    /// `zwlr_layer_surface_v1`'s own unmap procedure, taken literally: "Attaching a null buffer to
    /// a layer surface unmaps it." One commit, no destroyed protocol objects, which is the whole
    /// point of docs/adr/0038 decision 2 -- toggling a launcher costs this instead of a process
    /// spawn.
    ///
    /// This is the commit the staged state above it rides on, and it is the *only* commit an
    /// unmapped surface ever gets. Nothing else in this file may commit one, because the same
    /// description spells out that "the client can re-map the surface by performing a commit
    /// without any buffer attached" -- a stray bookkeeping commit on an unmapped surface would
    /// silently re-map it.
    fn unmap(&mut self, index: usize) {
        let tracked = &self.surfaces[index];
        tracked.layer.wl_surface().attach(None, 0, 0);
        tracked.layer.wl_surface().commit();
        self.surfaces[index].map_state = MapState::Unmapped;
        eprintln!("[oblisk-renderer] {} unmapped: visible = false", self.surfaces[index].surface_id);
    }

    /// The re-map half: "The client can re-map the surface by performing a commit without any
    /// buffer attached, waiting for a configure event and handling it as usual."
    ///
    /// Every layer-shell field is re-sent, not just the ones a diff would find, because the same
    /// description says an unmapped surface "returns to the state it had right after
    /// layer_shell.get_layer_surface". `anchor` is included for that reason alone: it is a
    /// topology field that can never *change* on a live surface, but it can be reset out from
    /// under one. The exclusive zone is not re-sent here because it is not a spec field -- the
    /// configure re-derives it from the size the compositor grants.
    ///
    /// `set_size` needs no [`ambiguous_zero_axis`] guard: `applied_spec`'s size is only ever one
    /// that already passed it, in `create_panels` or in [`App::apply_spec_change`], which both
    /// refuse rather than store a size the protocol would reject.
    ///
    /// **Two starting states share this one request sequence and end in different `MapState`s**,
    /// and the difference is the compositor's, not a choice made here. Measured against niri with
    /// `WAYLAND_DEBUG=1`:
    ///
    /// - A panel declared `visible = false` at startup was *never mapped*. It performed the
    ///   initial commit `get_layer_surface` requires, was configured, and was acked; it simply
    ///   never attached a buffer. Its layer-surface state was never reset, so the commit below
    ///   changes nothing the compositor has an opinion about and **no configure comes back**. The
    ///   surface is already in the "acked a configure, may attach a buffer" state the protocol
    ///   describes, so it goes straight to [`MapState::Mapped`] and the next
    ///   [`App::repaint_mapped_surfaces`] binds and draws it.
    /// - A surface that really was mapped and then null-buffered *has* been reset, so this commit
    ///   is a fresh initial commit and a configure does come back. Attaching a buffer before
    ///   acking it is exactly what `get_layer_surface`'s description forbids, so that case waits
    ///   in [`MapState::AwaitingConfigure`] and lets `bind_and_clear` handle the configure with no
    ///   re-map special case at all.
    ///
    /// `bound.is_some()` is the honest test for which of the two this is: an EGL surface exists
    /// only for a surface that has been through the bind-and-paint path, and every trip through it
    /// ends in a `swap_buffers`, so the two questions are the same question.
    fn remap(&mut self, index: usize) {
        let was_mapped = self.surfaces[index].bound.is_some();
        let tracked = &self.surfaces[index];
        let spec = &tracked.applied_spec;
        tracked.layer.set_anchor(anchor_for(spec.topology.anchor));
        tracked.layer.set_size(
            layer_extent_for(spec.width, tracked.output_size.width),
            layer_extent_for(spec.height, tracked.output_size.height),
        );
        tracked.layer.set_keyboard_interactivity(keyboard_interactivity_for(spec.keyboard_interactivity));
        tracked.layer.set_margin(
            spec.margin.top as i32,
            spec.margin.right as i32,
            spec.margin.bottom as i32,
            spec.margin.left as i32,
        );
        tracked.layer.wl_surface().commit();
        self.surfaces[index].map_state =
            if was_mapped { MapState::AwaitingConfigure } else { MapState::Mapped };
        eprintln!("[oblisk-renderer] {} mapping: visible = true", self.surfaces[index].surface_id);
    }

    /// Creates this surface's `wl_egl_window` and EGL window surface against the shared context if
    /// it has none yet, and initializes the process-wide `glow` context on the first one. Returns
    /// whether the surface is bound afterwards; a failure is fatal (`self.exit`), exactly as it
    /// was before this was factored out of `bind_and_clear`.
    fn ensure_bound(&mut self, index: usize) -> bool {
        if self.surfaces[index].bound.is_some() {
            return true;
        }
        let surface_id = self.surfaces[index].surface_id.clone();
        let (width, height) = self.surfaces[index].configured_size;
        let width = width.max(1) as i32;
        let height = height.max(1) as i32;

        let native_window = match WlEglSurface::new(self.surfaces[index].layer.wl_surface().id(), width, height) {
            Ok(w) => w,
            Err(e) => {
                log_bind_failure(&surface_id, "WlEglSurface::new", e);
                self.exit = true;
                return false;
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
                log_bind_failure(&surface_id, "eglCreateWindowSurface", e);
                self.exit = true;
                return false;
            }
        };

        if let Err(e) = self.egl.instance.make_current(
            self.egl.display,
            Some(egl_surface),
            Some(egl_surface),
            Some(self.egl.context),
        ) {
            log_bind_failure(&surface_id, "eglMakeCurrent", e);
            self.exit = true;
            return false;
        }

        // SAFETY: `glow::Context::from_loader_function`'s contract is that a GL context is
        // current on this thread for the lifetime of the returned `Context` -- guaranteed here
        // by the `eglMakeCurrent` call directly above, on this same single-threaded dispatch
        // loop, with no other context switch between the two.
        self.gl.get_or_insert_with(|| unsafe {
            glow::Context::from_loader_function(|s| {
                self.egl
                    .instance
                    .get_proc_address(s)
                    .map_or(std::ptr::null(), |f| f as *const c_void)
            })
        });

        eprintln!("[oblisk-renderer] {surface_id} up: {width}x{height}, EGL context current");
        self.surfaces[index].bound = Some(BoundSurface { egl_surface, native_window });
        true
    }

    /// Draws one bound surface's whole retained tree (build-steps.md Phase 19 items 6 and 8):
    /// make its EGL surface current, resize the shared canvas to it, clear, walk the resolved tree
    /// with [`layout::paint::paint_tree`], and swap.
    ///
    /// **One `TextPainter` serves every surface**, and that is the item 8 claim this is the first
    /// production code to rest on. All surfaces share one EGL context; under EGL a context owns
    /// its GL objects while a surface is only the framebuffer being drawn into, so
    /// `eglMakeCurrent` with a different draw surface leaves the canvas's textures, shaders and
    /// glyph atlas valid. What genuinely is per surface is the canvas's *viewport*, which is what
    /// `TextPainter::resize` (and so `Canvas::set_size`) sets on every call here. If a live run
    /// ever shows otherwise, one canvas per surface is the fallback, not a redesign.
    ///
    /// A surface whose instance has no resolved tree (an evaluation that failed to apply, or an
    /// instance the scene has not resolved yet) is cleared and swapped, not skipped: the buffer
    /// still has to be attached or the compositor keeps showing the last frame.
    ///
    /// ponytail: the paint scale is hardcoded `1.0`. Nothing calls `wl_surface::set_buffer_scale`
    /// and the `wl_egl_window` is sized in the logical pixels `configure` reports, so on a HiDPI
    /// output the whole shell renders at scale 1 and the compositor upscales it. The upgrade path
    /// is all three together -- `set_buffer_scale`, a `WlEglSurface::resize` to the scaled
    /// physical size, and this argument -- since passing a scale here alone would snap geometry to
    /// a physical grid the framebuffer does not have.
    fn paint_surface(&mut self, index: usize) {
        // An unmapped surface has no buffer, and one still waiting for the configure that follows
        // its (re-)map commit may not attach one yet (docs/adr/0038 decision 2; see [`MapState`]).
        // `swap_buffers` at the bottom of this function is that attach *and* the commit carrying
        // it, so this guard is what keeps `visible = false` from quietly re-mapping the surface it
        // just hid.
        if self.surfaces[index].map_state != MapState::Mapped {
            return;
        }
        let Some(egl_surface) = self.surfaces[index].bound.as_ref().map(|b| b.egl_surface) else {
            return;
        };
        let surface_id = self.surfaces[index].surface_id.clone();
        let (width, height) = self.surfaces[index].configured_size;
        let (width, height) = (width.max(1), height.max(1));

        // Another surface's own paint may have made a different EGL surface current on this
        // thread since this one last drew -- the context is shared across every surface, so it is
        // re-established here rather than assumed still current.
        if let Err(e) = self.egl.instance.make_current(
            self.egl.display,
            Some(egl_surface),
            Some(egl_surface),
            Some(self.egl.context),
        ) {
            log_bind_failure(&surface_id, "eglMakeCurrent", e);
            self.exit = true;
            return;
        }

        // SAFETY: every `glow::HasContext` method call requires a current GL context matching
        // `gl`'s own loader -- the `eglMakeCurrent` above is that context, and it's the only one
        // live on this thread.
        if let Some(gl) = self.gl.as_ref() {
            unsafe {
                use glow::HasContext;
                gl.clear_color(0.0, 0.0, 0.0, 0.0);
                gl.clear(glow::COLOR_BUFFER_BIT);
            }
        }

        if self.text_painter.is_none() {
            let font_chain_bytes = self.shaping.font_chain_bytes();
            match TextPainter::new(
                |s| self.egl.instance.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void),
                width,
                height,
                &font_chain_bytes,
            ) {
                Ok(painter) => self.text_painter = Some(painter),
                Err(e) => {
                    log_bind_failure(&surface_id, "FemtoVG init", e);
                    self.exit = true;
                    return;
                }
            }
        }

        // Owned (`Scene::surface` clones into a `ResolvedNode`), so the immutable borrow of
        // `self.client` ends before `self.text_painter` is borrowed mutably below.
        let tree = self.client.scene().surface(&surface_id);
        if let Some(painter) = self.text_painter.as_mut() {
            painter.resize(width, height);
            if let Some(tree) = tree.as_ref() {
                layout::paint::paint_tree(painter, tree, 1.0);
            }
        }

        if let Err(e) = self.egl.instance.swap_buffers(self.egl.display, egl_surface) {
            log_bind_failure(&surface_id, "eglSwapBuffers", e);
            self.exit = true;
        }
    }

    /// Repaints every mapped surface, after a re-resolve actually changed the scene. Every
    /// surface, not the changed ones: ADR-0044 decision 2's dirty flag is one flag for the whole
    /// scene, so which surfaces changed is not information this process has (that flag's own
    /// `ponytail:` records the same ceiling).
    ///
    /// This is also the commit that carries everything [`App::apply_resolved_surface_state`]
    /// staged for each surface on the same poll turn -- `paint_surface` ends in `swap_buffers`,
    /// which is a `wl_surface` commit.
    fn repaint_mapped_surfaces(&mut self) {
        for index in 0..self.surfaces.len() {
            if self.surfaces[index].map_state != MapState::Mapped {
                continue;
            }
            if self.surfaces[index].bound.is_none() {
                // A panel that started `visible = false` and has just been mapped by a `visible`
                // flip has no EGL surface yet: it was created and configured, but the configure
                // path returned before `ensure_bound` because there was nothing to draw into it.
                // This is the one place that bind can happen, since no further configure is coming
                // (see [`App::remap`]).
                //
                // Never for a Candidate, whatever the scene does: § 15.2 point 3 keeps it
                // invisible until `ActivateDraw`, and `activate_draw_one` is its only bind.
                if self.is_pba_candidate || !self.ensure_bound(index) {
                    continue;
                }
            }
            self.paint_surface(index);
            if self.exit {
                return;
            }
        }
    }

    /// § 15.2 points 2-3: once every tracked surface has staged, computes the surface_id list the
    /// Supervisor will expect presentation evidence from and queues it once as a `ReadySignal`. A
    /// no-op if it's already been sent, or if some surface hasn't staged yet -- called on every
    /// candidate-mode configure, since any of them might be the one that completes the set.
    ///
    /// Two different sets, deliberately. The *gate* is every surface, because a Candidate is not
    /// ready until each one has been dealt with. The *payload* is only the surfaces that will
    /// present a frame, because a panel declared `visible = false` never will -- see
    /// [`presenting_surface_ids`] for what each direction of a mismatch costs.
    fn maybe_send_ready_signal(&mut self) {
        if self.ready_signal_sent || !self.surfaces.iter().all(|s| s.null_buffered) {
            return;
        }
        self.ready_signal_sent = true;
        let surfaces = presenting_surface_ids(self.surfaces.iter().map(|s| (s.surface_id.as_str(), s.map_state)));
        if let Err(e) = self.outbound_tx.send(RendererFrame::ReadySignal(ReadySignal { surfaces })) {
            eprintln!("[oblisk-renderer] failed to queue ReadySignal for the socket thread: {e}");
        }
    }

    /// § 15.3: draws the first real frame in response to `ActivateDraw`, requesting
    /// `wp_presentation_feedback` for each surface drawn. `nonce` is remembered as `active_nonce`
    /// so the later `presented` callback (this file's `PresentationTimeHandler` impl) knows
    /// which handshake attempt to tag its evidence with.
    ///
    /// The surfaces that present, not every tracked surface: exactly the set
    /// `maybe_send_ready_signal` announced, filtered by the same [`MapState::presents`] predicate
    /// over a `map_state` that [`App::apply_visibility`] holds still for a Candidate's whole life.
    /// That is what makes the announced set and the drawn set identical rather than merely similar
    /// -- see [`presenting_surface_ids`] for why "similar" is a hang.
    fn activate_draw(&mut self, nonce: u64) {
        self.active_nonce = Some(nonce);
        for index in 0..self.surfaces.len() {
            if !self.surfaces[index].map_state.presents() {
                continue;
            }
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

    /// One tracked surface's `ActivateDraw` response: the same EGL bind
    /// [`App::bind_and_clear`]'s non-candidate path does, plus a `wp_presentation_feedback`
    /// request placed before the paint so it associates with the commit `swap_buffers` performs.
    /// Indexes into `self.surfaces` rather than holding a `&mut TrackedSurface` across the whole
    /// body -- this needs `&mut self` for EGL/GL state and `self.text_painter` at several points,
    /// which a held borrow of one surface would conflict with.
    fn activate_draw_one(&mut self, index: usize, nonce: u64) {
        if !self.ensure_bound(index) {
            return;
        }

        // § 15.3 point 2: request presentation feedback before the commit `paint_surface`'s
        // `swap_buffers` performs, so the request associates with it -- confirmed against
        // `wayland-client-0.31.15`'s own client examples' placement convention; verify with
        // `WAYLAND_DEBUG=1` during a manual smoke test that `feedback` appears on the wire
        // before the corresponding `commit`.
        if let Err(e) = self.presentation_time.feedback(self.surfaces[index].layer.wl_surface(), &self.queue_handle) {
            // Not fatal to the whole candidate -- the Supervisor's evidence_timeout is what
            // catches a surface that never presents (docs/adr/0025 item 6); don't invent a
            // second failure-reporting path here.
            log_bind_failure(&self.surfaces[index].surface_id.clone(), "wp_presentation::feedback", e);
        }

        self.paint_surface(index);
        if self.exit {
            return;
        }

        let (width, height) = self.surfaces[index].configured_size;
        eprintln!(
            "[oblisk-renderer] {} activated: {width}x{height}, presentation feedback requested (nonce={nonce})",
            self.surfaces[index].surface_id
        );
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
                log_bind_failure("<text-input>", "zwp_text_input_manager_v3::bind", e);
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

    /// The handled `button` under a pointer event on surface `index`, as its absolute rect and a
    /// clone of its `on_click` (docs/adr/0050 decision 1). `None` if the point misses every one.
    ///
    /// `position` is surface-local and *logical*, which is the space `layout::hit` walks
    /// `ResolvedNode::rect` in, so there is no conversion here at all. That holds only while
    /// `paint_surface` paints at scale `1.0` and nothing calls `wl_surface::set_buffer_scale`;
    /// docs/adr/0050's consequences name this as the third caller the HiDPI change from Phase 20
    /// has to move together with `paint_surface` and `apply_input_region`.
    ///
    /// Owned on the way out, both halves. `Scene::surface` clones into a `ResolvedNode` (the same
    /// property `paint_surface` relies on), so the borrow of `self.client` ends on that line, and
    /// the `Function` is cloned out of the local tree before it is dropped.
    fn button_under(&self, index: usize, position: (f64, f64)) -> Option<(LogicalRect, Function)> {
        let tree = self.client.scene().surface(&self.surfaces[index].surface_id)?;
        let point = layout::hit::LogicalPoint { x: position.0 as f32, y: position.1 as f32 };
        let path = layout::hit::hit_path(&tree, point);
        clickable_button(&path).map(|(rect, on_click)| (rect, on_click.clone()))
    }

    /// Calls one `button`'s `on_click` with its rect (docs/adr/0050 decision 3) and marks the
    /// scene dirty.
    ///
    /// A raise is logged against the surface it happened on and swallowed. A broken `on_click` is
    /// a config bug, and a config bug must not take a shell that is otherwise painting down with
    /// it; docs/adr/0046's rescue path is for an evaluation that failed, not for one misbehaving
    /// handler, so this deliberately does not set `self.exit` and deliberately does not enter
    /// rescue.
    ///
    /// The dirty mark is not conditional on the call succeeding: a handler that raised halfway
    /// may already have written whatever it wrote. Nothing re-resolves here -- `run`'s poll loop
    /// calls `dispatch_pending` (which is where this runs) at the top of the same turn whose
    /// `re_resolve_if_dirty`/`repaint_mapped_surfaces` pair then picks the mark up, so a click is
    /// on screen one turn later without this function knowing anything about painting.
    fn fire_on_click(&mut self, instance_id: &str, rect: LogicalRect, on_click: &Function) {
        // The table is built and the `&Lua` borrow released before the call, so no borrow of
        // `self.client` is live while Lua runs inside it.
        let argument = match rect_table(self.client.lua(), rect) {
            Ok(table) => table,
            Err(e) => {
                eprintln!("[oblisk-renderer] {instance_id}: could not build on_click's rect argument: {e}");
                return;
            }
        };
        // Nothing marks the scene dirty here. A handler that changes what is painted does it by
        // writing a `state(name, initial)` signal, and `signal:set()` marks the flag itself
        // (ADR-0044 decision 5); a handler that writes nothing correctly causes no re-resolve.
        if let Err(e) = on_click.call::<()>(argument) {
            eprintln!("[oblisk-renderer] {instance_id}: on_click raised, ignoring it: {e}");
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
/// A free function, not a `&mut self` method, for the same reason `apply_edit` is: it makes the whole read/zeroize contract directly unit-testable, which
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

    /// Pointer only. `bind_text_input` needs the bare `wl_seat` rather than a capability, and no
    /// keyboard or touch object is created from one either -- keyboard focus is build-steps.md
    /// Phase 21 item 2, and § 5.2 has no touch-specific property for a third to serve.
    ///
    /// Idempotent by the `is_some` guard, not by trusting the compositor: `wl_seat::capabilities`
    /// is a full re-statement of the current set on every change, so a seat that gains a keyboard
    /// re-announces its pointer, and SCTK turns each announcement into this call. Creating a
    /// second `wl_pointer` there would leave two objects delivering the same events into one
    /// `armed` slot.
    fn new_capability(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        if !matches!(capability, Capability::Pointer) || self.pointer.is_some() {
            return;
        }
        match self.seat_state.get_pointer(qh, &seat) {
            Ok(pointer) => self.pointer = Some(pointer),
            // Not fatal: a shell with no pointer still paints, still reloads, and still takes
            // `wp-text-input-v3` input. Only `on_click` stops working, which is what this says.
            Err(e) => eprintln!("[oblisk-renderer] wl_seat::get_pointer failed; no button's on_click will ever fire: {e}"),
        }
    }

    fn remove_capability(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat, capability: Capability) {
        if !matches!(capability, Capability::Pointer) {
            return;
        }
        // A pointer that is gone will never send the `release` this press was waiting for, which
        // is the same reason `leave` clears it (docs/adr/0050 decision 2).
        self.armed = None;
        if let Some(pointer) = self.pointer.take() {
            // `wl_pointer::release` is `since="3"`; below that the destructor does not exist and
            // dropping the proxy is the whole cleanup. Same guard SCTK's own `ThemedPointer::drop`
            // applies (src/seat/pointer/mod.rs:572).
            if pointer.version() >= 3 {
                pointer.release();
            }
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}
}

/// Pointer input to `on_click` (build-steps.md Phase 21 item 1, docs/adr/0050).
///
/// No `delegate_pointer!` call accompanies this, and adding one would not compile: this SCTK has
/// no such macro, and `PointerData<U>` carries a blanket `Dispatch2<WlPointer, D>` impl
/// (src/seat/pointer/mod.rs:209) that the file-wide `delegate_dispatch2!(App)` at the bottom
/// already turns into the `Dispatch<WlPointer, PointerData<()>>` half of `get_pointer`'s bound.
/// This trait is the only half left to supply.
impl PointerHandler for App {
    fn pointer_frame(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _pointer: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for event in events {
            // A surface this process does not own: a `wl_pointer` is per seat, not per surface,
            // and nothing stops the compositor from having delivered an event for a surface that
            // has since been destroyed by a `visible` flip or an output change.
            let Some(index) = self.surfaces.iter().position(|s| s.layer.wl_surface() == &event.surface) else {
                continue;
            };
            match event.kind {
                // `BTN_LEFT` alone (docs/adr/0050 decision 2). A right-click has no meaning in the
                // IDL, and inventing one here would be policy no config could override.
                PointerEventKind::Press { button: BTN_LEFT, .. } => {
                    let instance_id = self.surfaces[index].surface_id.clone();
                    self.armed = self.button_under(index, event.position).map(|(rect, _)| ArmedClick { instance_id, rect });
                }
                PointerEventKind::Release { button: BTN_LEFT, .. } => {
                    let instance_id = self.surfaces[index].surface_id.clone();
                    let hit = self.button_under(index, event.position);
                    let fires = release_completes_click(self.armed.as_ref(), &instance_id, hit.as_ref().map(|(rect, _)| *rect));
                    // Unconditionally, and before the call: a release ends this press whether or
                    // not it fired, and a handler that re-enters here must not find it still set.
                    self.armed = None;
                    if let Some((rect, on_click)) = hit.filter(|_| fires) {
                        self.fire_on_click(&instance_id, rect, &on_click);
                    }
                }
                // The pointer left the surface, so the release (if it ever comes) lands somewhere
                // else. This is the drag-off-and-cancel decision 2 is built around.
                PointerEventKind::Leave { .. } => self.armed = None,
                // `Enter`/`Motion`/`Axis`: nothing in § 5.2 reads hover or scroll yet, and a
                // motion that leaves the armed rect deliberately does *not* disarm -- dragging
                // back onto the button and releasing still clicks it, which is what every toolkit
                // does.
                _ => {}
            }
        }
    }
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

    // All three do the same two things, because one `wl_output` event owes both: update the
    // `screens` signal, then ask for a re-evaluation (docs/adr/0041 decisions 2 and 4). See
    // [`App::handle_output_change`].
    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.handle_output_change(qh, None);
    }

    // Not only a mode or scale change: `smithay_client_toolkit` also routes an output's *first*
    // `xdg_output` arrival here rather than to `new_output` when the `wl_output` was already
    // known, so this is a real path for a monitor's size becoming knowable, not just for one
    // changing.
    fn update_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.handle_output_change(qh, None);
    }

    fn output_destroyed(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        // Passed through explicitly because `smithay_client_toolkit`'s `remove_global` calls this
        // *before* removing the output from its own `OutputState` -- a plain read of `outputs()`
        // from in here still lists the monitor that just went away, so it has to be excluded by
        // identity (see [`App::screens`]).
        self.handle_output_change(qh, Some(&output));
    }
}

impl LayerShellHandler for App {
    /// `zwlr_layer_surface_v1::closed` means *this* surface is gone and must be destroyed -- the
    /// compositor sends it when the output the surface was on is destroyed, which is exactly
    /// docs/adr/0038 decision 3's removal half arriving by the layer-shell route instead of the
    /// `wl_output` one. It is not a shutdown signal.
    ///
    /// This used to set `self.exit`, which was defensible while one hardcoded bar was the only
    /// surface and is not once a config declares N of them across M monitors: unplugging one
    /// external display would have killed a shell still painting on the laptop panel, which is
    /// precisely the generation-swap-free in-place handling the ADR forbids swapping for.
    ///
    /// ponytail: a compositor that closes *every* surface therefore leaves this process alive with
    /// nothing on screen rather than exiting. That is the right answer for the hotplug case (the
    /// monitors coming back is another output change, not a new generation) and the wrong one for
    /// a compositor shutting down -- which in practice drops the Wayland connection a moment
    /// later, and `run`'s `dispatch_pending` fails out of the loop on its own. Upgrade path: exit
    /// on a `closed` that no output change explains, which needs the two events correlated.
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        let Some(surface_id) = self.surfaces.iter().find(|s| &s.layer == layer).map(|s| s.surface_id.clone()) else {
            return;
        };
        self.destroy_surface_by_id(&surface_id);
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
    fn every_layer_kind_maps_to_its_protocol_level() {
        assert_eq!(layer_for(LayerKind::Background), Layer::Background);
        assert_eq!(layer_for(LayerKind::Bottom), Layer::Bottom);
        assert_eq!(layer_for(LayerKind::Top), Layer::Top);
        assert_eq!(layer_for(LayerKind::Overlay), Layer::Overlay);
    }

    #[test]
    fn anchor_booleans_map_to_the_matching_bitflags() {
        assert_eq!(anchor_for(node::Anchor::default()), Anchor::empty());
        assert_eq!(
            anchor_for(node::Anchor { top: true, right: true, bottom: false, left: true }),
            Anchor::TOP | Anchor::RIGHT | Anchor::LEFT,
            "the ordinary bar shape: pinned to the top, spanning both sides"
        );
        assert_eq!(
            anchor_for(node::Anchor { top: true, right: true, bottom: true, left: true }),
            Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT
        );
    }

    #[test]
    fn every_keyboard_interactivity_maps_to_its_protocol_mode() {
        assert_eq!(keyboard_interactivity_for(node::KeyboardInteractivity::None), KeyboardInteractivity::None);
        assert_eq!(keyboard_interactivity_for(node::KeyboardInteractivity::OnDemand), KeyboardInteractivity::OnDemand);
        assert_eq!(keyboard_interactivity_for(node::KeyboardInteractivity::Exclusive), KeyboardInteractivity::Exclusive);
    }

    #[test]
    fn layer_extent_maps_fill_and_content_to_the_protocols_zero_and_resolves_a_percent() {
        assert_eq!(layer_extent_for(SizeMode::Fill, 1920.0), 0, "`Fill` means the anchors decide, which the protocol spells 0");
        assert_eq!(layer_extent_for(SizeMode::Content, 1920.0), 0, "an omitted width has no measured content at creation time either");
        assert_eq!(layer_extent_for(SizeMode::Pixels(32.0), 1920.0), 32);
        assert_eq!(layer_extent_for(SizeMode::Percent(0.5), 1920.0), 960);
    }

    #[test]
    fn a_zero_axis_is_only_legal_when_both_of_that_axiss_edges_are_anchored() {
        // The `set_size` protocol-error rule. Getting this wrong kills the whole connection, so a
        // config that trips it must be refused per surface instead.
        let bar = node::Anchor { top: true, right: true, bottom: false, left: true };
        assert_eq!(ambiguous_zero_axis((0, 32), bar), None, "width 0 is fine: left and right are both anchored");
        assert_eq!(ambiguous_zero_axis((0, 0), bar), Some("height"), "height 0 with only the top edge anchored is the protocol error");

        let corner = node::Anchor { top: true, right: true, bottom: false, left: false };
        assert_eq!(ambiguous_zero_axis((0, 40), corner), Some("width"));
        assert_eq!(ambiguous_zero_axis((380, 40), corner), None, "an explicit size on both axes is always legal");

        let full = node::Anchor { top: true, right: true, bottom: true, left: true };
        assert_eq!(ambiguous_zero_axis((0, 0), full), None, "a fullscreen surface may leave both axes to the compositor");
    }

    #[test]
    fn an_exclusive_bar_reserves_its_configured_height_and_a_dock_its_width() {
        // Derived from the size the compositor granted, which is why this is a configure-time
        // computation: at creation a `"Fill"`-sized bar has no height to reserve.
        let bar = node::Anchor { top: true, right: true, bottom: false, left: true };
        assert_eq!(exclusive_zone_for(bar, (1920, 32)), 32);

        let bottom_dock = node::Anchor { top: false, right: true, bottom: true, left: true };
        assert_eq!(exclusive_zone_for(bottom_dock, (1920, 48)), 48);

        let side_dock = node::Anchor { top: true, right: false, bottom: true, left: true };
        assert_eq!(exclusive_zone_for(side_dock, (64, 1080)), 64);
    }

    #[test]
    fn an_ambiguously_anchored_surface_reserves_nothing() {
        // All four edges, none of them, and a single corner: in each case there is no one edge to
        // reserve against, and the protocol's exclusive-zone wording only defines the strip cases.
        let all = node::Anchor { top: true, right: true, bottom: true, left: true };
        assert_eq!(exclusive_zone_for(all, (1920, 1080)), 0);
        assert_eq!(exclusive_zone_for(node::Anchor::default(), (400, 300)), 0);
        let corner = node::Anchor { top: true, right: false, bottom: false, left: true };
        assert_eq!(exclusive_zone_for(corner, (400, 300)), 0);
    }

    fn panel(id: &str) -> PanelSpec {
        PanelSpec {
            topology: node::SurfaceTopology {
                id: id.to_string(),
                layer: LayerKind::Top,
                anchor: node::Anchor { top: true, right: true, bottom: false, left: true },
                monitor: "All".to_string(),
                namespace: format!("oblisk-{id}"),
            },
            keyboard_interactivity: node::KeyboardInteractivity::None,
            exclusive: true,
            margin: node::EdgeInsets::default(),
            width: SizeMode::Fill,
            height: SizeMode::Pixels(32.0),
        }
    }

    fn facts(name: Option<&str>) -> OutputFacts {
        OutputFacts {
            name: name.map(str::to_string),
            logical_size: Some((1920, 1080)),
            current_mode: Some(((1920, 1080), 60_000)),
            scale_factor: 1,
        }
    }

    #[test]
    fn a_screens_entry_reports_the_logical_size_in_preference_to_the_current_modes_dimensions() {
        // A 3840x2160 panel driven at scale 2 is 1920x1080 of compositor space, which is the
        // coordinate system a layer surface's own geometry is in -- so the mode's raw dimensions
        // would put a config's own arithmetic on a different grid than the engine's.
        let mut facts = facts(Some("eDP-1"));
        facts.logical_size = Some((1920, 1080));
        facts.current_mode = Some(((3840, 2160), 60_000));
        facts.scale_factor = 2;

        let screen = screen_entry(0, &facts).expect("a logical size is enough on its own");
        assert_eq!((screen.width, screen.height), (1920, 1080));
        assert_eq!(screen.scale, 2);
    }

    #[test]
    fn a_screens_entry_falls_back_to_the_current_modes_dimensions_when_no_logical_size_is_reported() {
        // A compositor below wl_output v4, or one that has not sent an xdg_output yet.
        let mut facts = facts(Some("eDP-1"));
        facts.logical_size = None;
        facts.current_mode = Some(((1366, 768), 60_000));

        let screen = screen_entry(0, &facts).expect("the current mode is the documented fallback");
        assert_eq!((screen.width, screen.height), (1366, 768));
    }

    #[test]
    fn an_output_reporting_neither_a_logical_size_nor_a_current_mode_yields_no_screen_at_all() {
        // Not defaulted to some invented size: every surface on that monitor would then resolve
        // against a fiction, and the caller logs the miss instead.
        let mut facts = facts(Some("eDP-1"));
        facts.logical_size = None;
        facts.current_mode = None;

        assert!(screen_entry(0, &facts).is_none());
    }

    #[test]
    fn refresh_reaches_lua_in_hertz_although_wl_output_reports_millihertz() {
        assert_eq!(screen_entry(0, &facts(Some("eDP-1"))).unwrap().refresh, 60.0);

        // A real 144Hz panel's advertised rate is not a round number, so the division must keep
        // its fraction rather than truncating to an integer.
        let mut odd = facts(Some("DP-1"));
        odd.current_mode = Some(((2560, 1440), 143_868));
        assert_eq!(screen_entry(0, &odd).unwrap().refresh, 143.868);
    }

    #[test]
    fn a_screen_sized_from_its_logical_size_alone_reports_a_refresh_of_zero() {
        // `Mode`'s own docs allow a zero refresh rate for a virtual output, so zero is already
        // this field's "no real answer" value -- an output with no current mode at all reads the
        // same way rather than needing a separate nil case a config would have to guard.
        let mut facts = facts(Some("HEADLESS-1"));
        facts.current_mode = None;
        assert_eq!(screen_entry(0, &facts).unwrap().refresh, 0.0);
    }

    #[test]
    fn an_unnamed_output_takes_its_positional_id_so_the_shell_still_works_below_wl_output_v4() {
        let screen = screen_entry(2, &facts(None)).unwrap();
        assert_eq!(screen.name, "output-2");
    }

    #[test]
    fn the_screens_payload_is_the_array_of_field_tables_a_config_loops_over() {
        let screens = [
            screen_entry(0, &facts(Some("eDP-1"))).unwrap(),
            screen_entry(1, &facts(Some("DP-1"))).unwrap(),
        ];

        assert_eq!(
            screens_payload(&screens),
            serde_json::json!([
                { "name": "eDP-1", "width": 1920, "height": 1080, "scale": 1, "refresh": 60.0 },
                { "name": "DP-1", "width": 1920, "height": 1080, "scale": 1, "refresh": 60.0 },
            ])
        );
    }

    #[test]
    fn instance_expansion_reads_the_same_screen_list_the_signal_does() {
        // One source, two consumers (docs/adr/0041 decision 2): a `monitor` match and a `screens`
        // entry must never be able to disagree about which monitors exist or how large they are.
        let screens = [screen_entry(0, &facts(Some("eDP-1"))).unwrap()];
        assert_eq!(
            geometries_from(&screens),
            [OutputGeometry { name: "eDP-1".to_string(), size: layout::LogicalSize { width: 1920.0, height: 1080.0 } }]
        );
    }

    fn output_1080p() -> layout::LogicalSize {
        layout::LogicalSize { width: 1920.0, height: 1080.0 }
    }

    #[test]
    fn the_ready_signal_announces_every_surface_that_will_present_and_no_others() {
        // The one place a mistake hangs the shell instead of failing a test: `activate_draw` draws
        // exactly this set, `run_pba` expects evidence from exactly this set, and both directions
        // of a mismatch abort or time out the Candidate.
        let surfaces = [
            ("bar@eDP-1", MapState::Mapped),
            ("launcher@eDP-1", MapState::Unmapped),
            ("dock@DP-1", MapState::AwaitingConfigure),
        ];
        assert_eq!(
            presenting_surface_ids(surfaces.into_iter()),
            ["bar@eDP-1", "dock@DP-1"],
            "a panel declared `visible = false` is created and staged, but never presents a frame, so it must not be expected to"
        );
    }

    #[test]
    fn a_generation_whose_every_panel_starts_hidden_announces_nothing_at_all() {
        // Legal, not degenerate: `drive_handshake`'s `while collected.len() < expected.len()` loop
        // exits immediately on an empty expected set, so this Candidate completes its handshake.
        let surfaces = [("launcher@eDP-1", MapState::Unmapped)];
        assert!(presenting_surface_ids(surfaces.into_iter()).is_empty());
    }

    #[test]
    fn a_re_resolve_that_changed_nothing_sends_no_requests_at_all() {
        let applied = panel("bar");
        assert_eq!(spec_update(&applied, &applied.clone(), output_1080p()), SpecUpdate::default());
    }

    #[test]
    fn each_in_place_field_is_pushed_on_its_own_and_only_when_it_moved() {
        let applied = panel("bar");

        let mut moved_margin = applied.clone();
        moved_margin.margin = node::EdgeInsets { top: 12.0, right: 12.0, bottom: 0.0, left: 0.0 };
        assert_eq!(
            spec_update(&applied, &moved_margin, output_1080p()),
            SpecUpdate { margin: Some(moved_margin.margin), ..SpecUpdate::default() }
        );

        let mut takes_typing = applied.clone();
        takes_typing.keyboard_interactivity = node::KeyboardInteractivity::Exclusive;
        assert_eq!(
            spec_update(&applied, &takes_typing, output_1080p()),
            SpecUpdate {
                keyboard_interactivity: Some(node::KeyboardInteractivity::Exclusive),
                ..SpecUpdate::default()
            }
        );

        let mut stops_reserving = applied.clone();
        stops_reserving.exclusive = false;
        assert_eq!(
            spec_update(&applied, &stops_reserving, output_1080p()),
            SpecUpdate { exclusive: Some(false), ..SpecUpdate::default() }
        );
    }

    #[test]
    fn a_size_change_is_diffed_as_the_pixels_that_go_on_the_wire_not_as_the_size_mode() {
        let applied = panel("bar");

        let mut taller = applied.clone();
        taller.height = SizeMode::Pixels(48.0);
        assert_eq!(
            spec_update(&applied, &taller, output_1080p()),
            SpecUpdate { size: Some((0, 48)), ..SpecUpdate::default() },
            "`Fill` stays the protocol's 0 on the width axis; only the height moved"
        );

        // A percent resolves against the *output*, so half of a 1080p height is the same request
        // as an explicit 540, and neither is a change against the other.
        let mut half_by_percent = applied.clone();
        half_by_percent.height = SizeMode::Percent(0.5);
        let mut half_by_pixels = applied.clone();
        half_by_pixels.height = SizeMode::Pixels(540.0);
        assert_eq!(spec_update(&half_by_percent, &half_by_pixels, output_1080p()), SpecUpdate::default());
    }

    #[test]
    fn a_size_change_a_signal_could_make_is_refused_by_the_same_guard_creation_uses() {
        // `height` is an ordinary resolvable property, so a `Signal` can turn a fixed 32 into
        // `"Fill"` at runtime -- and `set_size(_, 0)` on a surface anchored to one vertical edge
        // is a protocol error that kills the connection and the whole shell with it. The guard has
        // to run on the update path, not only at creation.
        let applied = panel("bar");
        let mut filled = applied.clone();
        filled.height = SizeMode::Fill;

        let size = spec_update(&applied, &filled, output_1080p()).size.expect("the height moved from 32 to 0");
        assert_eq!(ambiguous_zero_axis(size, filled.topology.anchor), Some("height"));
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

    fn hit_node(lua: &Lua, kind: &str, (x, y, width, height): (f32, f32, f32, f32), on_click: bool) -> layout::ResolvedNode {
        let mut properties = HashMap::new();
        if on_click {
            properties.insert("on_click".to_string(), Value::Function(lua.create_function(|_, ()| Ok(())).unwrap()));
        }
        layout::ResolvedNode {
            kind: kind.to_string(),
            rect: LogicalRect { x, y, width, height },
            visible: true,
            properties,
            children: Vec::new(),
        }
    }

    #[test]
    fn the_innermost_handled_button_under_the_pointer_is_the_one_that_would_fire() {
        // The shape decision 1 exists for: the deepest node is the `text`, and the outer `row`
        // is not a button, so only the middle node answers.
        let lua = Lua::new();
        let mut button = hit_node(&lua, "button", (10.0, 4.0, 40.0, 24.0), true);
        button.children.push(hit_node(&lua, "text", (6.0, 5.0, 28.0, 14.0), false));
        let mut row = hit_node(&lua, "row", (0.0, 0.0, 100.0, 32.0), false);
        row.children.push(button);
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        root.children.push(row);

        let path = layout::hit::hit_path(&root, layout::hit::LogicalPoint { x: 20.0, y: 12.0 });
        let (rect, _) = clickable_button(&path).expect("the button carries an on_click");
        assert_eq!(rect, LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 });
    }

    #[test]
    fn a_button_with_no_on_click_is_transparent_rather_than_a_barrier() {
        // An unhandled `button` nested inside a handled one must not swallow the click: the scan
        // keeps walking outwards past it.
        let lua = Lua::new();
        let inner = hit_node(&lua, "button", (5.0, 2.0, 20.0, 20.0), false);
        let mut outer = hit_node(&lua, "button", (10.0, 4.0, 40.0, 24.0), true);
        outer.children.push(inner);
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        root.children.push(outer);

        let path = layout::hit::hit_path(&root, layout::hit::LogicalPoint { x: 20.0, y: 12.0 });
        assert_eq!(path.len(), 3, "the inner button is still on the path");
        let (rect, _) = clickable_button(&path).expect("the outer button carries the on_click");
        assert_eq!(rect, LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 });
    }

    #[test]
    fn an_on_click_that_is_not_a_function_is_not_a_click_handler() {
        // Nothing in `layout::node` parses this key (§ 5.2 leaves it opaque), so a config writing
        // `on_click = "quit"` reaches here as a string and must simply not fire.
        let lua = Lua::new();
        let mut button = hit_node(&lua, "button", (0.0, 0.0, 40.0, 24.0), false);
        button.properties.insert("on_click".to_string(), Value::String(lua.create_string("quit").unwrap()));
        let mut root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        root.children.push(button);

        let path = layout::hit::hit_path(&root, layout::hit::LogicalPoint { x: 20.0, y: 12.0 });
        assert!(clickable_button(&path).is_none());
    }

    #[test]
    fn a_release_fires_only_over_the_same_surface_and_the_same_rect_the_press_armed() {
        let rect = LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 };
        let moved = LogicalRect { x: 11.0, y: 4.0, width: 40.0, height: 24.0 };
        let armed = ArmedClick { instance_id: "bar@eDP-1".to_string(), rect };

        assert!(release_completes_click(Some(&armed), "bar@eDP-1", Some(rect)));
        // Dragged off the button, then released: the release hits no button at all.
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", None));
        // Dragged onto a different button on the same surface.
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", Some(moved)));
        // Same button geometry, different surface -- two panels can resolve identical rects.
        assert!(!release_completes_click(Some(&armed), "notification_area@eDP-1", Some(rect)));
        // A release with nothing armed (a press that hit no button, or a `leave` in between).
        assert!(!release_completes_click(None, "bar@eDP-1", Some(rect)));
    }

    #[test]
    fn on_clicks_argument_is_the_buttons_rect_as_four_named_fields() {
        let lua = Lua::new();
        let table = rect_table(&lua, LogicalRect { x: 10.5, y: 4.0, width: 40.0, height: 24.0 }).unwrap();
        assert_eq!(table.get::<f32>("x").unwrap(), 10.5);
        assert_eq!(table.get::<f32>("y").unwrap(), 4.0);
        assert_eq!(table.get::<f32>("width").unwrap(), 40.0);
        assert_eq!(table.get::<f32>("height").unwrap(), 24.0);
    }
}
