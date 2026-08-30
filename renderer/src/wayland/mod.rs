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

/// The protocol object one tracked surface's `wl_surface` has been given a role by, together with
/// the spec that object's state was last set from (§ 6, docs/adr/0040 decision 1). One enum rather
/// than two parallel `Vec<TrackedSurface>`s, because everything *around* the role object -- the EGL
/// binding, the paint pass, the input routing, the PBA staging -- is identical across roles and
/// indexes into one `App::surfaces`; splitting the vec would fork all of it.
///
/// **The variants differ in exactly one thing, and it is the whole of docs/adr/0049 decision 1: how
/// long the Wayland object lives.** A `panel`'s is created at generation startup and kept for the
/// generation's whole life, with `visible` mapping and unmapping it. A `window`'s exists only while
/// shown, which is why its object is an `Option` and a panel's is not.
enum TrackedRole {
    Panel {
        layer: LayerSurface,
        /// The `panel` spec this surface's layer-shell state was last set from -- the diff baseline
        /// [`spec_update`] compares a freshly resolved root against, so a re-resolve pushes only
        /// the fields that actually moved (docs/adr/0038 decision 2, build-steps.md Phase 20
        /// item 1).
        ///
        /// Also the standing answer to "what is this surface's anchor and is it exclusive", which
        /// [`App::apply_exclusive_zone`] needs once a `configure` says how large the surface is.
        spec: PanelSpec,
        /// This surface's output's logical size, the basis a `SizeMode::Percent` resolves against.
        ///
        /// Kept per surface rather than read back from `SurfaceInstance::available`, which is the
        /// same number only until the first `configure`: `set_instance_size` then replaces
        /// `available` with the size the compositor granted, so resolving a percent against it on a
        /// later re-resolve would take a percentage of a percentage and shrink the surface on every
        /// push.
        ///
        /// Panel-only because `layer_extent_for` is: § 6.2 gives a `window` no `width`/`height` at
        /// all, so a toplevel has no size request to resolve a percent for.
        output_size: layout::LogicalSize,
    },
    Window {
        /// `None` whenever `visible` is false, which for this role means the `xdg_toplevel`, its
        /// `xdg_surface` and its `wl_surface` do not exist at all (docs/adr/0049 decision 1). This
        /// is the memory win that ADR gives: a declared-but-never-shown window costs one retained
        /// node and zero Wayland objects, buffers, or EGL surfaces.
        window: Option<Window>,
        /// The `window` spec this toplevel's state was last set from, kept for the same reason a
        /// panel's is: [`window_update`]'s diff baseline. Maintained even while `window` is `None`,
        /// so the toplevel [`App::show_window`] creates is built from the spec the last re-resolve
        /// produced rather than the one the evaluation happened to parse.
        spec: WindowSpec,
    },
    Popup {
        /// `None` whenever this popup is not currently shown, for the same reason a `window`'s is
        /// and one stronger: `xdg_positioner` is consumed by `get_popup`, so a popup created once
        /// is anchored once (docs/adr/0049's opening argument). Every open builds a fresh
        /// positioner, a fresh `wl_surface` and a fresh `xdg_popup`.
        popup: Option<Popup>,
        /// The `popup` spec the next open will be built from. Unlike a `panel`'s or a `window`'s
        /// this is **not** a diff baseline, because there is nothing to diff against: every field
        /// on it is an `xdg_positioner` request, the positioner is consumed at creation, and
        /// `xdg_popup.reposition` -- the one request that could move a live popup -- is deliberately
        /// not built (docs/adr/0040, build-steps.md Phase 22's "deliberately deferred"). So this is
        /// a store, re-read whole at the next [`App::show_popup`].
        spec: PopupSpec,
        /// docs/adr/0051 decision 2's latch: [`App::pointer_input_count`] as it stood when the
        /// compositor dismissed this popup, or `None` if it has not been dismissed. No replacement
        /// may be created while that counter is still unmoved.
        ///
        /// Without the latch a click-outside is a livelock rather than a dismissal. `popup_done`
        /// destroys the object but leaves the resolved tree still saying `visible = true`, so the
        /// next re-resolve would create a second popup for the same click-outside to dismiss,
        /// forever. A config with no `on_dismiss` at all is not a config error and must not be that.
        ///
        /// A count rather than the `bool` decision 2 first asked for, per docs/adr/0051's first
        /// amendment: the `visible = false` edge that was supposed to clear it is unobservable in
        /// the case that matters, so the bool latched permanently and the dropdown died with the
        /// generation. See [`popup_visibility_action`], which reads it.
        ///
        /// Per declaration and per generation: it lives here, so a PBA swap starts every popup
        /// unlatched, which is correct because a new generation has shown nothing yet.
        dismissed_at: Option<u64>,
        /// Which of [`App::show_popup`]'s refusals was last logged for this run of `visible = true`,
        /// or `None` if none has been. docs/adr/0049's amendment says a refusal is *logged once*,
        /// and this is the once.
        ///
        /// Which one, rather than a bool per reason, because all three refusals answer the same
        /// question and a config hits them one at a time: a `parent` that names a hidden window
        /// still logs its line after the serial refusal already logged its own. Three parallel
        /// bools would be the same state clumped into three fields nothing keeps in step.
        ///
        /// It is not a second latch and deliberately does not stop the retry. A popup declared
        /// `visible = true` outright is refused on every re-resolve until one of them happens to be
        /// input-driven, and then it opens -- which is the rule working, not a special case, because
        /// the click that armed that serial is real user input. What must not repeat is the line: a
        /// re-resolve runs per capability push (ADR-0044 decision 2), so an unconditional
        /// `eprintln!` here writes several lines a second for as long as the config says `true`.
        ///
        /// Cleared on the same two edges the object's own lifetime turns on: a successful create,
        /// and `visible` resolving false. So a config that fixes itself says so again if it breaks
        /// again.
        refusal_logged: Option<PopupRefusal>,
    },
    Lock {
        /// The output this lock surface covers, held from the moment the instance was expanded
        /// rather than looked up again when the lock is taken. `get_lock_surface` takes a
        /// `wl_output` and § 6.4 gives a `lock` no `monitor` to name one with, so the instance's
        /// own output is the only possible answer -- and it is docs/adr/0041's output tracking
        /// rather than a second source of it, since [`App::create_surfaces`] is handed this proxy
        /// by the same map that places a `panel`.
        output: wl_output::WlOutput,
        /// `None` until this process holds the lock (docs/adr/0052 decision 2). The `Option` is a
        /// `window`'s with the trigger moved: a `window`'s object appears when the config says
        /// `visible`, and a lock surface's appears when the *compositor* has granted the lock, so
        /// a declared lock screen costs one retained node and zero Wayland objects for as long as
        /// the session is unlocked, which is nearly always.
        ///
        /// **Dropping this handle is the teardown, and nothing else is.**
        /// `SessionLockSurfaceInner::Drop` sends `ext_session_lock_surface_v1.destroy`, which the
        /// protocol *recommends* once the surface's `wl_output` global is gone and which makes the
        /// compositor "fall back to rendering a solid color" on an output that is still there. So
        /// the only two things that may clear this are an output removal
        /// ([`App::destroy_surface_by_id`], which drops the whole entry) and the end of the lock
        /// ([`App::teardown_lock_surfaces`]).
        surface: Option<SessionLockSurface>,
    },
}

impl TrackedRole {
    /// This surface's `wl_surface`, or `None` for a `window` or `popup` that is not currently shown
    /// -- the one question every role-agnostic path in this file asks (finding a surface by the one
    /// a `Dispatch` callback names, staging a null buffer, requesting presentation feedback).
    fn wl_surface(&self) -> Option<&wl_surface::WlSurface> {
        match self {
            TrackedRole::Panel { layer, .. } => Some(layer.wl_surface()),
            TrackedRole::Window { window, .. } => window.as_ref().map(WaylandSurface::wl_surface),
            TrackedRole::Popup { popup, .. } => popup.as_ref().map(WaylandSurface::wl_surface),
            TrackedRole::Lock { surface, .. } => surface.as_ref().map(SessionLockSurface::wl_surface),
        }
    }

    /// This surface as something an `xdg_popup` can be rooted under, or `None` if it cannot be one
    /// (§ 6.3's `parent`, docs/adr/0051 decision 1).
    ///
    /// A `window` or a `popup` that is not currently shown answers `None`, and that is the honest
    /// answer rather than a missing case: there is no surface to root under, so the popup asking is
    /// not created either.
    fn as_popup_parent(&self) -> Option<PopupParent> {
        match self {
            TrackedRole::Panel { layer, .. } => Some(PopupParent::Layer(layer.clone())),
            TrackedRole::Window { window, .. } => window.as_ref().map(|w| PopupParent::Xdg(w.xdg_surface().clone())),
            TrackedRole::Popup { popup, .. } => popup.as_ref().map(|p| PopupParent::Xdg(p.xdg_surface().clone())),
            // Never, and not for want of a mapped surface. `ext_session_lock_surface_v1` is neither
            // an `xdg_surface` nor a `zwlr_layer_surface_v1`, and those two are the whole of what
            // `xdg_surface.get_popup` and layer-shell's `get_popup` accept, so there is no request
            // that would root a popup here. That also happens to be the answer the protocol wants:
            // while the session is locked the compositor shows lock surfaces and nothing else
            // (docs/adr/0042), so a dropdown over a lock screen belongs in the lock screen's own
            // tree rather than in a second surface.
            TrackedRole::Lock { .. } => None,
        }
    }
}

/// Why [`App::show_popup`] declined to open a popup, remembered so the same line is not written
/// again on the next re-resolve while a different one still is (docs/adr/0049's amendment).
///
/// Each is a config-visible condition that can persist for the life of a generation, and a
/// re-resolve runs per capability push (ADR-0044 decision 2), so an unguarded `eprintln!` on any of
/// them writes several lines a second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopupRefusal {
    /// `grab = true` and no input event armed a serial this turn (docs/adr/0051 decision 3).
    Unarmed,
    /// `grab = true` and the compositor advertises no seat to take the grab on.
    Seatless,
    /// § 6.3's `parent` names no surface that is currently shown, which is the ordinary state of a
    /// popup parented to a `window` whose own `visible` is false.
    HiddenParent,
}

/// The two ways a popup gets rooted, which differ in *when* rather than in what (build-steps.md
/// Phase 22 item 2).
///
/// `Popup::from_surface` takes an `Option<&xdg_surface>` and roots the popup at creation, which
/// covers a `window` and a nested `popup`. A `panel` cannot go through that argument at all: its
/// surface has a layer-shell role and no `xdg_surface`, so layer-shell supplies its own
/// `zwlr_layer_surface_v1.get_popup` taking the raw `xdg_popup` back. That one has to be sent
/// *after* the popup object exists and *before* the initial commit, which is exactly why
/// [`Popup::new`] cannot be used here: it commits for you.
enum PopupParent {
    Layer(LayerSurface),
    Xdg(xdg_surface::XdgSurface),
}

struct TrackedSurface {
    role: TrackedRole,
    bound: Option<BoundSurface>,
    /// § 15's "surface_id", and since docs/adr/0038 the *instance* id
    /// (`layout::instance::SurfaceInstance::instance_id`): `"{id}@{output}"` for a panel and the
    /// bare declared `id` for a window, which has no output to qualify it with. This is the one id
    /// space Lua, the retained `Scene`, this `wl_surface`, and the PBA handshake all share --
    /// before this it was a fixed Rust-owned role's label, which overlapped none of them, which is
    /// why `layout::paint::paint_tree` had no caller.
    surface_id: String,
    map_state: MapState,
    /// Set once this surface's null buffer has been committed (PBA candidate mode only, § 15.2
    /// points 2-3). Irrelevant, always `false`, outside candidate mode.
    ///
    /// Never set for a `window` that is not shown, and that is why [`candidate_has_staged`] exists
    /// rather than the gate being a plain `all(null_buffered)`: staging happens on a configure, and
    /// a window with no `xdg_toplevel` will never get one.
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
    /// `xdg_wm_base`, plus the `zxdg_decoration_manager_v1` `XdgShell::bind` picks up alongside it
    /// (build-steps.md Phase 22 items 1 and 4). `None` on a compositor advertising no xdg-shell,
    /// which is legal if odd -- a `panel`-only config still works there, and a declared `window`
    /// says so once instead of taking the process down.
    xdg_shell: Option<XdgShell>,
    /// `ext_session_lock_manager_v1`, or the knowledge that the compositor advertises none
    /// (docs/adr/0042, build-steps.md Phase 23). Unlike `xdg_shell` this is not an `Option`: SCTK
    /// wraps the global in a `GlobalProxy`, so the absent case is carried inside and surfaces as a
    /// `GlobalError::MissingGlobal` from `lock` -- which is where it belongs, since docs/adr/0052
    /// decision 4 wants "this compositor cannot lock" reported as a *refusal of a lock command*
    /// rather than as a bind failure at startup that nobody asked for.
    ///
    /// Not in `registry_handlers![OutputState, SeatState]` either, and correctly so:
    /// `SessionLockState` is not a `RegistryHandler`. It binds once from the `GlobalList` in
    /// [`run`] and has no interest in later registry churn.
    session_lock_state: SessionLockState,
    /// The live `ext_session_lock_v1`, from the moment `lock` is sent until the lock ends by any of
    /// its three routes: an unlock the Supervisor ordered, a denial, or a teardown the compositor
    /// performed itself.
    ///
    /// `Some` with `is_locked()` still false is the in-flight window between the request and the
    /// compositor's answer, and that window is the whole reason `finished` is two different events
    /// (docs/adr/0042, build-steps.md Phase 23 item 2) -- see [`finished_outcome`], which reads
    /// exactly that flag. One field rather than a phase enum beside it, because SCTK already keeps
    /// the flag and a second copy here could only ever disagree with it.
    session_lock: Option<SessionLock>,
    egl: egl::EglState,
    gl: Option<glow::Context>,
    /// The one `ShapingHandle` for the whole process; `client` holds a clone of it, so
    /// content-sizing and painting share one worker thread and one `FontSystem`
    /// (docs/adr/0023 item 8, closed by docs/adr/0039 decision 3).
    shaping: ShapingHandle,
    text_painter: Option<TextPainter>,
    /// One image cache for the whole process, beside the one `TextPainter`, for the same reason:
    /// it is keyed by file path and pixel size, so a tray icon drawn on the bar and the same icon
    /// drawn in a popup are one upload, not one per surface (`CONTEXT.md`, **Image cache**). Not
    /// an `Option` unlike `text_painter`, which needs a live GL context to construct; this needs
    /// one only when it loads, and every load already goes through a `&mut Canvas`.
    image_cache: ImageCache,
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
    /// The seat's pointer, once it advertised one (build-steps.md Phase 21 item 1). Kept alive
    /// because dropping the proxy destroys the protocol object, and with it every
    /// `enter`/`press`/`release` this shell is interactive because of -- the same reasoning
    /// `BoundSurface`'s `#[allow(dead_code)]` fields already document.
    ///
    /// One, not one per seat: [`SeatHandler::new_capability`] takes whichever seat announced the
    /// capability into one slot, so this whole file is single-seat, and a second seat's pointer
    /// would need a second `armed` beside it rather than sharing this one.
    pointer: Option<wl_pointer::WlPointer>,
    /// The seat's keyboard, once it advertised one (build-steps.md Phase 21 item 2). Kept alive for
    /// the same reason `pointer` is, and single-seat for the same reason.
    ///
    /// This shell reads no keys off it. It is bound for its `enter`/`leave` alone, which is the only
    /// way a client learns which of its surfaces `keyboard_interactivity` (Phase 20) actually won
    /// focus for.
    keyboard: Option<wl_keyboard::WlKeyboard>,
    /// The instance id of the surface holding keyboard focus, if any of this process's surfaces
    /// does (docs/adr/0050's consequences).
    ///
    /// [`focus_is_still_armed`] reads it on every keystroke: a `secure_submit` field is armed only
    /// while the surface that declared it is the one this names.
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
    /// amendment). Monotonic and never reset: it is compared against, never read for a total.
    ///
    /// This is what makes the dismissal latch clearable. `input_serial` cannot do the job -- it is
    /// deliberately cleared at the end of each poll turn, so by the time a *later* turn asks "has
    /// the user asked again since the dismissal", there is nothing left to compare. Counting the
    /// same two events costs one `u64` and survives the disarm.
    ///
    /// Counting the *release* as well as the press is what makes the reopen work whatever order the
    /// compositor batches a dismissal in. The grab breaks at the press, so `popup_done` cannot
    /// arrive after the matching release; whether it lands before the press or between the two, one
    /// of them still increments this after [`App::latch_popup`] stamped it, and the end-of-turn
    /// `apply_popup_visibility` sees a moved counter.
    pointer_input_count: u64,
    /// The focused `secure_submit` field and the surface it lives on, set by the press that focused
    /// a `textfield` (docs/adr/0050 decision 4, [`focused_target`]) or by keyboard focus landing on
    /// a surface with a sole one ([`sole_secure_submit`]). `None` means no frame at all -- see
    /// [`submit_frame_for`]. Written only through [`App::focus_secure_submit`].
    focused_secure_submit: Option<FocusedField>,
    /// Accumulates the focused field's keystrokes until Enter completes them (build-steps.md
    /// Phase 15 item 2, Phase 23 item 3; ADR-0005/ADR-0009/ADR-0027) -- never surfaced to Lua.
    ///
    /// **Its lifetime belongs to `focused_secure_submit`, not to any transport event.** Every
    /// write to the field above goes through [`App::focus_secure_submit`], which zeroizes this
    /// on any change of destination, because bytes typed for one field must never be readdressed
    /// to the next one's capability -- see [`retarget_secure_submit`] for the leak that rule
    /// closes.
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
    // Optional, unlike layer-shell's: a compositor with no `xdg_wm_base` is legal, and a config
    // declaring only panels works fine there. Logged and carried, the same tolerance
    // `PresentationTimeState::bind` already applies to a protocol that may not
    // be advertised -- `create_surfaces` is what says which `window` went unbuilt, since only it
    // knows there was one.
    let xdg_shell = XdgShell::bind(&globals, &qh)
        .inspect_err(|err| log_bind_failure("<xdg-shell>", "xdg_wm_base::bind", err))
        .ok();
    let output_state = OutputState::new(&globals, &qh);
    let seat_state = SeatState::new(&globals, &qh);
    // Deliberately not `?` and deliberately not logged: `SessionLockState::new` cannot fail. It
    // stores a `GlobalProxy`, so a compositor advertising no `ext_session_lock_manager_v1` is
    // indistinguishable from one that does until something actually asks for a lock, which is the
    // only point at which anyone cares (docs/adr/0052 decision 4).
    let session_lock_state = SessionLockState::new(&globals, &qh);
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
        let SurfaceSpec::Panel(panel) = spec else {
            // Only a `panel` names a monitor (§ 6.2, § 6.3): the compositor places a toplevel and a
            // popup positions against its parent, so neither can miss one.
            continue;
        };
        if panel.topology.monitor != "All" && !outputs.iter().any(|output| output.name == panel.topology.monitor) {
            // `expand_instances` is pure and returns nothing for a miss; the log belongs here,
            // where the real output list is, so a config naming an unplugged monitor says so once
            // at startup rather than silently producing no surface.
            eprintln!(
                "[oblisk-renderer] surface {:?} targets monitor {:?}, which is not connected; no surface created for it",
                panel.topology.id, panel.topology.monitor
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

    app.create_surfaces(&qh, &specs, &instances);
    if app.is_pba_candidate {
        // The configure-driven check in `bind_and_clear` covers every surface that gets a
        // configure, and a generation whose every declared surface is a `window` with `visible =
        // false` gets none at all -- no `xdg_toplevel` exists to be configured (docs/adr/0049
        // decision 1). Without this call such a Candidate would never announce itself and would die
        // on `ready_timeout`. A no-op in every other case, since no panel has been configured yet
        // at this point and the gate refuses.
        app.maybe_send_ready_signal();
    }
    // From here on an output event owns the whole job: there is an evaluation to expand and
    // surfaces to reconcile against it (see `App::startup_complete`).
    app.startup_complete = true;

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
        // `Disconnected` is a separate answer from `Empty` here, and that is docs/adr/0059
        // decision 1. It used to be one answer: `while let Ok(frame)` treated a dead socket thread
        // (pump exited, see `crate::socket`) exactly like an idle one, so killing the Supervisor
        // left this process spinning its 15ms poll forever at 17.8% of a core, painting a shell
        // with no capability data behind it and no way to reach one.
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
        loop {
            let frame = match inbound_rx.try_recv() {
                Ok(frame) => frame,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                // `std::process::exit`, not `app.exit = true`. Breaking the loop returns from `run`
                // and drops `App`, and SCTK's `SessionLockInner::Drop` sends a bare
                // `ext_session_lock_v1.destroy`, which is the `invalid_destroy` protocol error once
                // `locked` has been sent -- the one error docs/adr/0052 was built to stay away
                // from. SCTK calls that choice failing secure and it is right, but the error is
                // avoidable: skipping the destructor closes the connection instead, which the
                // compositor treats as the same lock client death and logs as nothing.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    eprintln!("[oblisk-renderer] {}", supervisor_gone_report(app.session_lock.is_some()));
                    std::process::exit(EXIT_SUPERVISOR_GONE);
                }
            };
            match app.client.handle_frame(frame) {
                FrameOutcome::Handled => {}
                FrameOutcome::ActivateDraw(nonce) => draw_nonces.push(nonce),
                // Serviced here in the drain rather than collected the way a draw nonce is, and the
                // difference is what each one needs from the rest of this turn. A draw has to land
                // *after* the re-resolve below or it paints the pre-push layout, which is the whole
                // argument the comment above makes. A lock reads nothing a re-resolve produces:
                // whether this config declares a `lock` surface at all is a fact about the tracked
                // surface set (docs/adr/0052 decision 3), and no capability push can change it.
                // Deferring it would buy nothing and cost a poll turn on the one command whose
                // entire point is that the screen goes secure now.
                FrameOutcome::SetSessionLock(locked) => {
                    // A round trip before an *unlock*, and only before an unlock. SCTK sets the
                    // `locked` flag its `SessionLock::unlock` is gated on when
                    // `ext_session_lock_v1::locked` is **dispatched**, not when the compositor sends
                    // it, and this drain runs in a different turn from `dispatch_pending` above --
                    // the poll at the bottom of the previous turn may well have timed out with
                    // `locked` already on the wire. `unlock()` would then be a silent no-op and the
                    // `Drop` immediately after it would send the plain `destroy` that the protocol
                    // XML calls out by name: "it is a protocol error to make this request if the
                    // locked event was sent". That is `invalid_destroy`, which kills the connection
                    // with the session still locked -- the exact unrecoverable state docs/adr/0052
                    // exists to keep the user out of. `roundtrip` closes it by definition: the
                    // compositor's `wl_callback` cannot arrive before everything it sent earlier.
                    //
                    // The acquire path needs none of this and deliberately does not pay for it. Its
                    // inputs are the tracked surface set and `session_lock.is_some()`, both of which
                    // this thread owns outright, and an undispatched `locked` can only make
                    // `session_lock` already `Some`, which [`lock_command`] answers `Nothing`.
                    // Blocking the one command whose whole point is that the screen goes secure now
                    // on a compositor round trip would be a real cost for no fact gained.
                    //
                    // **Deliberately not `?`.** Propagating here would return from `run` between
                    // the correct password and `unlock_and_destroy`, killing the client with the
                    // session still locked -- and the compositor does not unlock when a lock client
                    // dies, so the user's only way back in would be a VT switch. That is the exact
                    // outcome this whole path exists to prevent, reached by the error handling
                    // rather than by the protocol. Every `DispatchError` this can raise means the
                    // connection is already broken, so the unlock attempt below may well send
                    // nothing; attempting it costs one failed flush and is strictly better than
                    // exiting without trying. What is *not* acceptable is skipping the attempt.
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
        // The disarm half of docs/adr/0049's amendment, and it has to be here rather than inside
        // the `if` above. `dispatch_pending` at the top of this turn armed `input_serial` if a
        // `BTN_LEFT` press or release arrived; `apply_resolved_surface_state` directly above is the
        // only thing that reads it, because it is the only thing that creates a popup. Clearing it
        // unconditionally is what makes the IDL's "a popup may only be opened in response to real
        // user input" fall out of the mechanism instead of being a rule bolted on: a notification
        // arriving over D-Bus marks the scene dirty and re-resolves on some later turn, finds
        // nothing armed, and a `grab = true` popup it tried to open is refused. Clearing inside the
        // `if` would leak a click's serial across every turn until the *next* re-resolve, which is
        // exactly the window that rule exists to close.
        app.input_serial = None;
        // Once a turn, so a focused `secure_submit` field whose surface this process tore down --
        // a lock screen the compositor `finished`, a `window` whose `visible` went false -- does not
        // sit there holding a half-typed password until some later keystroke happens to notice. The
        // load-bearing check is the one in `App::apply_secure_key`; this one is the residency
        // ceiling, and it is deliberately the narrower of the two.
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
    /// The evdev code the press carried, so the release has to be the *same* button and not merely
    /// a button (docs/adr/0050's second amendment).
    ///
    /// ponytail: one armed click, so chording drops both. `armed` is a single `Option`, and a
    /// second press overwrites the first, so pressing right then left on the same node and
    /// releasing either fires nothing: the release that arrives finds a different button armed, and
    /// the one after it finds nothing armed at all. It fails in the safe direction (a wrong handler
    /// never runs, a click is only lost) and it needs two buttons held at once, which nobody does
    /// to a shell. The upgrade is `armed` becoming keyed by button, an `ArrayVec` of three or a
    /// small map, with the same instance-id-and-rect comparison per entry; do it if a config ever
    /// wants a chord, or if a real mouse turns out to emit overlapping pairs on its own.
    button: u32,
}

/// The serial `xdg_popup.grab` needs, plus the surface the event carrying it was delivered to
/// (docs/adr/0049's amendment, docs/adr/0051 decision 1). Armed by [`PointerHandler::pointer_frame`]
/// and cleared by [`run`]'s poll loop at the end of the same turn.
///
/// Decision 2 of docs/adr/0049 claimed the re-resolve that creates a popup "is still running inside
/// input dispatch, so the engine has the serial of the event that caused it". It is not:
/// `re_resolve_if_dirty` runs in the poll loop, after `dispatch_pending` has returned, so by then
/// the dispatch callback's stack -- and any serial sitting on it -- is gone. Restructuring the loop
/// to resolve inside dispatch would put a full `Scene::apply`, arbitrary Lua, and Wayland object
/// creation inside a `Dispatch` callback, reentering the queue being dispatched from. So the serial
/// lives in a field for the length of one turn instead of on a stack, and the rule it protects is
/// unchanged: a re-resolve driven by anything other than input finds nothing here.
///
/// **Both a press and a release arm it, latest wins.** A click fires on the release (docs/adr/0050
/// decision 2), so the release's serial is the one a popup opened by `on_click` actually carries,
/// and it is the more recent of the two. `xdg_shell` asks only that the serial come from "a real
/// input event (button press, key press, touch down)" and leaves whether it was recent enough to the
/// compositor, which answers a refusal with an immediate `popup_done` -- a normal outcome
/// (docs/adr/0051 decision 3), not an error.
///
/// `instance_id` rather than the tracked index: `self.surfaces` is a `Vec` an output change removes
/// from (`destroy_surface_by_id`), and a monitor unplugged between the click and the poll turn's
/// re-resolve would leave an index naming a different surface. The id is stable and is what
/// `is_instance_of` compares against anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ArmedSerial {
    serial: u32,
    instance_id: String,
}

/// What one resolution of a `popup`'s `visible` does, given whether its `xdg_popup` currently
/// exists and whether docs/adr/0051 decision 2's latch is set (§ 5.1's `visible`, docs/adr/0049
/// decision 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopupAction {
    Create,
    Destroy,
    Nothing,
}

/// Decision 2's latch as a state machine, split out because it is the one part of the popup path
/// that is pure and the one part whose mistake is a livelock rather than a missing window.
///
/// `dismissed_at` is [`App::pointer_input_count`] as it stood when the compositor dismissed this
/// popup, and `pointer_input` is what that counter reads now. **A popup is latched only while no
/// pointer input has arrived since its dismissal**, which is docs/adr/0051's first amendment: the
/// `visible = false` edge decision 2 named is unobservable in the one case that matters, so a latch
/// keyed on it alone is permanent. The counter is the fact that separates a livelock from a person
/// reaching for the dropdown a second time.
///
/// The latch is *read* here and cleared by the caller on the same `visible = false` this returns
/// `Destroy` or `Nothing` for. That split is deliberate: the clear is a write and this function
/// makes no writes, and expressing "false clears it" as a returned action would need a fourth
/// variant that every caller would have to remember to pair with the other three.
///
/// The four interesting rows:
///
/// - `visible = true`, no object, dismissed with the counter unmoved: **nothing**. This is the whole
///   of decision 2. A compositor dismissal leaves the resolved tree still saying `true`, so without
///   the latch the next re-resolve creates a second popup for the same click-outside to dismiss,
///   forever -- and a config with no `on_dismiss` at all is not a config error.
/// - `visible = true`, no object, dismissed but the counter has moved: **create**. The user clicked
///   again, which is the amendment's whole point.
/// - `visible = false`, no object: **nothing**, and the caller still clears the latch. That is what
///   reopens the path: a config's own `on_dismiss` writing `visible = false` clears it immediately,
///   and a config without one clears it on the next deliberate close.
/// - `visible = true`, object already exists: **nothing**. Every re-resolve runs
///   `apply_resolved_state` for every surface (one dirty flag for the whole scene), so a popup that
///   is simply still open passes through here on every capability push.
fn popup_visibility_action(visible: bool, exists: bool, dismissed_at: Option<u64>, pointer_input: u64) -> PopupAction {
    let latched = dismissed_at == Some(pointer_input);
    match (visible, exists) {
        (true, false) if !latched => PopupAction::Create,
        (false, true) => PopupAction::Destroy,
        _ => PopupAction::Nothing,
    }
}

/// § 5.1's `visible` at the moment [`App::create_surfaces`] first builds one instance, given what
/// its resolved tree says (`None` when it has none) and which role was declared.
///
/// **The fallback is role-aware, and that is the whole of this function.** An absent tree means the
/// startup apply failed, and `Scene::apply` rolls its whole surface map back on error, so when one
/// instance has no tree none of them does. For a `panel`, whose Wayland object exists either way and
/// whose `visible` only maps and unmaps it (docs/adr/0038 decision 2), treating that as visible is
/// the "keep the shell up" fallback the rest of this file follows: the bar comes up, painting
/// nothing, and the next successful re-resolve fills it in.
///
/// The other three roles all answer `false`, and each for its own reason rather than by sharing a
/// fallback arm. For the two docs/adr/0049 decision 1 gives create-and-destroy semantics, an absent
/// tree would create an `xdg_toplevel` for every declared `window` and an `xdg_popup` for every
/// declared `popup`, so one failed apply opens the dev config's empty `settings` window, which
/// claims a tile and takes focus. An absent tree is not a declaration of `visible = true`.
///
/// A `lock` is the fourth answer and it is a different kind of `false`: § 6.4 gives a `lock` no
/// `visible` property at all, because the compositor owns when those surfaces exist
/// (docs/adr/0042), and [`App::create_surfaces`] accordingly hands this value to `create_panel`,
/// `create_window` and `create_popup` and *not* to `create_lock`. So the value is unread for this
/// role today, and `false` is still the only answer worth writing down: it is the one that stays
/// correct if a future caller does read it, since a `lock` that has not been granted is not up.
/// Spelling all four arms out is what stopped `Lock` being silently absorbed into a `matches!`
/// fallback the moment the role was added.
fn starting_visible(resolved: Option<bool>, roster: &SurfaceSpec) -> bool {
    resolved.unwrap_or(match roster {
        SurfaceSpec::Panel(_) => true,
        SurfaceSpec::Window(_) | SurfaceSpec::Popup(_) | SurfaceSpec::Lock(_) => false,
    })
}

/// One surface's [`SurfaceSpec`] re-derived from its resolved properties, and the § 6 role word for
/// the log line if it fails (docs/adr/0049's second amendment).
///
/// `roster` contributes only the role. Everything else comes from `properties`, which is a
/// `resolve_properties` result and so has already read each `Signal` exactly once for this pass
/// (ADR-0044 decision 1). The role cannot come from the properties instead: `kind` is what
/// `crate::socket`'s `surface_specs` matched on to build the roster in the first place, and a
/// resolved tree that disagreed with it would be a reconcile bug rather than something to re-decide
/// here.
///
/// One caller, [`App::create_surfaces`]. [`App::apply_resolved_state`] does the same three parses
/// inline because it dispatches on the *tracked* role rather than on a roster entry, and each arm
/// hands its result to a different applier -- there is no shared `SurfaceSpec` for it to build.
fn resolved_surface_spec(
    roster: &SurfaceSpec,
    properties: &HashMap<String, Value>,
) -> (&'static str, Result<SurfaceSpec, layout::node::LayoutError>) {
    match roster {
        SurfaceSpec::Panel(_) => ("panel", node::panel_spec(properties).map(SurfaceSpec::Panel)),
        SurfaceSpec::Window(_) => ("window", node::window_spec(properties).map(SurfaceSpec::Window)),
        SurfaceSpec::Popup(_) => ("popup", node::popup_spec(properties).map(SurfaceSpec::Popup)),
        SurfaceSpec::Lock(_) => ("lock", node::lock_spec(properties).map(SurfaceSpec::Lock)),
    }
}

/// docs/adr/0052 decision 3's refusal, as the sentence the user reads. A config that declares no
/// `lock` node cannot be locked, and the reasoning is worth stating where it is enforced: acquiring
/// the lock anyway paints nothing, the protocol guarantees the compositor will not unlock on client
/// death (docs/adr/0042), and the only way out is a VT switch and killing the shell. Locking a user
/// out of their own session over a config omission is not fail-secure, it is a denial of service
/// spelled the same way. Fail-secure is about a lock that was taken; this is one that never was, and
/// nothing was protected by it a moment earlier.
const NO_LOCK_DECLARED: &str =
    "this config declares no `lock` surface (§ 6.4), so locking the session would leave a black screen with no password field and no way back \
     in short of a VT switch; the lock was refused (docs/adr/0052 decision 3)";

/// The `(capability, action)` pair that reaches PAM, and the only one that can ever end a session
/// lock. `supervisor/src/main.rs` routes `SecureSubmit { capability: "lock", action:
/// "authenticate" }` to the PAM worker and answers a `PamOutcome::Success` with the one
/// `SetSessionLock { locked: false }` this process will ever see; every other pair lands in some
/// other capability's dispatch and can no more unlock the session than a `print` could.
const UNLOCK_TARGET: (&str, &str) = ("lock", "authenticate");

/// The second half of docs/adr/0052 decision 3's refusal, and the one the guard was missing.
///
/// `node::lock_spec` requires an `id` and nothing else -- `child` is optional -- so `lock { id =
/// "x" }` is a legal declaration that resolves to a surface with no password field, an empty input
/// region and a transparent buffer. Counting tracked `lock` instances therefore said "a lock screen
/// exists" for exactly the black screen the decision refuses the lock to avoid, reached *through*
/// the guard rather than around it. The condition that actually matters is not whether a `lock`
/// node was written but whether the tree under it holds a [`UNLOCK_TARGET`] field, because that is
/// the only thing in a config that can produce the `SecureSubmit` the Supervisor answers with an
/// unlock.
///
/// A separate sentence from [`NO_LOCK_DECLARED`] because they are separate edits: one config is
/// missing a `lock` node, the other is missing a `textfield` inside the one it has.
const LOCK_CANNOT_AUTHENTICATE: &str =
    "this config's `lock` surface (§ 6.4) does not hold exactly one `textfield` with `secure_submit = { capability = \"lock\", action = \"authenticate\" }` \
     and nothing else, so the compositor handing it keyboard focus would arm no field, nothing on it could ever authenticate, and the only way back in \
     would be a VT switch; the lock was refused (docs/adr/0052 decision 3)";

/// A `SetSessionLock { locked: false }` that reached a lock object the compositor never answered
/// with `locked`. See [`App::release_session_lock`]: nothing was released, because there was
/// nothing up to release.
const LOCK_NEVER_GRANTED: &str =
    "the session lock was given up before the compositor ever granted it (no `ext_session_lock_v1::locked` arrived), so nothing was unlocked";

/// The other half of `finished`: the compositor answered the `lock` request with an immediate
/// refusal instead of `locked`. Almost always another lock client already holds the session lock,
/// which the protocol names first among its reasons, but it is compositor policy and not something
/// this side of the wire can narrow down further -- so the message says what is known and does not
/// guess.
const LOCK_DENIED: &str =
    "the compositor denied the session lock; another lock client most likely holds it already (`ext_session_lock_v1::finished` arrived in place \
     of `locked`)";

/// What `oblisk.rescue` says when the compositor tore down a lock that really was up. Not a
/// failure of anything this process did, and the message says so: docs/adr/0052 decision 4 routes
/// it here rather than to `oblisk.lock`'s `error` because there is no lock screen left on the glass
/// to read a message on -- the ordinary scene is what came back.
const LOCK_TORN_DOWN: &str =
    "the compositor ended the session lock through its own mechanism; the session is unlocked and the lock screen is gone \
     (`ext_session_lock_v1::finished` after `locked`)";

/// The exit code this process uses when the Supervisor's control socket is gone (docs/adr/0059
/// decision 1). Nobody is left to read it -- the process that classifies Renderer exit codes is the
/// one that just died -- so this is for a journal and a `$status`, not for a handshake. Distinct
/// from `0` because this is not a clean exit, and distinct from `1` because it is not a failure of
/// anything this process was asked to do.
const EXIT_SUPERVISOR_GONE: i32 = 70;

/// What this process says on its way out when the Supervisor's control socket is gone, split on
/// whether it holds `ext_session_lock_v1` at that moment (docs/adr/0059 decisions 1 and 2).
///
/// Pure and split out because the locked half is the one message here that can mislead into an
/// unrecoverable state, and docs/adr/0058 decision 4 already caught the neighbouring version of
/// that mistake: a refusal ending "the lock screen that is on screen still stands" is true of a
/// vetoed reload and false of a process that is exiting.
fn supervisor_gone_report(holds_session_lock: bool) -> &'static str {
    if holds_session_lock {
        "the Supervisor's control socket is gone while this Renderer holds the session lock. PAM runs in the Supervisor (docs/adr/0028), so this \
         lock screen can no longer authenticate anyone, and exiting without unlocking is what keeps a `kill` from being a way past a lock screen. \
         The session stays locked behind whatever the compositor puts up for a lock client that died, and the way back in is a VT switch \
         (docs/adr/0059 decision 2)"
    } else {
        "the Supervisor's control socket is gone, so this Renderer has no capability data, no `process.run` and no PAM left to serve. Exiting \
         rather than painting a shell that still takes clicks and answers none of them (docs/adr/0059 decision 1)"
    }
}

/// What one `SetSessionLock` asks this process to do, decided before any Wayland object is touched
/// (docs/adr/0042, docs/adr/0052 decisions 3 and 4).
///
/// Pure and separate because the two interesting answers are refusals, and a refusal that only
/// exists inside a `&mut self` method that also talks to the compositor is a refusal nothing can
/// test. See [`lock_command`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockCommand {
    /// Call `SessionLockState::lock` and create the surfaces.
    Acquire,
    /// Call `SessionLock::unlock`, then tear the surfaces down. **The only value that unlocks.**
    Release,
    /// Touch no protocol object at all; report this as `LockOutcome::Refused` and set `rescue`.
    Refuse(&'static str),
    /// The command asks for the state the lock is already in.
    Nothing,
}

/// One `SetSessionLock`, resolved against what this process is already holding.
///
/// `locked = true` has four answers and only one of them is "take the lock". Two of the other three
/// are refusals, and docs/adr/0052 decision 3 is both: the lock must be refused *here*, before
/// `SessionLockState::lock` is called, because a lock that was granted and then found unusable is
/// exactly the black screen the decision exists to prevent, and the protocol guarantees the
/// compositor will not unlock when the client dies.
///
/// The two refusals ask two different questions, and both have to be asked. `declares_lock` is a
/// question about the tracked surface set, which is the only place "is there a `lock` instance"
/// lives. `can_authenticate` is a question about that instance's *resolved tree*, and it is the one
/// the first check cannot stand in for: `lock { id = "x" }` declares an instance and resolves to
/// nothing typable (see [`LOCK_CANNOT_AUTHENTICATE`]). Refusing on the first and granting on the
/// second would be decision 3 enforced against the config that forgot the node and waived for the
/// config that forgot its contents, which land the user in the same place.
///
/// The compositor-cannot-lock case is deliberately **not** an input. `SessionLockState` keeps its
/// `ext_session_lock_manager_v1` in a `GlobalProxy` and `lock` answers `GlobalError::MissingGlobal`
/// when there is none, so asking a second time here would mean reading the registry directly and
/// keeping two answers to one question in step. The caller maps that `Err` to a `Refuse` with the
/// error's own words.
///
/// `locked = false` against nothing held is `Nothing` rather than `Release`, and that is not
/// defensive tidying: `unlock_and_destroy` on a lock that never got `locked` is the protocol's own
/// `invalid_unlock` error, and this is the guard that makes the unlock path unable to send one.
fn lock_command(locked: bool, declares_lock: bool, can_authenticate: bool, lock_held: bool) -> LockCommand {
    match (locked, lock_held) {
        (false, true) => LockCommand::Release,
        (false, false) | (true, true) => LockCommand::Nothing,
        (true, false) if !declares_lock => LockCommand::Refuse(NO_LOCK_DECLARED),
        (true, false) if !can_authenticate => LockCommand::Refuse(LOCK_CANNOT_AUTHENTICATE),
        (true, false) => LockCommand::Acquire,
    }
}

/// What one ordered release actually did, given whether `ext_session_lock_v1::locked` had been
/// dispatched on the lock object being given up (docs/adr/0052 decision 4).
///
/// `Unlocked` is a state transition and the Supervisor's `lock::apply` moves its `active` flag on
/// it, so reporting one for a lock that was never granted tells the Supervisor the session went
/// from locked to unlocked when it was never locked at all. SCTK's `SessionLock::unlock` is a no-op
/// below `is_locked()`, so on that branch literally nothing was sent and there is nothing to
/// announce as having cleared -- [`LOCK_NEVER_GRANTED`] says so instead.
fn release_outcome(was_locked: bool) -> LockOutcome {
    if was_locked { LockOutcome::Unlocked } else { LockOutcome::Refused(LOCK_NEVER_GRANTED.to_string()) }
}

/// Which of `ext_session_lock_v1::finished`'s **two** events this one is (docs/adr/0042,
/// build-steps.md Phase 23 item 2), decided by the one fact that separates them: whether `locked`
/// was ever sent on this lock object.
///
/// The protocol puts both on one event and describes each separately. "The finished event should be
/// sent immediately on creation of this object if the compositor decides that the locked event will
/// not be sent" is a *denial*, typically because another lock client already holds the lock, and
/// nothing was ever protected by it. "If the locked event is sent on creation of this object the
/// finished event may still be sent at some later time" is a lock that was really up and that the
/// compositor then ended through its own secure mechanism, leaving the session unlocked without
/// anyone here asking for it.
///
/// They must not collapse into one report. The Supervisor routes them differently (`lock::apply`),
/// and a denial is a failure the user has to see while a teardown is a state change the user
/// already lived through. Build-steps.md Phase 23 item 2 says neither may be swallowed, and this is
/// where the two are told apart.
///
/// `was_locked` is SCTK's own flag: its `Dispatch2` for `ext_session_lock_v1` sets it on `Locked`
/// and never clears it, including not on `Finished`, so it answers exactly this question and no
/// bookkeeping of ours can drift from it.
fn finished_outcome(was_locked: bool) -> LockOutcome {
    if was_locked { LockOutcome::Finished } else { LockOutcome::Refused(LOCK_DENIED.to_string()) }
}

/// Which tracked surface a popup roots under (docs/adr/0051 decision 1), as an index into the same
/// iterator's order.
///
/// § 6.3's `parent` names a declared `id`, and a declared `id` is not one surface: `monitor = "All"`
/// expands a `panel` per output (docs/adr/0038 decision 3), so `parent = "bar"` on a two-monitor
/// session names two layer surfaces and `get_popup` takes exactly one. The tie-break is the click
/// that armed the grab: a dropdown belongs to the monitor it was opened on, and that event already
/// names a surface, so this costs a field on [`ArmedSerial`] rather than a second mechanism.
///
/// ponytail: with nothing armed -- a `grab = false` popup opened by a D-Bus notification -- this
/// falls back to the *first* instance of the named parent, which on a multi-monitor session is
/// whichever output `expand_instances` listed first. There is no better answer available: nothing
/// in § 6.3 lets such a popup say which monitor it means. The upgrade path is a `monitor` property
/// on `popup`, at which point this takes a third argument and the fallback becomes a real choice.
fn parent_instance_index<'a>(instance_ids: impl Iterator<Item = &'a str>, parent: &str, armed: Option<&str>) -> Option<usize> {
    let mut first = None;
    for (index, instance_id) in instance_ids.enumerate() {
        if !is_instance_of(instance_id, parent) {
            continue;
        }
        if armed == Some(instance_id) {
            return Some(index);
        }
        first.get_or_insert(index);
    }
    first
}

/// The size one `xdg_popup` configure asks for, as a buffer size.
///
/// A configure's `width`/`height` are the compositor's answer and are taken as given, the same way
/// a `Some` axis of an `xdg_toplevel` configure is: the compositor may have slid, flipped or
/// resized the popup to keep it on screen (§ 6.3's `constraint_adjustment`), and the size it lands
/// on is the one that has to be painted.
///
/// A non-positive axis falls back to the size the positioner asked for. That is a guard against
/// `smithay_client_toolkit`, not against a compositor: `PopupInner` seeds its pending dimensions at
/// `-1` and reports whatever they hold when the wrapping `xdg_surface.configure` arrives, so a
/// configure that reached the `xdg_surface` without an `xdg_popup.configure` before it would hand
/// this `-1`. xdg-shell requires that ordering, but a `-1` reaching `WlEglSurface::new` is a
/// crash-shaped failure and the spec's own requested size is right there.
///
/// At least 1 on both axes, for [`toplevel_size_for`]'s reason: a `wl_egl_window` of 0 is invalid.
fn popup_size_for(configured: (i32, i32), spec: &PopupSpec) -> (u32, u32) {
    let axis = |configured: i32, requested: f32| -> u32 {
        if configured > 0 {
            return configured as u32;
        }
        (requested.max(1.0)) as u32
    };
    (axis(configured.0, spec.width), axis(configured.1, spec.height))
}

/// § 6.3's `anchor` as `xdg_positioner`'s own enum. `Center` is § 6.3's name for the protocol's
/// `none`, which is not a fudge: with no edge specified the XML puts the anchor point "in the center
/// of the anchor rectangle".
fn positioner_anchor(anchor: PopupAnchor) -> xdg_positioner::Anchor {
    match anchor {
        PopupAnchor::Center => xdg_positioner::Anchor::None,
        PopupAnchor::Top => xdg_positioner::Anchor::Top,
        PopupAnchor::Bottom => xdg_positioner::Anchor::Bottom,
        PopupAnchor::Left => xdg_positioner::Anchor::Left,
        PopupAnchor::Right => xdg_positioner::Anchor::Right,
        PopupAnchor::TopLeft => xdg_positioner::Anchor::TopLeft,
        PopupAnchor::TopRight => xdg_positioner::Anchor::TopRight,
        PopupAnchor::BottomLeft => xdg_positioner::Anchor::BottomLeft,
        PopupAnchor::BottomRight => xdg_positioner::Anchor::BottomRight,
    }
}

/// § 6.3's `gravity`, which shares `anchor`'s value set and gets a second protocol enum with
/// identical members. `none` again for `Center`, and again the XML says why: a gravity of `none`
/// centers the surface "over the anchor point on any axis that had no gravity specified".
fn positioner_gravity(gravity: PopupAnchor) -> xdg_positioner::Gravity {
    match gravity {
        PopupAnchor::Center => xdg_positioner::Gravity::None,
        PopupAnchor::Top => xdg_positioner::Gravity::Top,
        PopupAnchor::Bottom => xdg_positioner::Gravity::Bottom,
        PopupAnchor::Left => xdg_positioner::Gravity::Left,
        PopupAnchor::Right => xdg_positioner::Gravity::Right,
        PopupAnchor::TopLeft => xdg_positioner::Gravity::TopLeft,
        PopupAnchor::TopRight => xdg_positioner::Gravity::TopRight,
        PopupAnchor::BottomLeft => xdg_positioner::Gravity::BottomLeft,
        PopupAnchor::BottomRight => xdg_positioner::Gravity::BottomRight,
    }
}

/// § 6.3's `constraint_adjustment` as the protocol's bitmask. Six independent booleans on one side
/// and six independent bits on the other, which is why [`ConstraintAdjustment`] is six booleans
/// rather than the array a config writes: the request takes a mask and the compositor fixes the
/// precedence, so the array's order was never carrying anything.
fn positioner_constraint(adjustment: ConstraintAdjustment) -> xdg_positioner::ConstraintAdjustment {
    let mut bits = xdg_positioner::ConstraintAdjustment::None;
    bits.set(xdg_positioner::ConstraintAdjustment::SlideX, adjustment.slide_x);
    bits.set(xdg_positioner::ConstraintAdjustment::SlideY, adjustment.slide_y);
    bits.set(xdg_positioner::ConstraintAdjustment::FlipX, adjustment.flip_x);
    bits.set(xdg_positioner::ConstraintAdjustment::FlipY, adjustment.flip_y);
    bits.set(xdg_positioner::ConstraintAdjustment::ResizeX, adjustment.resize_x);
    bits.set(xdg_positioner::ConstraintAdjustment::ResizeY, adjustment.resize_y);
    bits
}

/// Sends one [`PopupSpec`]'s whole § 6.3 positioner state, in one place so [`App::show_popup`] reads
/// as the protocol order it is (positioner, surface, popup, root, grab, commit) rather than as six
/// requests inline.
///
/// Every field is sent, including the ones whose value equals the protocol default. The positioner
/// is built fresh per open and destroyed with the call, so there is no live object for a diff to
/// spare and nothing carried over from a previous popup -- "send what changed" has no meaning here,
/// unlike on a `panel`'s live layer surface.
///
/// Rounded rather than truncated on the way to `i32`: these are logical pixels a `button`'s resolved
/// rect handed the config through `on_click` (docs/adr/0050 decision 3), so a rect at `x = 996.6`
/// belongs one pixel right of `996`, not on it.
///
/// The two sizes are clamped to at least 1. `node::parse_popup_extent` and `node::parse_anchor_rect`
/// already refuse a zero, so this only catches a positive value that rounds to zero, which
/// `set_size` and the positioner's own completeness rule both reject.
fn configure_positioner(positioner: &XdgPositioner, spec: &PopupSpec) {
    let round = |n: f32| n.round() as i32;
    positioner.set_size(round(spec.width).max(1), round(spec.height).max(1));
    positioner.set_anchor_rect(
        round(spec.anchor_rect.x),
        round(spec.anchor_rect.y),
        round(spec.anchor_rect.width).max(1),
        round(spec.anchor_rect.height).max(1),
    );
    positioner.set_anchor(positioner_anchor(spec.anchor));
    positioner.set_gravity(positioner_gravity(spec.gravity));
    positioner.set_constraint_adjustment(positioner_constraint(spec.constraint_adjustment));
    positioner.set_offset(round(spec.offset.x), round(spec.offset.y));
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

/// Both answers one press wants out of decision 1's single traversal: the `button` that would fire,
/// and the destination the innermost `textfield` addresses the next secret to.
///
/// One struct rather than two lookups because they come from one [`layout::hit::hit_path`] call.
/// Walking twice would be two answers to one event, and a re-resolve landing between the walks
/// could make them disagree about a tree that no longer exists.
struct PointerHit {
    button: Option<(LogicalRect, Function)>,
    /// `Err` is a malformed `secure_submit` on the innermost `textfield` -- see [`focused_target`].
    focus: Result<Option<node::SecureSubmitTarget>, node::LayoutError>,
}

/// The `secure_submit` destination the innermost `textfield` in a hit path names (docs/adr/0050
/// decision 4, § 5.2 item 8).
///
/// `Ok(None)` deliberately collapses two cases nothing downstream can tell apart: the path holds no
/// `textfield` at all, and the innermost one declared no `secure_submit`. Both mean the next
/// completed submit has nowhere to go, and § 5.2 item 8 makes the property optional precisely so a
/// masked field can exist without one.
///
/// ponytail: focus is therefore stored as its destination rather than as a node identity, so a
/// focused field with no destination is indistinguishable from no focus at all. Nothing reads focus
/// for any other purpose yet -- there is no caret, no selection, and no `on_key` (docs/adr/0050's
/// consequences). Upgrade path: carry the field's `NodeId` alongside the target once something has
/// to paint or address the *field* rather than its submit.
fn focused_target(path: &[&layout::ResolvedNode]) -> Result<Option<node::SecureSubmitTarget>, node::LayoutError> {
    let Some(field) = path.iter().rev().find(|node| node.kind == "textfield") else {
        return Ok(None);
    };
    node::parse_secure_submit(&field.properties)
}

/// The frame a completed `wp-text-input-v3` submit produces, or `None` when no focused `textfield`
/// named a destination for it (docs/adr/0050 decision 4).
///
/// `None` is the whole point of this function. Before it, the submit was addressed to
/// `"unknown"/"unknown"`, which no Supervisor capability routes -- a password put on the wire for
/// nobody, when ADR-0005's entire premise is that this buffer travels to exactly one named
/// destination. Sending nothing is the only safe answer to "whose password is this?".
///
/// The buffer is zeroized on both branches, and on the branch that sends nothing it is the only
/// thing that happens: a dropped submit must not leave the accumulated secret sitting in `App`
/// waiting for the next field to pick it up.
fn submit_frame_for(
    generation_id: u32,
    target: Option<&node::SecureSubmitTarget>,
    buffer: &mut shared::SecureBuffer,
) -> Option<RendererFrame> {
    // An empty buffer is not a password, and sending one is not free. The Supervisor routes it
    // straight into PAM, which spends one of the user's counted attempts and one `pam_unix` failure
    // delay answering a keystroke that said nothing -- on the lock screen, where attempts are the
    // scarce resource. Enter on an empty field does nothing, the way it does in every other password
    // prompt. Checked before the destination, because it is true whatever the destination was.
    let Some(target) = target.filter(|_| !buffer.is_empty()) else {
        buffer.zeroize();
        return None;
    };
    Some(secure_submit_frame(generation_id, &target.capability, &target.action, buffer))
}

/// A focused `secure_submit` field, together with the surface whose tree declared it.
///
/// **The surface id is the half the fourth review's defects 2 and 3 were both missing.** Focus used
/// to be nothing but a destination, so nothing could tell "the field on the surface that currently
/// has the keyboard" from "the field on a surface this process destroyed ten seconds ago". Two holes
/// fell out of that, and they are the same hole: [`KeyboardHandler::enter`]'s early returns moved
/// `keyboard_focus` on and left the old field armed, and every destruction path
/// ([`App::teardown_lock_surfaces`], [`App::destroy_surface_by_id`], [`App::hide_window`]) tore down
/// the `wl_surface` that owned the field while the target and the half-typed secret stayed live.
/// The traced consequence of the second is the login password: type one on the lock screen, let the
/// compositor send `finished`, and the plaintext sits in `App::secure_buffer` still addressed to
/// `("lock", "authenticate")` with later bar keystrokes appending to it. `wl_keyboard.leave` is what
/// used to be relied on to notice, and the protocol does not require a compositor to send one for a
/// surface the client itself destroyed.
///
/// So the field is bound to its surface and [`focus_is_still_armed`] is the one question every
/// keystroke asks, rather than a clearing call bolted onto each of the five or six sites that can
/// take a surface away -- which is exactly the shape that left the hole to begin with.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FocusedField {
    /// The `"{id}@{output}"` instance id of the surface the field was declared on.
    surface_id: String,
    target: node::SecureSubmitTarget,
}

/// The one place `App::focused_secure_submit` is ever written, and the reason it is one place.
///
/// **A `shared::SecureBuffer`'s lifetime belongs to the field the bytes were typed into, not to the
/// transport that carried them.** While `zwp_text_input_v3` was the only writer that looked like a
/// property of its `leave` event, and the scrub lived there. It never was: three other sites cleared
/// or *reassigned* the focus target and left the plaintext behind -- `KeyboardHandler::leave`,
/// `SeatHandler::remove_capability`'s keyboard arm, and the press in `PointerHandler::pointer_frame`
/// that retargets outright. The traced consequence is a credential leak with no lock involved: type
/// a login password into the lock screen's field and press nothing, let the compositor tear the lock
/// surfaces down, take keyboard focus on a bar whose sole `secure_submit` is `("network",
/// "connect")`, type a Wi-Fi PSK, press Enter, and [`submit_frame_for`] addresses
/// `<login password><psk>` to the network capability. That is precisely the routing docs/adr/0005
/// exists to make impossible.
///
/// So the rule is enforced on the *transition* rather than at each site that performs one: any
/// change of destination, including one field to another directly, scrubs. A fifth caller added
/// later inherits it by construction instead of having to remember it.
///
/// Re-arming the *same* destination deliberately does not scrub. A press decides focus
/// unconditionally (docs/adr/0050 decision 4), so clicking twice in the field being typed into
/// arrives here with the target unchanged, and wiping there would delete half an entry.
///
/// A free function taking both halves rather than a `&mut self` method, so the property is testable
/// without a live Wayland connection -- the same reason [`secure_submit_frame`] is one.
fn retarget_secure_submit(focused: &mut Option<FocusedField>, buffer: &mut shared::SecureBuffer, next: Option<FocusedField>) {
    if *focused != next {
        buffer.zeroize();
    }
    *focused = next;
}

/// Whether this `secure_submit` destination is the one that can end a session lock.
///
/// Named rather than compared inline because two callers want it for opposite reasons:
/// [`lock_command`] refuses a lock screen that has no such field, and nothing else in the file may
/// quietly grow a second opinion about which pair unlocks. See [`UNLOCK_TARGET`].
fn unlocks_the_session(target: &node::SecureSubmitTarget) -> bool {
    (target.capability.as_str(), target.action.as_str()) == UNLOCK_TARGET
}

/// Every `secure_submit` destination a resolved tree declares, in document order.
///
/// Whole-tree, unlike [`focused_target`], and the difference is what each answer is for: a press
/// names one node, so it walks a hit path and takes the innermost. These two callers have no node
/// to start from -- one is asking what a surface as a whole offers before a single event has
/// arrived on it.
///
/// A malformed `secure_submit` contributes nothing rather than an error. The config bug is already
/// reported where it can name the surface it is on (the press path logs it), and neither caller
/// here has a use for a second copy: a field whose destination cannot be parsed is a field nothing
/// can address a secret to, which is exactly what "not a candidate" means.
fn secure_submit_targets(tree: &layout::ResolvedNode) -> Vec<node::SecureSubmitTarget> {
    let mut found = Vec::new();
    let mut stack = vec![tree];
    while let Some(node) = stack.pop() {
        if node.kind == "textfield"
            && let Ok(Some(target)) = node::parse_secure_submit(&node.properties)
        {
            found.push(target);
        }
        stack.extend(node.children.iter().rev());
    }
    found
}

/// The destination a surface takes on *keyboard* focus, when its tree declares exactly one
/// (build-steps.md Phase 23 item 3).
///
/// **Why keyboard focus focuses a field at all.** Until this, `focused_secure_submit` was set only
/// by a pointer press, which made a lock screen require a mouse click before a keystroke could
/// reach `shared::SecureBuffer` -- on the one surface whose entire purpose is to accept a password
/// with the rest of the session hidden behind it. A lock surface has to be typable the moment the
/// compositor hands it keyboard focus, and the compositor saying "this surface has the keyboard" is
/// the only signal available before the user has touched anything.
///
/// **Exactly one, deliberately.** With two `secure_submit` fields on one surface there is no
/// non-arbitrary answer to "whose password is this?", and guessing is the thing
/// [`submit_frame_for`] already refuses to do (docs/adr/0050 decision 4). Zero is the same answer
/// for the same reason. Both cases leave focus alone for a press to decide, which is what a
/// multi-field surface has always needed anyway; the rule buys the single-field case, which is
/// every lock screen and every password prompt.
fn sole_secure_submit(tree: &layout::ResolvedNode) -> Option<node::SecureSubmitTarget> {
    let mut targets = secure_submit_targets(tree);
    (targets.len() == 1).then(|| targets.remove(0))
}

/// What `focused_secure_submit` becomes when keyboard focus arrives on `surface_id`, given that
/// surface's resolved tree and whatever is focused now.
///
/// **A total function, which is defect 2.** [`KeyboardHandler::enter`] used to spell its two
/// "nothing to arm" cases -- an untracked surface, and a tracked one declaring no sole
/// `secure_submit` -- as early returns that moved `keyboard_focus` on and left
/// `focused_secure_submit` exactly as it was. `apply_secure_key` gates on the focus alone, so
/// keystrokes arriving on a surface with no password field went on accumulating into the *previous*
/// surface's field and could still be submitted to that field's capability. Every case answers here,
/// and `enter` pushes the answer through [`App::focus_secure_submit`] whatever it is, so "nothing to
/// arm" is the scrub it always should have been.
///
/// **What survives an `enter` is a field on the surface that is entering, and only that.** A press
/// on a surface declaring several `secure_submit` fields picks one that [`sole_secure_submit`]
/// deliberately refuses to pick, and the compositor's `enter` for that same surface commonly follows
/// the press that caused it -- so discarding the press's choice would make a multi-field surface
/// untypable by clicking. Requiring the tree to still declare that destination is what keeps a
/// reload from leaving the choice pointing at a field the config has since deleted.
fn focus_on_enter(surface_id: Option<&str>, tree: Option<&layout::ResolvedNode>, current: Option<&FocusedField>) -> Option<FocusedField> {
    let (id, tree) = (surface_id?, tree?);
    if let Some(current) = current.filter(|field| field.surface_id == id && secure_submit_targets(tree).contains(&field.target)) {
        return Some(current.clone());
    }
    Some(FocusedField { surface_id: id.to_string(), target: sole_secure_submit(tree)? })
}

/// Whether a focused field is still armed: its own surface both holds the keyboard and still exists
/// as a live `wl_surface` in this process.
///
/// **Both clauses, and neither is redundant.** The keyboard clause is defect 2: a pointer press arms
/// focus on whatever surface it landed on, so without it a field on a `keyboard_interactivity =
/// none` panel stays armed while another surface is the one actually receiving keys. The liveness
/// clause is defect 3: a `wl_surface` this process destroyed may never produce a `leave` at all, so
/// the field on a torn-down lock screen would otherwise stay armed with a login password in it.
///
/// Asked at the point of use rather than enforced at each site that can break it. There are five or
/// six such sites today and the next one added would inherit nothing; this way a field is armed only
/// while both facts are true, by construction.
fn focus_is_still_armed(field: &FocusedField, keyboard_focus: Option<&str>, its_surface_is_live: bool) -> bool {
    keyboard_focus == Some(field.surface_id.as_str()) && its_surface_is_live
}

/// Whether a `lock` surface's resolved tree can actually be authenticated out of -- the predicate
/// [`lock_command`]'s `can_authenticate` reads, and it is deliberately built out of
/// [`sole_secure_submit`] rather than out of [`secure_submit_targets`].
///
/// **The guard that grants the lock and the rule that arms the keyboard must be one predicate.**
/// They were two: admission asked whether *any* field in the tree unlocks, focus armed only a
/// *sole* field. A lock screen with two `secure_submit` fields therefore passed the guard, took the
/// lock -- which the compositor will not release when the client dies -- and then armed nothing when
/// the compositor handed the surface keyboard focus. On a keyboard-only machine, or with the second
/// field buried in a subtree the user cannot see to click, the only way back into the session was a
/// VT switch. Two predicates that agree in the common case are not a guard; this is one function
/// with two callers.
///
/// Sole-and-unlocking is the right rule of the two, and not merely the stricter one. `any` is not
/// implementable as a focus rule at all: with two destinations there is no non-arbitrary answer to
/// "whose password is this?", which is the guess [`submit_frame_for`] already refuses to make
/// (docs/adr/0050 decision 4). Weakening focus to match `any` would mean picking one field by
/// document order and sending a lock password to whatever capability that field happened to name.
/// So the focus rule stays, and admission is what moves to meet it.
pub(crate) fn tree_can_authenticate(tree: &layout::ResolvedNode) -> bool {
    sole_secure_submit(tree).as_ref().is_some_and(unlocks_the_session)
}

/// What one key event does to a focused `secure_submit` field.
///
/// Borrowed rather than owned so the decision costs no allocation: the `String` only ever exists
/// because SCTK already built one on the `KeyEvent`.
#[derive(Debug, PartialEq, Eq)]
enum SecureKeyAction<'a> {
    Append(&'a str),
    Backspace,
    /// Escape: throw the whole entry away and stay in the field.
    Clear,
    Submit,
    Ignore,
}

/// One `wl_keyboard` key, as an edit to a focused `secure_submit` buffer (build-steps.md Phase 23
/// item 3).
///
/// **Why the keyboard and not `zwp_text_input_v3`.** text-input-v3 only ever produces a
/// `commit_string` when the compositor has an input method bound to the seat, so on an ordinary
/// session with no IME running -- the normal case, and the case on the machine this was found on --
/// not one byte reached `shared::SecureBuffer`, no `SecureSubmit` was ever built, and a lock that
/// had been granted could not be authenticated out of at all. It is also the security-correct
/// transport independently of that: a password must not be routed through an input method, which is
/// why swaylock and hyprlock read xkb directly and do not bind text-input either.
///
/// **So the `zwp_text_input_v3` binding is gone entirely, and this is what replaced it.** Keeping
/// it would have left two independent writers on one `shared::SecureBuffer` -- this one and
/// `handle_text_input_event`'s `done` arm -- with an IME able to land a character through both, and
/// a live `ContentPurpose::Password` session sitting open beside the keyboard reader for a protocol
/// docs/adr/0027's amendment says must never see a password in the first place. Nothing else
/// consumed it: `on_change`/`on_submit` were never wired to anything, so the bridge served only the
/// one field kind that must not use it. Deleted rather than left dormant, since a dormant enabled
/// text-input object is still an IME session the compositor may route keystrokes into.
///
/// docs/adr/0027 still records the design and it is still the right one for the *other* field kind:
/// what brings the binding back is an ordinary Lua-readable `textfield` with `on_change`/`on_submit`
/// (§ 5.2 item 8's unmasked half), which needs IME composition and must not be a raw keysym reader.
/// That one binds without `ContentPurpose::Password`, writes a Lua-visible buffer rather than this
/// one, and shares nothing with this path but the node kind.
///
/// **This adds no IDL surface, and § 5.2 still declares no key handler.** Nothing here reaches Lua:
/// the bytes go into a native buffer and out to the Supervisor, which is the whole definition of a
/// `secure_submit` field (docs/adr/0005), and a key that does not land in one is [`Ignore`d]. The
/// old comment on the empty `press_key` was right that docs/adr/0050 does not invent an `on_key`
/// property; it stays right, because this is not one.
///
/// [`Ignore`d]: SecureKeyAction::Ignore
///
/// **Control characters are filtered by their text, not by an allow-list of keysyms.** `utf8` is
/// `Some` for Escape, Tab and Return alike -- xkbcommon hands back the C0 control character -- so an
/// unfiltered append would bury an ESC byte inside a secret and leave PAM rejecting it for no
/// visible reason.
///
/// `repeat` exists for one case: a held Enter must not submit twice. A submit zeroizes the buffer as
/// it reads it (see [`secure_submit_frame`]), so the repeat would send an *empty* password to PAM
/// and spend one of the user's attempts on it. Characters and Backspace repeat normally, which is
/// what every text field does.
fn secure_key_action<'a>(event: &'a KeyEvent, repeat: bool) -> SecureKeyAction<'a> {
    match event.keysym {
        Keysym::Return | Keysym::KP_Enter => {
            if repeat {
                SecureKeyAction::Ignore
            } else {
                SecureKeyAction::Submit
            }
        }
        Keysym::BackSpace => SecureKeyAction::Backspace,
        // Escape used to fall through to the control-character filter below and be ignored, which
        // left one Backspace per character as the only way to abandon a mistyped password -- on the
        // one surface where getting it wrong costs a counted PAM attempt. Every other password
        // prompt clears on Escape; so does this one.
        Keysym::Escape => SecureKeyAction::Clear,
        _ => match event.utf8.as_deref() {
            Some(text) if !text.is_empty() && !text.chars().any(char::is_control) => SecureKeyAction::Append(text),
            _ => SecureKeyAction::Ignore,
        },
    }
}

/// The name `on_click`'s second argument carries for one evdev button code, or `None` for a button
/// this engine does not hand to Lua at all.
///
/// A string rather than the raw `273` or a normalized `1`/`2`/`3`, because every categorical value
/// that crosses this boundary already is one: `fit` (`image::Fit::from_str`), `layer` and `anchor`
/// (`layout::node::parse_layer`/`parse_anchor`), `align_h` (`parse_align`). docs/adr/0050's second
/// amendment argues the rest of it and is the place to change if this is ever revisited.
///
/// `None` means the press never arms and the release never fires, which is what an unhandled button
/// already did. The set that fires has to equal the set a config can name: handed `"other"` for
/// `BTN_TASK`, a config cannot tell it from `BTN_EXTRA`, cannot write a correct handler for either,
/// and would instead run whatever was written for the left button.
///
/// ponytail: back and forward do nothing, on a mouse that has them. Three of the eight `BTN_*`
/// codes `smithay_client_toolkit::seat::pointer` names are handled here and the other five are
/// dropped, which costs a five-button mouse its two thumb buttons. Adding them is not a rename:
/// real mice emit `BTN_SIDE` (0x113) and `BTN_EXTRA` (0x114) for back and forward, while
/// `BTN_BACK` (0x116) and `BTN_FORWARD` (0x115) carry the literal names and are rarer, so a correct
/// mapping is four codes onto two names and there is no caller to check it against yet. Map both
/// pairs with the first config that asks; this function is the only place that changes.
fn pointer_button_name(code: u32) -> Option<&'static str> {
    match code {
        BTN_LEFT => Some("left"),
        BTN_RIGHT => Some("right"),
        BTN_MIDDLE => Some("middle"),
        _ => None,
    }
}

/// Whether a release of `button` ends the press `armed` is holding, whether or not it completes it.
///
/// The pair of [`release_completes_click`] and the narrower of the two. Completing needs the same
/// surface, the same rect and the same button; ending needs only the same button, because dragging
/// off the node and releasing ends the press exactly as clicking does. What must not end it is a
/// release of a *different* button: that event is the release of some other press, and clearing the
/// slot for it throws away a press that is still live. Pressing left, pressing right, then
/// releasing left used to do that, and lost the right click as well as the left one.
fn release_ends_press(armed: Option<&ArmedClick>, button: u32) -> bool {
    armed.is_some_and(|armed| armed.button == button)
}

/// Whether a release on `instance_id`, over the button at `released_on`, completes `armed`
/// (docs/adr/0050 decision 2).
///
/// Both halves have to be a *button* hit, not merely the same coordinates: a release that lands in
/// the armed rect but on something that is no longer a handled button (the config re-resolved and
/// put a plain `rect` there) is not the click the press started. `released_on` is therefore
/// [`clickable_button`]'s answer for the release, not the raw pointer position.
fn release_completes_click(armed: Option<&ArmedClick>, instance_id: &str, released_on: Option<LogicalRect>, button: u32) -> bool {
    match (armed, released_on) {
        (Some(armed), Some(rect)) => armed.instance_id == instance_id && armed.rect == rect && armed.button == button,
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
/// `Err` names the step as well as carrying the error, because the two failures are not the same
/// bug: a rect table this engine could not build is the engine's, and a handler that raised is the
/// config's. [`App::fire_on_click`] prints the pair, and merging them would tell a config author to
/// look at their own Lua for a fault that is not there.
fn call_on_click(lua: &Lua, on_click: &Function, rect: LogicalRect, button: &str) -> Result<(), (&'static str, mlua::Error)> {
    let argument = rect_table(lua, rect).map_err(|e| ("could not build on_click's rect argument", e))?;
    on_click.call::<()>((argument, button)).map_err(|e| ("on_click raised, ignoring it", e))
}

/// `on_click`'s first argument: the button's rect as `{ x, y, width, height }` in its surface's
/// logical coordinates (docs/adr/0050 decision 3).
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
/// never presents a frame, so it must not be in the expected set. A `window` or `popup` declared
/// `visible = false` reaches the same answer by a shorter route, since docs/adr/0049 decision 1 does
/// not create its role object at all. A `popup` is the strongest case of the three: a Candidate is
/// frozen in [`App::apply_visibility`] and has no armed serial to grab with, so a declared popup
/// never presents during a handshake whatever its `visible` says.
///
/// An empty result is legal, not a degenerate case -- `drive_handshake`'s collection loop exits
/// immediately on an empty expected set, so a generation whose every surface starts hidden
/// completes its handshake.
fn presenting_surface_ids<'a>(surfaces: impl Iterator<Item = (&'a str, MapState)>) -> Vec<String> {
    surfaces
        .filter(|(_, state)| state.presents())
        .map(|(id, _)| id.to_string())
        .collect()
}

/// Whether every tracked surface has staged everything a PBA Candidate owes it, which is
/// [`App::maybe_send_ready_signal`]'s gate (§ 15.2 points 2-3). Takes `(null_buffered, exists)` per
/// surface, where `exists` is whether the surface currently has a Wayland object at all.
///
/// The second half is what build-steps.md Phase 22 item 1's "PBA's null-buffer staging needs no new
/// branch" missed, and it is a gate rather than a staging difference. The *staging* really does
/// generalize: xdg-shell's initial-commit discipline is layer-shell's, so a shown `window` attaches
/// a null buffer on its first configure through the identical code path. What does not generalize is
/// the assumption underneath the old `all(null_buffered)` gate -- that every tracked surface gets a
/// configure. A `panel` always does, because it is created and initially committed at startup even
/// when `visible` is false. A `window` declared `visible = false` has no `xdg_toplevel` at all
/// (docs/adr/0049 decision 1), so no configure is coming, `null_buffered` would stay false forever,
/// and the Candidate would never send its `ReadySignal` -- a `ready_timeout` hang on every config
/// that declares a hidden window, which is the shape the dev config already has. A `popup` widens
/// that from "a hidden window" to "any declared popup": a Candidate freezes `visible`
/// ([`App::apply_visibility`]) and has no armed serial to grab with, so a popup's `xdg_popup` never
/// exists during a handshake at all.
///
/// A surface with no object has nothing to stage and nothing to present, so it is complete by
/// construction. It is filtered out of the announced set by [`presenting_surface_ids`] on the same
/// `MapState::Unmapped` that makes it objectless here, which is what keeps the two in step.
fn candidate_has_staged(surfaces: impl Iterator<Item = (bool, bool)>) -> bool {
    surfaces.into_iter().all(|(null_buffered, exists)| null_buffered || !exists)
}

/// What a `window` takes on a configure axis the compositor left to it, when the config declared no
/// `min_size` to take instead.
///
/// ponytail: a constant, because § 6.2 gives a `window` no `width`/`height` for a config to state
/// one with, and its tree cannot answer either -- a toplevel's root is forced to the surface the
/// compositor granted (`layout::scene`'s `Scene::apply_one_instance`), so "the size the content
/// wants" is not a number this engine ever computes. The ceiling is that a config with no
/// `min_size` opens at this size on a compositor that leaves the first configure at zero, whatever
/// it actually draws. Two upgrade paths, either of which retires the constant: § 6.2 gaining an
/// advisory initial size, or a real two-pass content measure that can size a `Content` root against
/// a known budget (`resolve_and_reconcile`'s own `ponytail:` names that second pass).
const UNCONFIGURED_WINDOW_SIZE: (f32, f32) = (640.0, 480.0);

/// The size a toplevel's buffer takes for one `xdg_toplevel` configure (build-steps.md Phase 22
/// item 1).
///
/// A `Some` axis is the compositor's and is taken as given: `xdg_toplevel::configure`'s own wording
/// makes a maximized or fullscreen size binding, and a tiling compositor sizes every window this
/// way, so on niri this is the only branch that ever runs.
///
/// A `None` axis is "the client picks" ("If this value is None, you may set the size of the window
/// as you wish"), which is the ordinary first configure on a floating compositor. What it picks is
/// the config's own `min_size` for that axis, falling back to [`UNCONFIGURED_WINDOW_SIZE`], then
/// clamped by `max_size`. The hints are what a config can actually say about a window's size, and on
/// this branch the client holds the pen: § 6.2's "advisory" caveat is about what the *compositor*
/// may do with them, not a licence to ignore our own numbers when nobody else has chosen.
///
/// A zero `max_size` axis is not a maximum of zero: `set_max_size`'s own "0 means no expected
/// maximum size in the given dimension", the same reading [`node::window_spec`]'s parser applies
/// when it refuses a maximum below a minimum.
///
/// At least 1 on both axes, because a `wl_egl_window` of 0 is invalid and a window has to attach a
/// buffer to map at all.
fn toplevel_size_for(new_size: (Option<std::num::NonZeroU32>, Option<std::num::NonZeroU32>), spec: &WindowSpec) -> (u32, u32) {
    let axis = |configured: Option<std::num::NonZeroU32>, fallback: f32, min: f32, max: f32| -> u32 {
        if let Some(configured) = configured {
            return configured.get();
        }
        let mut picked = if min > 0.0 { min } else { fallback };
        if max > 0.0 {
            picked = picked.min(max);
        }
        (picked.max(1.0)) as u32
    };
    let min = spec.min_size.unwrap_or(SizeHint { width: 0.0, height: 0.0 });
    let max = spec.max_size.unwrap_or(SizeHint { width: 0.0, height: 0.0 });
    (
        axis(new_size.0, UNCONFIGURED_WINDOW_SIZE.0, min.width, max.width),
        axis(new_size.1, UNCONFIGURED_WINDOW_SIZE.1, min.height, max.height),
    )
}

/// The `xdg_toplevel` requests one *live* toplevel needs after a re-resolve changed its `window`
/// properties (§ 6.2, docs/adr/0049's second amendment). `None` per field means "unchanged, send
/// nothing", exactly as [`SpecUpdate`] does for a panel and for the same reason: all four are
/// double-buffered, so re-sending an unchanged value is noise rather than an error.
///
/// **Every one of § 6.2's protocol-facing fields is here, which is the difference from a panel.**
/// `SpecUpdate` deliberately omits [`node::SurfaceTopology`]'s five, because `get_layer_surface`
/// fixes them at creation. A toplevel has no such set: `xdg-shell.xml` says of `set_app_id` that it
/// "can be sent after the xdg_toplevel has been mapped to update the property", `set_title` is the
/// same shape, and both size hints are ordinary double-buffered requests. So a changed `title` is an
/// in-place update, never a recreate, and the only field left out is `id`, which is the reconcile
/// identity rather than a protocol field.
///
/// `Option<Option<SizeHint>>` reads oddly and is the honest type: the outer layer is "did it move",
/// the inner one is § 6.2's own absent-versus-present distinction, and "moved to absent" is a real
/// transition that has to reach `set_min_size(None)` -- which the protocol spells as a zero, meaning
/// unset.
#[derive(Debug, Default, PartialEq)]
struct WindowUpdate {
    title: Option<String>,
    app_id: Option<String>,
    min_size: Option<Option<SizeHint>>,
    max_size: Option<Option<SizeHint>>,
}

fn window_update(applied: &WindowSpec, fresh: &WindowSpec) -> WindowUpdate {
    WindowUpdate {
        title: (fresh.title != applied.title).then(|| fresh.title.clone()),
        app_id: (fresh.app_id != applied.app_id).then(|| fresh.app_id.clone()),
        min_size: (fresh.min_size != applied.min_size).then_some(fresh.min_size),
        max_size: (fresh.max_size != applied.max_size).then_some(fresh.max_size),
    }
}

/// A [`SizeHint`] as the two `xdg_toplevel` requests take it. `None` stays `None`, which
/// `Window::set_min_size`/`set_max_size` send as the protocol's zero, meaning unset.
fn size_hint_pair(hint: Option<SizeHint>) -> Option<(u32, u32)> {
    hint.map(|hint| (hint.width.max(0.0) as u32, hint.height.max(0.0) as u32))
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

    /// One tracked surface per surface instance, built from the evaluation that declared it
    /// (docs/adr/0038 decision 1, docs/adr/0049 decision 1, build-steps.md Phase 20 items 1 and 2
    /// and Phase 22 item 1). This replaced `create_main_bar`/`create_overlay_canvas`/
    /// `create_wallpaper_layers`, which ran *before* any Lua had been evaluated and discarded every
    /// field the config wrote.
    ///
    /// The roles diverge in what "create" means, and only there. A `panel` gets its
    /// `zwlr_layer_surface_v1` here whatever its `visible` says, because that object lives as long
    /// as the generation. A `window` or `popup` gets a `TrackedSurface` here and its Wayland object
    /// only if `visible` already resolves true, through the same [`App::show_window`] and
    /// [`App::show_popup`] a later flip uses -- one creation path, not a startup special case.
    ///
    /// `instances` and `specs` come from the same evaluation, so an instance whose declared id has
    /// no spec cannot happen; it is skipped with a log rather than panicking, on the same
    /// "keep the shell up" principle as every other failure in this file.
    ///
    /// **`specs` contributes the roster, not the field values** ([`resolved_surface_spec`],
    /// [`starting_visible`]). It is parsed from the evaluation's *unresolved* properties, which is
    /// what makes it the wrong thing to build a surface out of: the caller resolves the scene
    /// between parsing it and calling this, so every signal-bound field in it is still at its
    /// parser placeholder. What it is right for is which declarations exist and what role each one
    /// is, neither of which a re-resolve can change (docs/adr/0049 decision 3).
    ///
    /// Called with the whole instance set at startup and with only the *added* instances on a
    /// monitor hotplug (see [`App::handle_output_change`]) -- the same function either way, since
    /// "build the surface this instance names" is the same job in both.
    fn create_surfaces(&mut self, qh: &QueueHandle<App>, specs: &[SurfaceSpec], instances: &[SurfaceInstance]) {
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
            let Some(roster) = specs.iter().find(|spec| spec.declared_id() == instance.declared_id) else {
                eprintln!("[oblisk-renderer] instance {:?} has no matching declaration; skipping", instance.instance_id);
                continue;
            };
            // `Scene::surface` hands back an owned `ResolvedNode`, so nothing borrows `self` past
            // this line and the `&mut self` creates below are free to run.
            let tree = self.client.scene().surface(&instance.instance_id);
            let visible = starting_visible(tree.as_ref().map(|tree| tree.visible), roster);
            // Built from the *resolved* properties, exactly as [`App::apply_resolved_state`] builds
            // it on every later pass (docs/adr/0049's second amendment). `run` parses `specs` out of
            // the evaluation's raw properties, before `resolve_properties` has run, so a
            // signal-bound field is still at its parser placeholder there -- and for a popup that is
            // permanent damage rather than one stale frame: every `PopupSpec` field is an
            // `xdg_positioner` request, the positioner is consumed by `get_popup`, and
            // `xdg_popup.reposition` is deliberately not built, so a popup shown from the roster
            // spec keeps `DEFERRED_POPUP_EXTENT`'s 1x1-at-(0,0) placeholder for its whole life.
            //
            // A `panel` goes through the same path rather than being special-cased, and that is not
            // scope creep: `resolve_properties` copies structural properties through raw, so a
            // panel's topology is identical either way, and re-deriving from the resolved tree is
            // what `apply_spec_change` already does on every later pass.
            let spec = match tree.as_ref().map(|tree| resolved_surface_spec(roster, &tree.properties)) {
                Some((_, Ok(fresh))) => fresh,
                Some((role, Err(err))) => {
                    eprintln!(
                        "[oblisk-renderer] {}: re-resolved {role} properties are invalid, keeping the last applied ones: {err}",
                        instance.instance_id
                    );
                    roster.clone()
                }
                None => roster.clone(),
            };

            match &spec {
                SurfaceSpec::Panel(panel) => self.create_panel(qh, panel, instance, &outputs, visible),
                SurfaceSpec::Window(window) => self.create_window(qh, window, instance, visible),
                SurfaceSpec::Popup(popup) => self.create_popup(qh, popup, instance, visible),
                SurfaceSpec::Lock(_) => self.create_lock(instance, &outputs),
            }
        }
        // The "and any new outputs as they are advertised" half of `ext-session-lock-v1`'s own
        // expectation (docs/adr/0042, build-steps.md Phase 23 item 1). A no-op unless a lock is
        // being held right now, which at startup it never is; on a monitor hotplug it is what gives
        // the freshly advertised output its lock surface instead of leaving the compositor to paint
        // a solid colour there.
        self.ensure_lock_surfaces(qh);
    }

    /// [`App::create_surfaces`]'s `lock` arm: the tracking entry always, the
    /// `ext_session_lock_surface_v1` never from here (docs/adr/0052 decision 2).
    ///
    /// The entry exists for [`App::create_window`]'s reason and for one that is stronger. It is what
    /// makes the retained scene resolve this instance's tree at all, which is what lets an in-place
    /// reload restyle a live lock screen; and it is the **only** record that this config declares a
    /// lock screen, which is the fact docs/adr/0052 decision 3 refuses a lock on the absence of.
    /// [`App::set_session_lock`] asks that question by looking for these entries, so there is no
    /// second roster to keep in step with this one.
    ///
    /// No `visible` is consulted and there is none to consult: `layout::node::lock_spec` refuses the
    /// property outright, because the compositor owns this surface's lifetime end to end. The
    /// resolved tree's default `true` is not a statement the config made.
    ///
    /// The `wl_output` is kept rather than the output's name, for the reason [`TrackedRole::Lock`]
    /// gives: `get_lock_surface` takes the proxy, and this is the one place it is already in hand.
    fn create_lock(&mut self, instance: &SurfaceInstance, outputs: &HashMap<String, wl_output::WlOutput>) {
        let Some(output) = outputs.get(&instance.output) else {
            eprintln!("[oblisk-renderer] instance {:?} names an output that has since gone; skipping", instance.instance_id);
            return;
        };
        self.surfaces.push(TrackedSurface {
            role: TrackedRole::Lock { output: output.clone(), surface: None },
            bound: None,
            surface_id: instance.instance_id.clone(),
            map_state: MapState::Unmapped,
            null_buffered: false,
            configured_size: (0, 0),
        });
    }

    /// One `ext_session_lock_surface_v1` for every declared `lock` instance that does not have one
    /// yet, or nothing at all if this process holds no lock (build-steps.md Phase 23 item 1).
    ///
    /// **Idempotent per output, and that is a protocol requirement rather than tidiness.** A second
    /// lock surface on one output is a `duplicate_output` error, which kills the connection with the
    /// session still locked. `layout::instance::expand_instances` produces one `lock` instance per
    /// output *per declared lock spec* (docs/adr/0052 decision 2), so "this instance already has a
    /// surface" and "this output already has one" are the same test only while a config declares at
    /// most one `lock` -- which is exactly what `crate::socket`'s `surface_specs` now refuses to let
    /// through, for this invariant. The `surface: None` in the pattern below is the per-instance
    /// half; that refusal is the other half, and neither is sufficient alone.
    ///
    /// Three callers, one job, because "make the set of lock surfaces match the set of outputs" is
    /// the same job whenever either set moves. Right after `lock` succeeds, because the protocol
    /// asks clients to "immediately create lock surfaces for all outputs currently present" -- the
    /// compositor may wait for them before it sends `locked`, precisely to avoid showing a blank
    /// frame first, and a client that waits for `locked` to create them guarantees that blank frame
    /// for however long the compositor's time limit is. Again on `locked` itself, for an output
    /// advertised inside that window. And from [`App::create_surfaces`], which is the hotplug path.
    ///
    /// **No commit here, and this is the one role where that is not an oversight.** Every other
    /// create path in this file ends in the initial commit its shell protocol requires;
    /// `ext_session_lock_surface_v1` inverts the rule -- "Committing the surface before acking the
    /// first configure is a protocol error" -- and the compositor sends that first configure
    /// immediately on `get_lock_surface`. So `MapState::AwaitingConfigure` here means what it means
    /// everywhere else, the ordinary [`App::bind_and_clear`] path does the whole map, and SCTK has
    /// already acked by the time it runs.
    fn ensure_lock_surfaces(&mut self, qh: &QueueHandle<App>) {
        // Cloned out of `self` so the `&mut self` calls in the loop are free to run; `SessionLock`
        // is an `Arc` handle, so this is a refcount bump and not a second lock.
        let Some(lock) = self.session_lock.clone() else {
            return;
        };
        for index in 0..self.surfaces.len() {
            let TrackedRole::Lock { output, surface: None } = &self.surfaces[index].role else {
                continue;
            };
            let output = output.clone();
            let wl_surface = self.compositor_state.create_surface(qh);
            let lock_surface = lock.create_lock_surface(wl_surface, &output, qh);
            if let TrackedRole::Lock { surface, .. } = &mut self.surfaces[index].role {
                *surface = Some(lock_surface);
            }
            self.surfaces[index].map_state = MapState::AwaitingConfigure;
            eprintln!("[oblisk-renderer] {}: lock surface created, awaiting its configure", self.surfaces[index].surface_id);
        }
    }

    /// Destroys every live `ext_session_lock_surface_v1` and frees the EGL side behind it, leaving
    /// the tracking entries where they are.
    ///
    /// The order is [`App::destroy_surface_by_id`]'s minus its last step: `eglDestroySurface` and
    /// `wl_egl_window_destroy` first ([`App::release_bound`]), then the `SessionLockSurface` handle,
    /// whose `Drop` sends `destroy` and then destroys the `wl_surface` underneath it. A
    /// `wl_egl_window` still pointing at a destroyed `wl_surface` is the failure that order exists
    /// to prevent, and it does not care which protocol destroyed the surface.
    ///
    /// The entries survive because the declarations did. A `lock` instance is one retained node for
    /// as long as the config declares it, and the next lock builds its surfaces again through
    /// [`App::ensure_lock_surfaces`] -- the same shape a `window` has when `visible` goes false.
    ///
    /// **When this runs relative to the unlock is load-bearing.** On the ordered-unlock path it runs
    /// *after* `unlock_and_destroy`, because destroying a lock surface whose output is still active
    /// while the session is still locked makes the compositor "fall back to rendering a solid
    /// color" -- a visible flash between the password being accepted and the desktop coming back.
    /// On the `finished` path there is no such window: the compositor has already ended the lock.
    fn teardown_lock_surfaces(&mut self) {
        for index in 0..self.surfaces.len() {
            if !matches!(self.surfaces[index].role, TrackedRole::Lock { surface: Some(_), .. }) {
                continue;
            }
            self.release_bound(index);
            if let TrackedRole::Lock { surface, .. } = &mut self.surfaces[index].role {
                *surface = None;
            }
            self.surfaces[index].map_state = MapState::Unmapped;
            eprintln!("[oblisk-renderer] {}: lock surface destroyed", self.surfaces[index].surface_id);
        }
    }

    /// One `SetSessionLock` from the Supervisor (docs/adr/0042, docs/adr/0052 decision 1). The
    /// decision is [`lock_command`], which is pure and tested; this is the protocol traffic it does
    /// not do.
    ///
    /// `declares_lock` is counted off the tracked surface set rather than off the roster or the
    /// scene, because that set is the one place the question has a single answer: `create_lock`
    /// pushes an entry per `lock` instance and `destroy_surface_by_id` removes it with its output.
    /// A session with no outputs at all therefore declares no lock instance and is refused, which is
    /// right -- there is no screen to lock.
    ///
    /// `can_authenticate` is the second question and it goes to the *scene*, because that is where
    /// the answer lives: a tracked instance says a `lock` node was written, and only its resolved
    /// tree says whether anything under it could ever produce the `SecureSubmit` that unlocks (see
    /// [`LOCK_CANNOT_AUTHENTICATE`]). The per-tree answer is [`tree_can_authenticate`], which is the
    /// *same* predicate keyboard focus arms on -- see its doc comment for why two nearly-equal rules
    /// here strand the machine. `any`, not `all`, across the instances: one lock surface per output
    /// is the protocol's requirement and they all resolve from the same declaration, so a single
    /// typable one is the config being correct rather than a partial answer.
    ///
    /// A `lock` that fails at the protocol level is a refusal and not a crash, which is the same
    /// tolerance [`App::show_window`] applies to a missing `xdg_wm_base`: the shell keeps painting,
    /// and the one thing that did not happen says so through the channel docs/adr/0052 decision 4
    /// named for it.
    fn set_session_lock(&mut self, qh: &QueueHandle<App>, locked: bool) {
        let lock_instances: Vec<String> = self
            .surfaces
            .iter()
            .filter(|tracked| matches!(tracked.role, TrackedRole::Lock { .. }))
            .map(|tracked| tracked.surface_id.clone())
            .collect();
        let can_authenticate = lock_instances.iter().filter_map(|id| self.client.scene().surface(id)).any(|tree| tree_can_authenticate(&tree));
        match lock_command(locked, !lock_instances.is_empty(), can_authenticate, self.session_lock.is_some()) {
            LockCommand::Nothing => {}
            LockCommand::Refuse(reason) => self.refuse_lock(reason),
            LockCommand::Acquire => match self.session_lock_state.lock(qh) {
                Ok(lock) => {
                    self.session_lock = Some(lock);
                    // Armed here rather than on `locked`. A reload landing between the request and
                    // the grant would otherwise strip the password field out of the very tree the
                    // compositor is about to put on screen -- see `crate::socket`'s
                    // `lock_stays_authenticatable`, which is also why nothing but the fact of the
                    // lock is handed over: the ids the guard above answered on are the ids that
                    // existed *now*, and a monitor hotplug retires and replaces them.
                    self.client.set_session_locked(true);
                    self.ensure_lock_surfaces(qh);
                    eprintln!("[oblisk-renderer] session lock requested; waiting for the compositor's `locked` or `finished`");
                }
                // The compositor advertises no `ext_session_lock_manager_v1`. Carried as the
                // error's own words rather than a constant beside the other two, because this is
                // the one refusal whose cause is outside both this shell and its config, and
                // `GlobalError` already says which global is missing.
                Err(err) => {
                    let reason = format!("this compositor cannot lock the session: {err} (docs/adr/0042)");
                    self.refuse_lock(&reason);
                }
            },
            LockCommand::Release => self.release_session_lock(),
        }
    }

    /// `unlock_and_destroy`, and **the only path in this process that performs one** (docs/adr/0042,
    /// docs/adr/0052's consequences).
    ///
    /// It is reachable from exactly one place: a `SetSessionLock { locked: false }`, which the
    /// Supervisor sends only from the `pam_outcomes` arm of its `select!` loop, on a
    /// `PamOutcome::Success`. The PAM conversation itself is spawned off that loop rather than
    /// awaited inside the `secure_submit(lock, authenticate)` arm that starts it, so the arm that
    /// *begins* an attempt and the arm that *orders* the unlock are two, but the property is
    /// unchanged and is the reason this comment exists: `LockController::unlock` still has exactly
    /// one call site and it is still reached only on a `Success`, which makes "never unlock except
    /// on a successful authentication" a property of one call site in the Supervisor rather than a
    /// rule the Renderer has to be trusted with. **No convenience path may be added here** -- not on
    /// shutdown, not on a `finished`, not on a config reload. SCTK's `Drop` deliberately does not
    /// unlock, and the reason is the whole security model: a Renderer that dies while locked leaves
    /// the session locked, and anything in this file that unlocked on its own initiative would be
    /// the one way to turn a crash into an unlocked desktop.
    ///
    /// `SessionLock::unlock` is itself a no-op unless `is_locked()`, so the in-flight case -- a
    /// `lock` request whose `locked` has not arrived -- sends nothing and the `Drop` below sends the
    /// plain `destroy` the protocol requires there instead. That is not belt-and-braces on top of
    /// [`lock_command`]'s guard; it is SCTK's guarantee, and it is what makes the two cases one
    /// function. **It is also why [`run`] round-trips before calling this** -- `is_locked()` is set
    /// when `locked` is *dispatched*, not when the compositor sends it, so without that round trip
    /// a `locked` sitting unread on the wire would make this send a plain `destroy` that the
    /// compositor answers with `invalid_destroy`.
    ///
    /// The report matches what happened rather than what was asked for ([`release_outcome`]): a
    /// no-op `unlock` released nothing, and the Supervisor's `active` flag moves on these reports.
    fn release_session_lock(&mut self) {
        let Some(lock) = self.session_lock.take() else {
            return;
        };
        // Read before the unlock, because `unlock_and_destroy` is a destructor and the flag it is
        // gated on is the same one being reported here.
        let was_locked = lock.is_locked();
        lock.unlock();
        // Sends `ext_session_lock_v1.destroy` if `unlock` did not already destroy the object, which
        // is SCTK's sanctioned sequence: its own `SessionLock` doc says a locked object must be
        // `unlock`ed before it is dropped.
        drop(lock);
        // After the unlock, per [`App::teardown_lock_surfaces`]'s last paragraph.
        self.teardown_lock_surfaces();
        // Nothing is locked any more, so an in-place reload is free to reshape the lock screen
        // however it likes again, including out of existence.
        self.client.set_session_locked(false);
        let outcome = release_outcome(was_locked);
        match &outcome {
            LockOutcome::Unlocked => eprintln!("[oblisk-renderer] the session lock was released"),
            _ => eprintln!("[oblisk-renderer] {LOCK_NEVER_GRANTED}"),
        }
        self.report_lock(outcome);
    }

    /// A lock that was asked for and did not happen: logged, pushed to `oblisk.rescue`, and reported
    /// (docs/adr/0052 decision 4).
    ///
    /// `rescue` is the right channel and the ADR's reasoning is worth restating where the write
    /// happens: a refused lock leaves the *ordinary* scene on the glass, so there is no lock screen
    /// for the message to appear on, and `rescue` is rendered by the config's own surfaces. The
    /// wrong-password case is the opposite and deliberately does not come here -- it happens with
    /// the lock surfaces mapped and everything else hidden, so it reaches the config as `oblisk.lock`
    /// state instead.
    ///
    /// Nothing clears this again on purpose. A later successful evaluation clears `rescue` on its
    /// own success path (`RendererClient::handle_reevaluate`), which is exactly the event that
    /// matters: the ordinary way out of `NO_LOCK_DECLARED` is editing the config to declare a lock
    /// screen, and that edit *is* a re-evaluation. Clearing it on a subsequent successful lock
    /// instead would stamp on an unrelated evaluation failure that had nothing to do with locking.
    fn refuse_lock(&mut self, reason: &str) {
        eprintln!("[oblisk-renderer] the session lock was refused: {reason}");
        self.client.set_rescue_state(true, reason);
        self.report_lock(LockOutcome::Refused(reason.to_string()));
    }

    /// Queues one `LockReport` on the outbound channel, the way `ReadySignal` and
    /// `PresentationEvidence` are queued. Every lock state change goes through here, so the
    /// Supervisor's `lock::apply` sees each transition exactly once -- its `active` flag moves on
    /// these reports alone and on nothing it ordered itself.
    fn report_lock(&mut self, outcome: LockOutcome) {
        if let Err(e) = self.outbound_tx.send(RendererFrame::LockReport(LockReport { outcome })) {
            eprintln!("[oblisk-renderer] failed to queue a LockReport for the socket thread: {e}");
        }
    }

    /// [`App::create_surfaces`]'s `panel` arm: one `zwlr_layer_surface_v1` on this instance's own
    /// output, initially committed and tracked.
    fn create_panel(
        &mut self,
        qh: &QueueHandle<App>,
        spec: &PanelSpec,
        instance: &SurfaceInstance,
        outputs: &HashMap<String, wl_output::WlOutput>,
        visible: bool,
    ) {
        let Some(output) = outputs.get(&instance.output) else {
            eprintln!("[oblisk-renderer] instance {:?} names an output that has since gone; skipping", instance.instance_id);
            return;
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
            return;
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

        // § 6.1's `visible`. A panel declared `visible = false` is still created (docs/adr/0038
        // decision 2: `visible` maps and unmaps, it does not create and destroy). It still performs
        // the initial commit directly above, which `get_layer_surface` requires before any configure
        // arrives and which does not map anything on its own; what makes it invisible is that no
        // buffer is ever attached, and `MapState::Unmapped` is what keeps `paint_surface` from
        // attaching one. No *unmap* commit is needed or wanted here, since on an already-bufferless
        // surface that is the protocol's re-map procedure rather than an unmap -- see
        // [`App::remap`], which measured both sides of this distinction against a real compositor.
        self.surfaces.push(TrackedSurface {
            role: TrackedRole::Panel { layer, spec: spec.clone(), output_size: instance.available },
            bound: None,
            surface_id: instance.instance_id.clone(),
            map_state: if visible { MapState::AwaitingConfigure } else { MapState::Unmapped },
            null_buffered: false,
            configured_size: (0, 0),
        });
    }

    /// [`App::create_surfaces`]'s `window` arm: the tracking entry always, the `xdg_toplevel` only
    /// if this window is already shown (docs/adr/0049 decision 1).
    ///
    /// The entry exists either way because it is what makes the window reachable at all: the poll
    /// loop's [`App::apply_resolved_surface_state`] walks `self.surfaces`, and a window with no
    /// entry would never have its `visible` looked at, so it could never open.
    fn create_window(&mut self, qh: &QueueHandle<App>, spec: &WindowSpec, instance: &SurfaceInstance, visible: bool) {
        self.surfaces.push(TrackedSurface {
            role: TrackedRole::Window { window: None, spec: spec.clone() },
            bound: None,
            surface_id: instance.instance_id.clone(),
            map_state: MapState::Unmapped,
            null_buffered: false,
            configured_size: (0, 0),
        });
        if visible {
            let index = self.surfaces.len() - 1;
            self.show_window(qh, index);
        }
    }

    /// [`App::create_surfaces`]'s `popup` arm: the tracking entry always, the `xdg_popup` only if
    /// this popup is already shown (docs/adr/0049 decision 1, docs/adr/0051 decision 1).
    ///
    /// The entry exists for [`App::create_window`]'s reason, restated by docs/adr/0051's
    /// consequences: the instance is what makes the scene resolve the popup's tree at all, and
    /// `visible` is read off that resolved tree. Twenty declared popups still cost twenty retained
    /// nodes and zero Wayland objects, which is docs/adr/0049's memory claim intact.
    ///
    /// A popup declared `visible = true` at startup with § 6.3's default `grab = true` is refused by
    /// [`App::show_popup`] and says so once, which is correct rather than a startup failure: nothing
    /// has been clicked, so there is no serial, and a dropdown that cannot be dismissed by clicking
    /// outside it is worse than one that did not open (docs/adr/0049's amendment).
    fn create_popup(&mut self, qh: &QueueHandle<App>, spec: &PopupSpec, instance: &SurfaceInstance, visible: bool) {
        self.surfaces.push(TrackedSurface {
            role: TrackedRole::Popup { popup: None, spec: spec.clone(), dismissed_at: None, refusal_logged: None },
            bound: None,
            surface_id: instance.instance_id.clone(),
            map_state: MapState::Unmapped,
            null_buffered: false,
            configured_size: (0, 0),
        });
        if visible {
            let index = self.surfaces.len() - 1;
            self.show_popup(qh, index);
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

        let specs = self.client.applied_surface_specs();
        let fresh = expand_instances(&specs, &geometries_from(&screens));
        let reconcile = reconcile_instances(self.client.instances(), &fresh);

        for instance_id in &reconcile.removed {
            self.destroy_surface_by_id(instance_id);
        }
        // A surviving *panel*'s `output_size` is the basis a `SizeMode::Percent` resolves against,
        // so a mode change that resized the monitor under it has to move it -- `fresh` carries the
        // output's *current* logical size, while the instance set deliberately keeps the size the
        // compositor configured each surface to (see `reconcile_instances`). A `window` has no such
        // field: § 6.2 gives it no size request, and the seed `expand_instances` hands its instance
        // is superseded by the first configure.
        for instance in &fresh {
            if let Some(TrackedRole::Panel { output_size, .. }) =
                self.surfaces.iter_mut().find(|s| s.surface_id == instance.instance_id).map(|s| &mut s.role)
            {
                *output_size = instance.available;
            }
        }
        // Before `create_surfaces`, which reads the scene by instance id to decide a new surface's
        // starting `visible`.
        self.client.set_instances(reconcile.instances);
        self.create_surfaces(qh, &specs, &reconcile.added);
        self.client.request_reload();
    }

    /// Frees one surface's rendering side -- its EGL surface and its `wl_egl_window` -- and leaves
    /// it unbound, with its role object untouched. Steps 1 and 2 of the teardown order
    /// [`App::destroy_surface_by_id`] documents; whoever calls this owns step 3.
    ///
    /// Two callers, and they differ in what they do with the role object rather than in how they
    /// free this half: `destroy_surface_by_id` drops it, and [`App::hide_window`] drops only the
    /// `xdg_toplevel` and keeps the tracking entry (docs/adr/0049 decision 1).
    fn release_bound(&mut self, index: usize) {
        let Some(bound) = self.surfaces[index].bound.take() else {
            return;
        };
        // `eglDestroySurface`, by hand, because `khronos_egl::Surface` is a plain copyable handle
        // with no `Drop` -- without this every unplugged monitor and every closed window leaks one
        // EGL surface. It has to come before the `wl_egl_window` is destroyed, since
        // [`BoundSurface`]'s own contract is that the `WlEglSurface` outlives the EGL surface built
        // from it.
        if let Err(err) = self.egl.instance.destroy_surface(self.egl.display, bound.egl_surface) {
            log_bind_failure(&self.surfaces[index].surface_id, "eglDestroySurface", err);
        }
        // `BoundSurface`'s drop, which is `wl_egl_window_destroy`.
        drop(bound);
        self.surfaces[index].configured_size = (0, 0);
    }

    /// Destroys one surface instance: its role object, its `wl_surface`, its `wl_egl_window`, and
    /// its EGL surface (docs/adr/0038 decision 3's removal half). A no-op for an id this process has
    /// no surface for, which is the normal case for the second of the two events an unplugged
    /// monitor produces -- `zwlr_layer_surface_v1::closed` and `OutputHandler::output_destroyed`
    /// both arrive, in either order, and whichever comes first does the work.
    ///
    /// Teardown runs outermost-first, and the explicit steps below are what make that so rather
    /// than leaving it to field order (`TrackedSurface` declares `role` before `bound`, so a plain
    /// drop would destroy the `wl_surface` out from under the `wl_egl_window` still pointing at it):
    ///
    /// 0. Every popup rooted under this surface ([`App::drop_child_popups`]), because xdg-shell
    ///    refuses to destroy an `xdg_surface` that still has one.
    /// 1. `eglDestroySurface`, by hand ([`App::release_bound`]).
    /// 2. `BoundSurface`'s drop, which is `wl_egl_window_destroy` (also `release_bound`).
    /// 3. The role object's drop, which destroys the role (`zwlr_layer_surface_v1`, or an
    ///    `xdg_toplevel` preceded by its decoration object) and then the `wl_surface`, in that
    ///    order -- both protocols require it and `smithay_client_toolkit` implements it, so it is
    ///    not this function's job.
    fn destroy_surface_by_id(&mut self, instance_id: &str) {
        let Some(index) = self.surfaces.iter().position(|s| s.surface_id == instance_id) else {
            return;
        };
        // Step 0, and it has to run before the `remove` below invalidates every index past this
        // one: an unplugged monitor destroys a per-output panel, and a popup still rooted under it
        // would outlive its parent's `wl_surface`. See [`App::drop_child_popups`].
        self.drop_child_popups(index);
        self.release_bound(index);
        let TrackedSurface { role, surface_id, .. } = self.surfaces.remove(index);
        drop(role);
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
    ///
    /// Role-agnostic since build-steps.md Phase 22 item 1, and that is the ADR-0040 decision 4
    /// claim made literal: xdg-shell's initial-commit discipline is `zwlr_layer_surface_v1`'s, so
    /// an `xdg_toplevel` configure lands here through the same path with nothing branching on which
    /// protocol asked. The two callers differ only in where the size comes from -- layer-shell
    /// hands one over, and a toplevel's may be the client's to pick (see [`toplevel_size_for`]).
    fn bind_and_clear(&mut self, index: usize, width: u32, height: u32) {
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
            // Cloned rather than borrowed: a `wl_surface` proxy is a refcounted handle, and holding
            // a borrow of `self.surfaces` across the `null_buffered` write below would not compile.
            let surface = self.surfaces[index].role.wl_surface().cloned();
            if let Some(surface) = surface.filter(|_| self.surfaces[index].map_state.presents()) {
                if !self.surfaces[index].null_buffered {
                    // verified against wayland_client::protocol::wl_surface::WlSurface's generated
                    // API: `attach(&self, buffer: Option<&wl_buffer::WlBuffer>, x: i32, y: i32)`,
                    // `commit(&self)`.
                    surface.attach(None, 0, 0);
                    self.surfaces[index].null_buffered = true;
                }
                // Committed on every candidate-mode configure rather than only the first: a
                // Candidate has no `swap_buffers` to ride on until `ActivateDraw`, so this is the
                // only commit that can carry the state staged directly above.
                surface.commit();
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
    ///
    /// A no-op on a `window`, and the protocol is why rather than an omission: an exclusive zone is
    /// `zwlr_layer_surface_v1`'s own request, and a toplevel reserves no screen area -- reserving
    /// space is what makes a surface a shell component instead of a window (§ 6.1, § 6.2).
    fn apply_exclusive_zone(&mut self, index: usize) {
        let tracked = &self.surfaces[index];
        let TrackedRole::Panel { layer, spec, .. } = &tracked.role else {
            return;
        };
        let zone = if spec.exclusive { exclusive_zone_for(spec.topology.anchor, tracked.configured_size) } else { 0 };
        layer.set_exclusive_zone(zone);
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

    /// Pushes one surface's freshly resolved root back to the compositor: the protocol fields its
    /// role permits changing on a live object, the input region, and whether the surface is shown
    /// at all (docs/adr/0038 decision 2, docs/adr/0049 decisions 1-2, § 6.1's `visible` and
    /// `margin` rows, § 6.2's `title` row, build-steps.md Phase 20 items 1 and 5 and Phase 22
    /// items 1 and 5).
    ///
    /// **This is where a `window`'s authoritative [`WindowSpec`] is derived, and the "resolved" is
    /// the whole point** (docs/adr/0049's second amendment). `crate::socket`'s `surface_specs`
    /// parses the *unresolved* properties, which is right for a `panel`'s topology fields -- they
    /// reject a `Signal` on purpose, because `get_layer_surface` fixes them at creation. A
    /// `window`'s `title` is the opposite case: § 6.2 spells it as `string`/`Signal` precisely so it
    /// can move, and parsing it at evaluation time would freeze it at whatever the file last saw.
    /// `tree.properties` here is a `resolve_properties` result, so every `Signal` in it has already
    /// been read exactly once for this pass (ADR-0044 decision 1) -- one read, at the one point the
    /// surface is being reconciled, which is the same read `visible` and the input region below use.
    ///
    /// All three pushes are double-buffered `wl_surface` state and are therefore *staged* here, not
    /// committed: the caller's commit -- `paint_surface`'s `swap_buffers` on a mapped surface, the
    /// candidate branch's own commit on a staging Candidate -- carries the whole update at once.
    /// The exceptions are the map, unmap, create and destroy transitions, which are commits (or
    /// object lifetimes) by definition and perform their own.
    ///
    /// The spec push runs **before** `apply_visibility`, and for a `window` that ordering is
    /// load-bearing rather than incidental: a `visible` flip from false to true creates the
    /// `xdg_toplevel` out of the stored spec, so the spec has to be this pass's before the object
    /// is built from it.
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

        match &self.surfaces[index].role {
            TrackedRole::Panel { .. } => match node::panel_spec(&tree.properties) {
                Ok(fresh) => self.apply_spec_change(index, fresh),
                Err(err) => eprintln!(
                    "[oblisk-renderer] {surface_id}: re-resolved panel properties are invalid, keeping the last applied ones: {err}"
                ),
            },
            TrackedRole::Window { .. } => match node::window_spec(&tree.properties) {
                Ok(fresh) => self.apply_window_change(index, fresh),
                Err(err) => eprintln!(
                    "[oblisk-renderer] {surface_id}: re-resolved window properties are invalid, keeping the last applied ones: {err}"
                ),
            },
            TrackedRole::Popup { .. } => match node::popup_spec(&tree.properties) {
                // A store, not a diff, and the protocol is why: every field on a `PopupSpec` is an
                // `xdg_positioner` request, the positioner is consumed by `get_popup`, and
                // `xdg_popup.reposition` is deliberately not built. So there is nothing to send at
                // a live popup and nothing a diff could find -- what this push buys is that the
                // *next* `show_popup` builds its positioner from this pass's `anchor_rect`, which
                // is the whole of docs/adr/0049's second amendment. The click that opens a dropdown
                // writes the button's rect to a `state` signal on the same turn this reads it.
                Ok(fresh) => {
                    if let TrackedRole::Popup { spec, .. } = &mut self.surfaces[index].role {
                        *spec = fresh;
                    }
                }
                Err(err) => eprintln!(
                    "[oblisk-renderer] {surface_id}: re-resolved popup properties are invalid, keeping the last applied ones: {err}"
                ),
            },
            // Nothing to push, and § 6.4 is the reason rather than an omission. A lock surface has
            // no protocol field a config could set: `ext_session_lock_surface_v1` has exactly one
            // request, `ack_configure`, and the size arrives in the configure rather than being
            // asked for, so a re-parsed `LockSpec` would carry an `id` this surface already has and
            // nothing else to send.
            //
            // The create path *does* call `node::lock_spec`, through [`resolved_surface_spec`], and
            // the two are not in disagreement: that call exists to rebuild a `SurfaceSpec` for
            // `create_surfaces` to dispatch its four-arm `match` on, and its `Err` is what makes a
            // `lock` whose resolved properties are invalid fall back to the roster rather than being
            // created from them. Here there is no `match` to feed and no object to create, so the
            // parse would produce a value with no consumer. What does still run for a lock is
            // everything past this match: the input region, computed from the same resolved tree as
            // any other surface's, and `apply_visibility`, which deliberately does nothing for this
            // role.
            TrackedRole::Lock { .. } => {}
        }
        self.apply_input_region(index, &tree);
        self.apply_visibility(index, tree.visible);
    }

    /// Diffs one surface's freshly resolved `panel` spec against the one its layer-shell state was
    /// last set from and sends only what moved (see [`spec_update`] for which fields, and for why
    /// the topology ones are not among them).
    fn apply_spec_change(&mut self, index: usize, mut fresh: PanelSpec) {
        let TrackedRole::Panel { layer, spec: applied, output_size } = &self.surfaces[index].role else {
            return;
        };
        let update = spec_update(applied, &fresh, *output_size);

        if let Some(margin) = update.margin {
            layer.set_margin(
                margin.top as i32,
                margin.right as i32,
                margin.bottom as i32,
                margin.left as i32,
            );
        }
        if let Some(mode) = update.keyboard_interactivity {
            layer.set_keyboard_interactivity(keyboard_interactivity_for(mode));
        }
        if let Some(size) = update.size {
            // The same guard `create_panel` runs, and it has to run again here rather than only
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
                fresh.width = applied.width;
                fresh.height = applied.height;
            } else {
                layer.set_size(size.0, size.1);
            }
        }

        // Before `apply_exclusive_zone`, which reads `exclusive` and the anchor off it. The borrow
        // of `self.surfaces[index].role` taken at the top ends here, which is why every request
        // above had to be sent first.
        if let TrackedRole::Panel { spec, .. } = &mut self.surfaces[index].role {
            *spec = fresh;
        }
        if update.exclusive.is_some() || update.size.is_some() {
            self.apply_exclusive_zone(index);
        }
    }

    /// Diffs one toplevel's freshly resolved `window` spec against the one its `xdg_toplevel` state
    /// was last set from and sends only what moved (§ 6.2, build-steps.md Phase 22 item 1; see
    /// [`window_update`] for which fields and why all of them qualify).
    ///
    /// Sends nothing while the window is not shown, and stores the spec anyway. That is not a
    /// dropped update: `visible = false` means there is no `xdg_toplevel` to send a request to
    /// (docs/adr/0049 decision 1), and [`App::show_window`] builds the next one out of exactly this
    /// stored spec. So a `title` that changed three times while the window was closed opens with
    /// the third one.
    fn apply_window_change(&mut self, index: usize, fresh: WindowSpec) {
        let TrackedRole::Window { window, spec: applied } = &mut self.surfaces[index].role else {
            return;
        };
        let update = window_update(applied, &fresh);
        *applied = fresh;
        let Some(window) = window.as_ref() else {
            return;
        };
        if let Some(title) = update.title {
            window.set_title(title);
        }
        if let Some(app_id) = update.app_id {
            window.set_app_id(app_id);
        }
        // Minimum before maximum, so the pair the compositor validates at the next commit is never
        // momentarily inverted -- `set_max_size` raises `invalid_size` for a maximum under the
        // minimum, and `node::window_spec` has already refused that pairing in the fresh spec.
        if let Some(min_size) = update.min_size {
            window.set_min_size(size_hint_pair(min_size));
        }
        if let Some(max_size) = update.max_size {
            window.set_max_size(size_hint_pair(max_size));
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
    ///
    /// Skipped for a `window` that is not shown, which is the only role-aware line in it: there is
    /// no `wl_surface` to set a region on, and [`App::show_window`]'s first re-resolve after the
    /// window opens sets one.
    fn apply_input_region(&mut self, index: usize, tree: &layout::ResolvedNode) {
        let Some(surface) = self.surfaces[index].role.wl_surface().cloned() else {
            return;
        };
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
        surface.set_input_region(Some(region.wl_region()));
        // `region` drops here, destroying the `wl_region` -- `wl_surface::set_input_region` copies
        // its contents, so the object has no reason to outlive the request. Same shape the deleted
        // `create_overlay_canvas` used.
    }

    /// Applies § 5.1's `visible` to a live surface, by whichever mechanism the role's lifetime rule
    /// calls for (docs/adr/0038 decision 2, docs/adr/0049 decisions 1-2).
    ///
    /// **The same Lua-facing property, two different mechanics underneath, and this is the one
    /// function where that divergence lives.** A `panel`'s Wayland object outlives every flip, so
    /// `visible` is a map or an unmap commit. A `window`'s exists only while shown, so `visible` is
    /// a create or a destroy. Nothing new drives either: a `state` write marks the scene dirty
    /// (ADR-0044 decision 5), the poll loop re-resolves, and `apply_resolved_state` calls this with
    /// whatever the fresh tree says.
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
        match &self.surfaces[index].role {
            TrackedRole::Panel { .. } => match (self.surfaces[index].map_state, visible) {
                (MapState::Unmapped, true) => self.remap(index),
                (MapState::AwaitingConfigure | MapState::Mapped, false) => self.unmap(index),
                _ => {}
            },
            TrackedRole::Window { .. } => match (self.surfaces[index].map_state, visible) {
                (MapState::Unmapped, true) => {
                    // Cloned because `show_window` takes `&mut self`; a `QueueHandle` is a cheap
                    // refcounted handle, which is why `App` keeps one for exactly this kind of call
                    // from outside a `Dispatch` callback.
                    let qh = self.queue_handle.clone();
                    self.show_window(&qh, index);
                }
                (MapState::AwaitingConfigure | MapState::Mapped, false) => self.hide_window(index),
                _ => {}
            },
            // Its own function rather than a third arm of the same `map_state` match, because a
            // popup answers on two inputs and not one: docs/adr/0051 decision 2's latch is the
            // second, and a dismissed popup sits in `MapState::Unmapped` with `visible` still true
            // -- a state the other two roles never reach.
            TrackedRole::Popup { .. } => self.apply_popup_visibility(index, visible),
            // The one role where `visible` is not a property at all. `layout::node::lock_spec`
            // refuses the key outright, so the `true` this is called with is `parse_visible`'s
            // default and not something the config said. A lock surface's lifetime is the
            // compositor's from end to end -- created once `locked` arrives, destroyed at
            // `unlock_and_destroy` -- and between those two points the protocol requires one on
            // every output, so acting on a `visible` here could only destroy a surface the
            // compositor is still showing and make it fall back to a solid colour (docs/adr/0042,
            // docs/adr/0052 decision 2).
            TrackedRole::Lock { .. } => {}
        }
    }

    /// [`App::apply_visibility`]'s `popup` arm (docs/adr/0049 decision 2, docs/adr/0051 decision 2).
    ///
    /// The decision itself is [`popup_visibility_action`], which is pure and tested; this is the
    /// writes it does not make. The latch clear is one of them, and it is unconditional on the
    /// `visible = false` edge rather than paired with a `Destroy`: a popup the compositor dismissed
    /// is already objectless, so the false edge that reopens its path is exactly the one where
    /// there is nothing left to destroy.
    ///
    /// That edge is kept even though docs/adr/0051's first amendment made it no longer the only way
    /// out of the latch. It is still correct and still the ordinary case: a config whose
    /// `on_dismiss` writes `visible = false` reopens the path on the turn it does so, without
    /// waiting for the pointer count to move.
    fn apply_popup_visibility(&mut self, index: usize, visible: bool) {
        let TrackedRole::Popup { popup, dismissed_at, .. } = &self.surfaces[index].role else {
            return;
        };
        let action = popup_visibility_action(visible, popup.is_some(), *dismissed_at, self.pointer_input_count);
        if !visible && let TrackedRole::Popup { dismissed_at, refusal_logged, .. } = &mut self.surfaces[index].role {
            *dismissed_at = None;
            *refusal_logged = None;
        }
        match action {
            PopupAction::Create => {
                let qh = self.queue_handle.clone();
                self.show_popup(&qh, index);
            }
            PopupAction::Destroy => self.hide_popup(index),
            PopupAction::Nothing => {}
        }
    }

    /// Creates this window's `xdg_toplevel` and performs the initial commit `xdg_surface` requires
    /// (§ 6.2, docs/adr/0040 decisions 4 and 5, docs/adr/0049 decision 1, build-steps.md Phase 22
    /// items 1 and 4).
    ///
    /// The whole sequence is the layer-shell one with a different constructor, which is exactly what
    /// ADR-0040 decision 4 predicted: create the surface, send the role's state, commit with **no
    /// buffer attached**, and wait for the configure before anything may be drawn. `MapState::
    /// AwaitingConfigure` is that wait, shared verbatim with the panel path.
    ///
    /// SCTK does the two things it would be easy to get wrong here. It acks each `xdg_surface.
    /// configure` itself, through the wrapping `xdg_surface` rather than the role object
    /// (`shell/xdg/window/inner.rs`'s `Dispatch2<XdgSurface, _>`), so nothing in this file acks;
    /// and `XdgShell::bind` already picked up `zxdg_decoration_manager_v1` alongside `xdg_wm_base`,
    /// so `WindowDecorations::RequestServer` plus [`Window::request_decoration_mode`] is the whole
    /// of build-steps.md Phase 22 item 4 and there is no second global to bind.
    ///
    /// No `set_window_geometry`: `xdg_surface`'s own default is the bounding box of the surface and
    /// its subsurfaces, this shell draws its content edge to edge with no client-side shadow to
    /// exclude, and there are no subsurfaces. Sending the default back would be ceremony.
    ///
    /// A compositor with no xdg-shell leaves the window unbuilt, logged once per attempt: not
    /// fatal, on the same "keep the shell up" principle every other failure in this file follows --
    /// the panels still paint.
    fn show_window(&mut self, qh: &QueueHandle<App>, index: usize) {
        let Some(xdg_shell) = self.xdg_shell.as_ref() else {
            eprintln!(
                "[oblisk-renderer] {}: this compositor advertises no xdg_wm_base, so no window can be created for it",
                self.surfaces[index].surface_id
            );
            return;
        };
        let TrackedRole::Window { spec, .. } = &self.surfaces[index].role else {
            return;
        };
        let spec = spec.clone();

        let surface = self.compositor_state.create_surface(qh);
        let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, qh);
        // Asked for explicitly as well as through `WindowDecorations::RequestServer`, because the
        // two reach different objects: the constructor argument decides whether a
        // `zxdg_toplevel_decoration_v1` is created at all, and this is the `set_mode` on it. Whatever
        // the compositor answers with is accepted -- `WindowHandler::configure` logs a client-side
        // grant and carries on undecorated rather than faking a frame (ADR-0040 decision 4, § 6.2).
        window.request_decoration_mode(Some(DecorationMode::Server));
        window.set_title(spec.title.clone());
        window.set_app_id(spec.app_id.clone());
        // Advisory, and sent as such: nothing in `layout` clamps the resolved tree against them
        // (§ 6.2, `WindowSpec`'s own note). They do bound the size *this* client picks on a
        // `None` configure axis, which is the one place the choice is ours -- see
        // [`toplevel_size_for`].
        window.set_min_size(size_hint_pair(spec.min_size));
        window.set_max_size(size_hint_pair(spec.max_size));
        // The initial commit `xdg_surface` requires: "the client must perform an initial commit
        // without any buffer attached", after which the compositor replies with the configure that
        // makes attaching one legal.
        window.commit();

        if let TrackedRole::Window { window: slot, .. } = &mut self.surfaces[index].role {
            *slot = Some(window);
        }
        self.surfaces[index].map_state = MapState::AwaitingConfigure;
        eprintln!("[oblisk-renderer] {} creating: visible = true", self.surfaces[index].surface_id);
    }

    /// Destroys this window's `xdg_toplevel` and everything hanging off it, leaving the tracking
    /// entry behind so a later `visible = true` can build a fresh one (docs/adr/0049 decision 1).
    ///
    /// Teardown order is [`App::destroy_surface_by_id`]'s, reused rather than restated: EGL surface
    /// by hand, then the `wl_egl_window`, then the role object. Dropping the [`Window`] handle is
    /// that last step -- `smithay_client_toolkit`'s `WindowInner::drop` destroys the decoration
    /// object, then the `xdg_toplevel`, then the `xdg_surface`, then the `wl_surface`, which is the
    /// order xdg-shell requires and not this function's to re-derive.
    ///
    /// `configured_size` is cleared with the binding (inside `release_bound`), because the next
    /// toplevel gets its own configure and must not paint into a stale one.
    fn hide_window(&mut self, index: usize) {
        // Before anything of this window's own is torn down: a popup rooted under it must not
        // outlive its `xdg_surface`. See [`App::drop_child_popups`] for what that costs when it is
        // skipped.
        self.drop_child_popups(index);
        self.release_bound(index);
        if let TrackedRole::Window { window, .. } = &mut self.surfaces[index].role {
            drop(window.take());
        }
        self.surfaces[index].map_state = MapState::Unmapped;
        self.surfaces[index].null_buffered = false;
        eprintln!("[oblisk-renderer] {} destroyed: visible = false", self.surfaces[index].surface_id);
    }

    /// Creates this popup's `xdg_positioner` and `xdg_popup`, roots it under one parent instance,
    /// takes the grab if § 6.3 asked for one, and performs the initial commit (§ 6.3,
    /// docs/adr/0040 decision 2, docs/adr/0049 decisions 1-2, docs/adr/0051 decisions 1 and 3,
    /// build-steps.md Phase 22 items 2 and 3).
    ///
    /// **The order below is the protocol's and every step of it is load-bearing.** Build the
    /// positioner and set every field, because `get_popup` reads it once and consumes it. Create the
    /// `wl_surface` and the popup with [`Popup::from_surface`], not [`Popup::new`] -- `new` sends the
    /// initial commit for you, which is fatal for a `panel` parent whose rooting request has not
    /// been sent yet ("If you do not specify a parent surface, you must configure the parent using
    /// an alternate function such as `LayerSurface::get_popup` prior to committing the surface, or
    /// you will get an `invalid_popup_parent` protocol error"). Root it. Take the grab, which "must
    /// be requested before the popup is mapped" or the compositor raises `invalid_grab`. Then
    /// commit, and wait for the configure in [`MapState::AwaitingConfigure`] exactly as the other
    /// two roles do.
    ///
    /// `grab = true` with nothing armed refuses to create the popup **at all**, rather than creating
    /// one without its grab (docs/adr/0049's amendment, docs/adr/0051 decision 3). A dropdown that
    /// cannot be dismissed by clicking outside it is worse than one that did not open: the
    /// click-outside dismissal is the whole reason docs/adr/0040 reached for a real `xdg_popup`
    /// instead of a second `panel`. A grab the *compositor* refuses is a different thing and needs
    /// no branch here -- it arrives as an immediate `popup_done` and goes through
    /// [`PopupHandler::done`] like a click-outside, which § 6.3 says to treat as a normal outcome.
    ///
    /// SCTK acks each `xdg_surface.configure` itself before calling [`PopupHandler::configure`]
    /// (`shell/xdg/popup.rs`'s `Dispatch2<XdgSurface, _>`), so nothing here acks. The grab is the one
    /// request it does not wrap, reached through `Popup::xdg_popup()` -- the same raw-object escape
    /// hatch docs/adr/0009 established for `wp-text-input-v3`.
    fn show_popup(&mut self, qh: &QueueHandle<App>, index: usize) {
        let surface_id = self.surfaces[index].surface_id.clone();
        let Some(xdg_shell) = self.xdg_shell.as_ref() else {
            eprintln!("[oblisk-renderer] {surface_id}: this compositor advertises no xdg_wm_base, so no popup can be created for it");
            return;
        };
        let TrackedRole::Popup { spec, .. } = &self.surfaces[index].role else {
            return;
        };
        let spec = spec.clone();

        // Both halves of the grab are decided before anything is created, so a refusal costs no
        // protocol objects and leaves nothing half-built.
        let grab = if spec.grab {
            let Some(armed) = self.input_serial.clone() else {
                if self.refusal_is_new(index, PopupRefusal::Unarmed) {
                    eprintln!(
                        "[oblisk-renderer] {surface_id}: `grab = true` and no input event armed a serial this turn, so it is not opened. \
                         A popup may only be opened in response to real user input (§ 6.3); open it from an `on_click`, or declare `grab = false`. \
                         Logged once until it opens or `visible` resolves false."
                    );
                }
                return;
            };
            let Some(seat) = self.seat_state.seats().next() else {
                if self.refusal_is_new(index, PopupRefusal::Seatless) {
                    eprintln!("[oblisk-renderer] {surface_id}: `grab = true` and this compositor advertises no seat, so it is not opened");
                }
                return;
            };
            Some((seat, armed))
        } else {
            None
        };

        // The armed surface is consulted whether or not a grab was asked for: docs/adr/0051
        // decision 1 is about which *instance* of the declared parent a dropdown belongs to, and a
        // `grab = false` popup opened by a click belongs to the monitor that click landed on just
        // as much as a grabbing one does.
        let parent_index = parent_instance_index(
            self.surfaces.iter().map(|tracked| tracked.surface_id.as_str()),
            &spec.parent,
            self.input_serial.as_ref().map(|armed| armed.instance_id.as_str()),
        );
        let Some(parent) = parent_index.and_then(|parent| self.surfaces[parent].role.as_popup_parent()) else {
            if self.refusal_is_new(index, PopupRefusal::HiddenParent) {
                eprintln!(
                    "[oblisk-renderer] {surface_id}: its `parent` {:?} names no surface that is currently shown, so it is not opened",
                    spec.parent
                );
            }
            return;
        };
        // Logged with the popup, because on a multi-monitor session this is the answer
        // docs/adr/0051 decision 1 exists to give and there is no other way to see which instance
        // won.
        let parent_id = parent_index.map_or("<none>", |parent| self.surfaces[parent].surface_id.as_str()).to_string();

        let positioner = match XdgPositioner::new(xdg_shell) {
            Ok(positioner) => positioner,
            Err(err) => {
                log_bind_failure(&surface_id, "xdg_wm_base::create_positioner", err);
                return;
            }
        };
        configure_positioner(&positioner, &spec);

        let surface = self.compositor_state.create_surface(qh);
        let rooted_at_creation = match &parent {
            PopupParent::Xdg(xdg_surface) => Some(xdg_surface),
            PopupParent::Layer(_) => None,
        };
        let popup = match Popup::from_surface(rooted_at_creation, &positioner, qh, surface, xdg_shell) {
            Ok(popup) => popup,
            Err(err) => {
                log_bind_failure(&surface_id, "xdg_surface::get_popup", err);
                return;
            }
        };
        if let PopupParent::Layer(layer) = &parent {
            // Layer-shell's own rooting request, and the reason `Popup::from_surface` was handed no
            // parent above. Before the commit below, or `invalid_popup_parent`.
            layer.get_popup(popup.xdg_popup());
        }
        if let Some((seat, armed)) = &grab {
            popup.xdg_popup().grab(seat, armed.serial);
        }
        // The initial commit `xdg_surface` requires, and the line every step above had to precede.
        popup.wl_surface().commit();
        // `positioner` drops at the end of this function, which destroys the `xdg_positioner`. That
        // is the protocol's own lifecycle -- `get_popup` has already copied its state -- and
        // `XdgPositioner`'s `Drop` is what sends it.

        if let TrackedRole::Popup { popup: slot, refusal_logged, .. } = &mut self.surfaces[index].role {
            *slot = Some(popup);
            *refusal_logged = None;
        }
        self.surfaces[index].map_state = MapState::AwaitingConfigure;
        eprintln!(
            "[oblisk-renderer] {surface_id} creating: visible = true, anchored to {parent_id}, grab {}",
            if grab.is_some() { "taken" } else { "not requested" }
        );
    }

    /// Destroys this popup's `xdg_popup` and every popup nested under it, leaving the tracking
    /// entries behind so a later `visible = true` can build fresh ones (docs/adr/0049 decision 1).
    ///
    /// **Children first, which is the protocol's requirement and not merely docs/adr/0040's
    /// preference**: `xdg_popup`'s own description makes destroying a parent before its child a
    /// protocol error. [`App::shown_popups_under`] produces exactly that order.
    ///
    /// A nested child is latched on the way down. It never received a `popup_done` of its own --
    /// the engine took its parent away, the compositor did not -- but it is in the same position as
    /// one that did: its object is gone while its own `visible` still says true, and re-creating it
    /// on the next re-resolve would only find its parent missing. The latch clears on its own
    /// `visible = false`, exactly as decision 2 says.
    fn hide_popup(&mut self, index: usize) {
        for child in self.drop_child_popups(index) {
            self.latch_popup(child);
        }
        self.drop_popup_object(index);
    }

    /// Destroys every popup currently rooted under the surface at `index`, deepest first, and
    /// returns them in that order. The surface at `index` is left alone: its own teardown is the
    /// caller's, and the three callers differ in what that is.
    ///
    /// **Every path that destroys a surface owes this call, and the protocol is why.** wlroots
    /// rejects destroying an `xdg_surface` whose popup list is non-empty, which takes the Wayland
    /// connection and the whole shell down with it. [`App::hide_popup`] was the only path that did
    /// it, so a `popup { parent = "settings" }` that was open when the config wrote
    /// `settings_open = false` destroyed its parent's `xdg_toplevel` underneath a live `xdg_popup`;
    /// output removal did the same to a popup parented to a per-output panel. On a compositor that
    /// tolerates it the popup was instead left with an object and `MapState::Mapped`, which
    /// [`popup_visibility_action`] answers `Nothing` to forever.
    ///
    /// The latch is deliberately **not** set here, and only `hide_popup` sets it on what this
    /// returns. A parent going away is not a compositor dismissal: the child's own `visible` never
    /// moved, so the declarative answer is that it reappears the moment its parent does, on the
    /// first re-resolve after that. Latching would make it wait for pointer input instead, which a
    /// parent window reopened by a D-Bus notification never produces. The cost is that each
    /// re-resolve while the parent is away runs a [`App::show_popup`] that refuses at the `parent`
    /// check, which is a handful of branches and one throttled log line ([`PopupRefusal`]).
    fn drop_child_popups(&mut self, index: usize) -> Vec<usize> {
        let mut nested = Vec::new();
        self.shown_popups_under(index, &mut nested);
        for &child in &nested {
            self.drop_popup_object(child);
        }
        nested
    }

    /// One popup's object teardown, with no opinion about the latch or about nesting. Teardown
    /// order is [`App::destroy_surface_by_id`]'s, reused rather than restated: EGL surface by hand,
    /// then the `wl_egl_window`, then the role object -- and dropping the [`Popup`] handle is that
    /// last step, since `PopupInner::drop` is what sends `xdg_popup.destroy`.
    fn drop_popup_object(&mut self, index: usize) {
        self.release_bound(index);
        if let TrackedRole::Popup { popup, .. } = &mut self.surfaces[index].role {
            drop(popup.take());
        }
        self.surfaces[index].map_state = MapState::Unmapped;
        self.surfaces[index].null_buffered = false;
        eprintln!("[oblisk-renderer] {} destroyed", self.surfaces[index].surface_id);
    }

    /// Whether this refusal's line is worth writing, recording it either way: true unless the same
    /// refusal was the last one logged for this popup (docs/adr/0049's amendment, [`PopupRefusal`]).
    ///
    /// A *different* refusal still gets its one line, which is the whole reason the field holds a
    /// reason rather than a flag: a config that fixes its `grab` and then trips over a hidden
    /// `parent` would otherwise be told nothing.
    fn refusal_is_new(&mut self, index: usize, refusal: PopupRefusal) -> bool {
        let TrackedRole::Popup { refusal_logged, .. } = &mut self.surfaces[index].role else {
            return false;
        };
        refusal_logged.replace(refusal) != Some(refusal)
    }

    /// Sets docs/adr/0051 decision 2's latch on one popup, stamped with the pointer-input count it
    /// holds now. A no-op on any other role, which is why it is a method rather than a field write
    /// at each of its call sites.
    ///
    /// The stamp is what the amendment turns on: the latch holds until [`App::pointer_input_count`]
    /// moves past this value, so a dismissal followed by nothing holds forever and a dismissal
    /// followed by a click does not.
    fn latch_popup(&mut self, index: usize) {
        let stamp = self.pointer_input_count;
        if let TrackedRole::Popup { dismissed_at, .. } = &mut self.surfaces[index].role {
            *dismissed_at = Some(stamp);
        }
    }

    /// Every currently shown popup rooted under the surface at `index`, appended deepest-first --
    /// the order [`App::hide_popup`] destroys in.
    ///
    /// Post-order over the parent tree, so a grandchild is appended before its parent and a parent
    /// before `index` itself (which this never appends; the caller owns that). Siblings come out in
    /// tracked order, and that is fine rather than sloppy: xdg-shell constrains a popup against its
    /// *parent*, and two popups under one parent constrain each other not at all.
    ///
    /// Cannot recurse forever, and not because of a depth guard. A popup is only counted here while
    /// it holds an object, and a popup cannot hold one unless its parent held one first
    /// ([`App::show_popup`] refuses otherwise), so a `parent` cycle in a config -- including a popup
    /// naming itself -- has no member that ever opens.
    fn shown_popups_under(&self, index: usize, out: &mut Vec<usize>) {
        let parent_id = self.surfaces[index].surface_id.clone();
        let children: Vec<usize> = (0..self.surfaces.len())
            .filter(|&child| child != index)
            .filter(|&child| {
                matches!(&self.surfaces[child].role, TrackedRole::Popup { popup: Some(_), spec, .. } if is_instance_of(&parent_id, &spec.parent))
            })
            .collect();
        for child in children {
            self.shown_popups_under(child, out);
            out.push(child);
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
        let TrackedRole::Panel { layer, .. } = &self.surfaces[index].role else {
            return;
        };
        layer.wl_surface().attach(None, 0, 0);
        layer.wl_surface().commit();
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
    /// `set_size` needs no [`ambiguous_zero_axis`] guard: the applied spec's size is only ever one
    /// that already passed it, in [`App::create_panel`] or in [`App::apply_spec_change`], which both
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
        let TrackedRole::Panel { layer, spec, output_size } = &self.surfaces[index].role else {
            return;
        };
        layer.set_anchor(anchor_for(spec.topology.anchor));
        layer.set_size(
            layer_extent_for(spec.width, output_size.width),
            layer_extent_for(spec.height, output_size.height),
        );
        layer.set_keyboard_interactivity(keyboard_interactivity_for(spec.keyboard_interactivity));
        layer.set_margin(
            spec.margin.top as i32,
            spec.margin.right as i32,
            spec.margin.bottom as i32,
            spec.margin.left as i32,
        );
        layer.wl_surface().commit();
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
        let Some(surface_object_id) = self.surfaces[index].role.wl_surface().map(Proxy::id) else {
            // A `window` whose `visible` went false between the call that asked for a bind and this
            // one. Not fatal and not an error: there is nothing left to bind, and the caller's
            // `map_state` guard has already stopped it painting.
            return false;
        };

        let native_window = match WlEglSurface::new(surface_object_id, width, height) {
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
                layout::paint::paint_tree(painter, &mut self.image_cache, tree, 1.0);
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
        let staged = candidate_has_staged(
            self.surfaces.iter().map(|s| (s.null_buffered, s.role.wl_surface().is_some())),
        );
        if self.ready_signal_sent || !staged {
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
        if let Some(surface) = self.surfaces[index].role.wl_surface().cloned()
            && let Err(e) = self.presentation_time.feedback(&surface, &self.queue_handle)
        {
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

    /// Every write to `focused_secure_submit` in this file, funnelled so [`retarget_secure_submit`]
    /// gets to enforce the buffer's lifetime. See that function for the leak this closes; assigning
    /// the field directly anywhere else reopens it.
    fn focus_secure_submit(&mut self, next: Option<FocusedField>) {
        retarget_secure_submit(&mut self.focused_secure_submit, &mut self.secure_buffer, next);
    }

    /// Whether `instance_id` is still a surface this process has a live `wl_surface` for.
    ///
    /// `TrackedRole::wl_surface` is the whole test, and it is the right one because it answers
    /// `None` for both shapes a gone surface takes here: the entry removed outright
    /// ([`App::destroy_surface_by_id`]) and the entry kept with its role object dropped
    /// ([`App::hide_window`], [`App::teardown_lock_surfaces`], [`App::drop_popup_object`]).
    fn surface_is_live(&self, instance_id: &str) -> bool {
        self.surfaces.iter().any(|tracked| tracked.surface_id == instance_id && tracked.role.wl_surface().is_some())
    }

    /// Drops the focused field, and the half-typed secret with it, the moment [`focus_is_still_armed`]
    /// stops holding -- through [`App::focus_secure_submit`], so the scrub is the same one every
    /// other transition gets.
    ///
    /// Called before every keystroke, which is what makes the rule load-bearing rather than
    /// advisory: nothing can reach `secure_buffer` through a focus that has gone stale, whatever
    /// took the surface away and whether or not a `leave` ever followed.
    fn prune_secure_focus(&mut self) {
        let armed = self
            .focused_secure_submit
            .as_ref()
            .is_some_and(|field| focus_is_still_armed(field, self.keyboard_focus.as_deref(), self.surface_is_live(&field.surface_id)));
        if self.focused_secure_submit.is_some() && !armed {
            eprintln!("[oblisk-renderer] the focused secure_submit field is no longer the one receiving keys; dropping it and scrubbing its buffer");
            self.focus_secure_submit(None);
        }
    }

    /// The half of [`App::prune_secure_focus`] that does not wait for a keystroke: a field whose
    /// surface this process destroyed is dropped, and its buffer scrubbed, on the next poll turn.
    ///
    /// **Only the liveness clause, deliberately.** `prune_secure_focus`'s other clause is about
    /// *routing* -- which surface is receiving keys -- and it is only ever wrong at the moment a key
    /// arrives, which is where it is asked. Applying it once a turn would also disarm the field a
    /// press on a multi-field surface just chose, in the window before the compositor's matching
    /// `enter` lands, and `sole_secure_submit` cannot re-choose it (see [`focus_on_enter`]).
    ///
    /// What this buys is the residency ceiling defect 3 named: type a password on the lock screen,
    /// let the compositor send `finished`, and `teardown_lock_surfaces` destroys the `wl_surface`
    /// without the protocol requiring any `leave` to follow. Without this the plaintext would sit in
    /// `secure_buffer`, still addressed to `("lock", "authenticate")`, until some later keystroke
    /// happened to notice -- which on a session where the user walks away is never.
    fn drop_secure_focus_if_its_surface_is_gone(&mut self) {
        let gone = self.focused_secure_submit.as_ref().is_some_and(|field| !self.surface_is_live(&field.surface_id));
        if gone {
            eprintln!("[oblisk-renderer] the surface holding the focused secure_submit field is gone; dropping it and scrubbing its buffer");
            self.focus_secure_submit(None);
        }
    }

    /// One key event applied to the focused `secure_submit` field, or nothing at all when no field
    /// is focused (build-steps.md Phase 23 item 3, docs/adr/0005).
    ///
    /// The focus check is the gate, and `focused_secure_submit` is exactly the right one to gate on:
    /// it is `Some` only when some field named a destination for the next secret, so a keystroke
    /// that reaches the buffer already has somewhere to be sent. A `textfield` with no
    /// `secure_submit` leaves it `None` (see [`focused_target`]), and a key arriving then is
    /// dropped rather than accumulated -- there is no destination to address it to, and buffering
    /// a password for a field that can never submit it is a secret held for no reason.
    ///
    /// Nothing here touches Lua. That is the whole of docs/adr/0005: the bytes go from the
    /// `KeyEvent` into a native `shared::SecureBuffer` and out to the Supervisor, and no Lua value
    /// is ever built from them.
    fn apply_secure_key(&mut self, event: &KeyEvent, repeat: bool) {
        // Before the gate, not after it: the gate reads `focused_secure_submit` alone, and a focus
        // whose surface is gone or is no longer the one receiving keys is exactly the state this key
        // must not be appended to (see [`focus_is_still_armed`]).
        self.prune_secure_focus();
        if self.focused_secure_submit.is_none() {
            return;
        }
        match secure_key_action(event, repeat) {
            SecureKeyAction::Append(text) => self.secure_buffer.push_str(text),
            // `pop_char` zeroizes the bytes it drops rather than only shortening the buffer, which
            // is what keeps a corrected character from staying readable in this process's heap for
            // the rest of the entry.
            SecureKeyAction::Backspace => {
                self.secure_buffer.pop_char();
            }
            // Through the seam in both directions rather than reaching for the buffer directly: the
            // scrub Escape wants *is* the one `retarget_secure_submit` performs on a transition, and
            // re-arming the identical field immediately afterwards is what leaves the user still in
            // it, free to retype. A fifth writer of `secure_buffer` with its own idea of what
            // clearing means is what this file has spent two reviews avoiding.
            SecureKeyAction::Clear => {
                let field = self.focused_secure_submit.clone();
                self.focus_secure_submit(None);
                self.focus_secure_submit(field);
            }
            SecureKeyAction::Submit => self.finish_secure_submit(),
            SecureKeyAction::Ignore => {}
        }
    }

    /// A completed `secure_submit`: builds the outgoing frame out of the accumulated buffer and
    /// queues it for the socket thread. [`submit_frame_for`] both performs the one sanctioned read
    /// and leaves `self.secure_buffer` scrubbed and empty on either branch, ready for the next
    /// entry.
    ///
    /// [`submit_frame_for`] refuses on two counts and both end here: no focused destination
    /// (docs/adr/0050 decision 4) and an empty buffer. Logged rather than silent, because a user who
    /// pressed enter deserves an explanation somewhere for why nothing happened, and it is almost
    /// always a `textfield` missing its `secure_submit` table -- the empty case explains itself on
    /// the glass, since there is nothing in the field.
    fn finish_secure_submit(&mut self) {
        let target = self.focused_secure_submit.as_ref().map(|field| &field.target);
        let Some(frame) = submit_frame_for(self.generation_id, target, &mut self.secure_buffer) else {
            eprintln!(
                "[oblisk-renderer] secure_submit dropped: nothing had been typed, or no focused textfield named a capability/action to address it to; the buffer was zeroized and nothing was sent"
            );
            return;
        };
        if let Err(e) = self.outbound_tx.send(frame) {
            eprintln!("[oblisk-renderer] failed to queue SecureSubmit for the socket thread: {e}");
        }
    }

    /// One hit-test of surface `index` at `position`, answering both of [`PointerHit`]'s questions
    /// (docs/adr/0050 decisions 1 and 4).
    ///
    /// `position` is surface-local and *logical*, which is the space `layout::hit` walks
    /// `ResolvedNode::rect` in, so there is no conversion here at all. That holds only while
    /// `paint_surface` paints at scale `1.0` and nothing calls `wl_surface::set_buffer_scale`;
    /// docs/adr/0050's consequences name this as the third caller the HiDPI change from Phase 20
    /// has to move together with `paint_surface` and `apply_input_region`.
    ///
    /// A surface with no resolved tree answers the same as a point that missed everything: no
    /// button, and no focused destination. There is nothing under the pointer either way.
    ///
    /// Owned on the way out, every part. `Scene::surface` clones into a `ResolvedNode` (the same
    /// property `paint_surface` relies on), so the borrow of `self.client` ends on that line, and
    /// both the `Function` and the target are cloned out of the local tree before it is dropped.
    fn hit_under(&self, index: usize, position: (f64, f64)) -> PointerHit {
        let Some(tree) = self.client.scene().surface(&self.surfaces[index].surface_id) else {
            return PointerHit { button: None, focus: Ok(None) };
        };
        let point = layout::hit::LogicalPoint { x: position.0 as f32, y: position.1 as f32 };
        let path = layout::hit::hit_path(&tree, point);
        PointerHit {
            button: clickable_button(&path).map(|(rect, on_click)| (rect, on_click.clone())),
            focus: focused_target(&path),
        }
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
    fn fire_on_click(&mut self, instance_id: &str, rect: LogicalRect, button: &str, on_click: &Function) {
        // Nothing marks the scene dirty here. A handler that changes what is painted does it by
        // writing a `state(name, initial)` signal, and `signal:set()` marks the flag itself
        // (ADR-0044 decision 5); a handler that writes nothing correctly causes no re-resolve.
        if let Err((what, e)) = call_on_click(self.client.lua(), on_click, rect, button) {
            eprintln!("[oblisk-renderer] {instance_id}: {what}: {e}");
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
/// A free function, not a `&mut self` method, for [`retarget_secure_submit`]'s reason: it makes the
/// whole read/zeroize contract directly unit-testable, which nothing involving a live `wl_surface`
/// is.
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

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    /// Pointer and keyboard, and no touch object at all -- § 5.2 has no touch-specific property for
    /// one to serve.
    ///
    /// Idempotent by the `is_none` guards, not by trusting the compositor: `wl_seat::capabilities`
    /// is a full re-statement of the current set on every change, so a seat that gains a keyboard
    /// re-announces its pointer, and SCTK turns each announcement into this call. Creating a
    /// second `wl_pointer` there would leave two objects delivering the same events into one
    /// `armed` slot, and a second `wl_keyboard` two `enter`/`leave` streams into one
    /// `keyboard_focus`.
    fn new_capability(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        match capability {
            Capability::Pointer if self.pointer.is_none() => match self.seat_state.get_pointer(qh, &seat) {
                Ok(pointer) => self.pointer = Some(pointer),
                // Not fatal: a shell with no pointer still paints, still reloads, and still takes
                // `wp-text-input-v3` input. Only `on_click` stops working, which is what this says.
                Err(e) => eprintln!("[oblisk-renderer] wl_seat::get_pointer failed; no button's on_click will ever fire: {e}"),
            },
            // `None` rmlvo: take the compositor's own keymap. This shell never interprets a keysym
            // (there is no `on_key` in § 5.2), so imposing a layout of its own would be policy
            // serving nothing.
            Capability::Keyboard if self.keyboard.is_none() => match self.seat_state.get_keyboard(qh, &seat, None) {
                Ok(keyboard) => self.keyboard = Some(keyboard),
                // Also not fatal, and narrower than it looks: losing this loses the `enter`/`leave`
                // that clear a focused `textfield`, so a stale focus can outlive the user moving on.
                Err(e) => eprintln!("[oblisk-renderer] wl_seat::get_keyboard failed; keyboard focus will never be tracked: {e}"),
            },
            _ => {}
        }
    }

    fn remove_capability(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat, capability: Capability) {
        match capability {
            Capability::Pointer => {
                // A pointer that is gone will never send the `release` this press was waiting for,
                // which is the same reason `leave` clears it (docs/adr/0050 decision 2).
                self.armed = None;
                if let Some(pointer) = self.pointer.take() {
                    // `wl_pointer::release` is `since="3"`; below that the destructor does not exist
                    // and dropping the proxy is the whole cleanup. Same guard SCTK's own
                    // `ThemedPointer::drop` applies (src/seat/pointer/mod.rs:572).
                    if pointer.version() >= 3 {
                        pointer.release();
                    }
                }
            }
            Capability::Keyboard => {
                // No keyboard means nothing will ever report the user leaving, so the focus this
                // was holding is stale from here on (docs/adr/0050 decision 4), and whatever was
                // half-typed into it goes with it ([`App::focus_secure_submit`]).
                self.keyboard_focus = None;
                self.focus_secure_submit(None);
                if let Some(keyboard) = self.keyboard.take() {
                    // `wl_keyboard::release` is `since="3"` too (wayland.xml); same reasoning as
                    // the pointer's guard directly above.
                    if keyboard.version() >= 3 {
                        keyboard.release();
                    }
                }
            }
            _ => {}
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
            let Some(index) = self.index_of_surface(&event.surface) else {
                continue;
            };
            match event.kind {
                // Left, right and middle (docs/adr/0050's second amendment). Any other code is not
                // a button a config can name, so it arms nothing and fires nothing, which is what
                // decision 2's original `BTN_LEFT`-only match did for every code but one.
                PointerEventKind::Press { button, serial, .. } => {
                    if pointer_button_name(button).is_none() {
                        continue;
                    }
                    let instance_id = self.surfaces[index].surface_id.clone();
                    // docs/adr/0049's amendment: armed here, read by this turn's re-resolve if one
                    // creates a popup, cleared by `run`'s poll loop at the end of the turn either
                    // way. See [`ArmedSerial`] for why both edges of a click arm it.
                    self.input_serial = Some(ArmedSerial { serial, instance_id: instance_id.clone() });
                    // docs/adr/0051's first amendment, counted on exactly the events that arm the
                    // serial: the same "the user asked again" fact, kept past the end-of-turn disarm.
                    self.pointer_input_count += 1;
                    let hit = self.hit_under(index, event.position);
                    // The press decides focus, not the release: decision 4 says a press whose path
                    // holds a `textfield` focuses it, and a press that lands anywhere else clears
                    // it. A malformed `secure_submit` is the config's bug, not this shell's, so it
                    // is logged against the surface and treated as no destination -- refusing to
                    // guess a capability is the same call `focused_target` documents.
                    let focus = match hit.focus {
                        Ok(target) => target,
                        Err(e) => {
                            eprintln!("[oblisk-renderer] {instance_id}: textfield has a malformed secure_submit, so it takes focus with no destination: {e}");
                            None
                        }
                    }
                    // Bound to the surface the press landed on, per [`FocusedField`]: a field armed
                    // here stays armed only while that surface is both alive and the one the
                    // compositor is sending keys to.
                    .map(|target| FocusedField { surface_id: instance_id.clone(), target });
                    // Through the seam, because this is the site that *reassigns* rather than
                    // clears: a press moving from one `textfield` to another is the direct A-to-B
                    // transition [`retarget_secure_submit`] exists for.
                    self.focus_secure_submit(focus);
                    self.armed = hit.button.map(|(rect, _)| ArmedClick { instance_id, rect, button });
                }
                PointerEventKind::Release { button, serial, .. } => {
                    let Some(name) = pointer_button_name(button) else {
                        continue;
                    };
                    let instance_id = self.surfaces[index].surface_id.clone();
                    // Overwrites the press's, and that is the point: a click fires on the release
                    // (docs/adr/0050 decision 2), so a popup opened by `on_click` is opened by
                    // *this* event and carries this serial.
                    self.input_serial = Some(ArmedSerial { serial, instance_id: instance_id.clone() });
                    self.pointer_input_count += 1;
                    // Focus is untouched here. The press already decided it, and a release that
                    // drags off a `textfield` must not un-focus the field the user is typing into.
                    let hit = self.hit_under(index, event.position).button;
                    let fires = release_completes_click(self.armed.as_ref(), &instance_id, hit.as_ref().map(|(rect, _)| *rect), button);
                    // Before the call, so a handler that re-enters here cannot find its own press
                    // still armed. See [`release_ends_press`] for why this is not unconditional.
                    if release_ends_press(self.armed.as_ref(), button) {
                        self.armed = None;
                    }
                    if let Some((rect, on_click)) = hit.filter(|_| fires) {
                        self.fire_on_click(&instance_id, rect, name, &on_click);
                    }
                }
                // The pointer left the surface, so the release (if it ever comes) lands somewhere
                // else. This is the drag-off-and-cancel decision 2 is built around.
                PointerEventKind::Leave { .. } => self.armed = None,
                // `Enter`/`Motion`/`Axis`: nothing in § 5.2 reads hover or scroll yet
                // (build-steps.md section 6 ranks both), and a motion that leaves the armed rect
                // deliberately does *not* disarm -- dragging back onto the button and releasing
                // still clicks it, which is what every toolkit does.
                _ => {}
            }
        }
    }
}

/// Which surface the compositor gave keyboard focus to (build-steps.md Phase 21 item 2).
///
/// `keyboard_interactivity` (Phase 20 item 1) is the client's half of this: it tells the compositor
/// whether a surface may be focused at all. `wl_keyboard`'s `enter`/`leave` is the only way the
/// client learns what the compositor decided, which is the whole reason this trait is implemented.
///
/// No `delegate_keyboard!` accompanies it, for the same reason `PointerHandler` has no
/// `delegate_pointer!`: `KeyboardData<D, U>` carries a blanket `Dispatch2<WlKeyboard, D>` impl
/// (src/seat/keyboard/mod.rs:494) that the file-wide `delegate_dispatch2!(App)` at the bottom
/// already turns into the `Dispatch<WlKeyboard, KeyboardData<App, ()>>` half of `get_keyboard`'s
/// bound. Adding the macro would collide with it.
impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
        // `raw`/`keysyms` are the keys already held down when focus arrived. Nothing reads a key
        // here, so they are dropped along with every other key event below.
        //
        // A `wl_keyboard` is per seat, not per surface, so an `enter` can name a surface this
        // process destroyed since the compositor sent it (a `visible` flip, an output change); that
        // is the `None` below and it is not an error.
        self.keyboard_focus = self.surface_id_for(surface).map(str::to_string);
        // `Scene::surface` hands back an owned tree, so the borrow of `self.client` ends on this
        // line and the write below is free to take `&mut self`.
        let tree = self.keyboard_focus.as_ref().and_then(|id| self.client.scene().surface(id));
        // The rule that makes a lock screen typable with no click: keyboard focus on a surface
        // declaring exactly one `secure_submit` field focuses that field (see [`sole_secure_submit`]
        // for why exactly one, and why this is needed at all).
        let next = focus_on_enter(self.keyboard_focus.as_deref(), tree.as_ref(), self.focused_secure_submit.as_ref());
        match (&self.keyboard_focus, &next) {
            (None, _) => eprintln!("[oblisk-renderer] keyboard focus entered an untracked surface; not tracking it"),
            (Some(id), Some(field)) => eprintln!(
                "[oblisk-renderer] {id}: keyboard focus takes its `secure_submit` field ({}/{})",
                field.target.capability, field.target.action
            ),
            (Some(id), None) => eprintln!("[oblisk-renderer] keyboard focus entered {id}, which declares no sole `secure_submit` field"),
        }
        // Unconditional, and that is defect 2. Both "nothing to arm" cases used to be early returns
        // that moved `keyboard_focus` on and left the previous surface's field armed with its
        // half-typed secret, which `apply_secure_key` would then go on appending to and submitting
        // to that surface's capability. A focus that arms nothing has to *disarm*.
        self.focus_secure_submit(next);
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
        // Unconditional, ignoring which surface is named: the protocol orders `leave` on the old
        // surface before `enter` on the new one, so there is no interleaving where clearing here
        // would drop a focus that had already moved on.
        let left = self.keyboard_focus.take().unwrap_or_else(|| "an untracked surface".to_string());
        // docs/adr/0050 decision 4's third clearing source. The user is demonstrably somewhere
        // else, so the `textfield` stops owning the next secret and the armed press will never see
        // its release -- the same answer `PointerEventKind::Leave` gives for the same reason. The
        // scrub that used to live on `zwp_text_input_v3`'s `leave` is now this call's, and it is the
        // load-bearing half: no submit is coming for those bytes.
        self.focus_secure_submit(None);
        self.armed = None;
        eprintln!("[oblisk-renderer] keyboard focus left {left}");
    }

    // A key reaches exactly one place and it is not a config. § 5.2 still declares no key-handler
    // property on any node, and docs/adr/0050's consequences section still says this ADR does not
    // invent one, so a keysym arriving here has nowhere in Lua to go and is not offered one. What it
    // does have is the `secure_submit` field docs/adr/0005 defines as the node whose bytes bypass
    // the VM entirely: [`App::apply_secure_key`] pushes them into a native `shared::SecureBuffer`
    // and out to the Supervisor without a Lua value ever existing. That is engine-internal handling
    // for a field whose whole definition is that the password never enters the Lua VM, so it adds no
    // IDL surface. See [`secure_key_action`] for why this is the keyboard and not text-input.
    fn press_key(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _keyboard: &wl_keyboard::WlKeyboard, _serial: u32, event: KeyEvent) {
        self.apply_secure_key(&event, false);
    }

    fn repeat_key(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _keyboard: &wl_keyboard::WlKeyboard, _serial: u32, event: KeyEvent) {
        self.apply_secure_key(&event, true);
    }

    // Genuinely empty, and the two below with it: a release carries no `utf8` at all (SCTK's own
    // `KeyEvent` doc says so), and neither a modifier latch nor a layout change edits a buffer.
    // They exist because `KeyboardHandler` has no default bodies for them.
    fn release_key(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _keyboard: &wl_keyboard::WlKeyboard, _serial: u32, _event: KeyEvent) {}

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _modifiers: Modifiers,
        _raw_modifiers: RawModifiers,
        _layout: u32,
    ) {
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
    /// Resolves a raw `wl_surface` (as handed back by a `wp_presentation_feedback` callback, a
    /// pointer event, or a keyboard focus event) to the tracked surface that owns it.
    ///
    /// `None` is routine rather than exceptional on every one of those paths: a `wl_pointer`,
    /// a `wl_keyboard` and a feedback object are all per seat or per commit, not per surface, so
    /// any of them can name a surface this process has since destroyed -- through an output change,
    /// or a `visible` flip that took a `window`'s toplevel away (docs/adr/0049 decision 1).
    fn index_of_surface(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.surfaces.iter().position(|s| s.role.wl_surface() == Some(surface))
    }

    /// [`App::index_of_surface`]'s answer as the surface id -- shared by `presented`/`discarded`,
    /// which both used to inline the same lookup independently (Standards review).
    fn surface_id_for(&self, surface: &wl_surface::WlSurface) -> Option<&str> {
        self.index_of_surface(surface).map(|index| self.surfaces[index].surface_id.as_str())
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
        let Some(surface_id) = self.surface_id_for(layer.wl_surface()).map(str::to_string) else {
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
        let Some(index) = self.index_of_surface(layer.wl_surface()) else {
            return;
        };
        let (width, height) = configure.new_size;
        self.bind_and_clear(index, width, height);
    }
}

/// `xdg_toplevel` for the `window` role (build-steps.md Phase 22 items 1 and 4, § 6.2).
///
/// No `delegate_xdg_shell!`/`delegate_xdg_window!` accompanies it, and neither exists in this SCTK
/// to add: `smithay-client-toolkit-0.21.1` ships exactly two `delegate_*` macros
/// (`delegate_dispatch2!` and `delegate_registry!`, checked against `src/`). Every user-data type
/// this role needs -- `WindowData` for the `xdg_surface`, the `xdg_toplevel` and the
/// `zxdg_toplevel_decoration_v1`, and `GlobalData` for `xdg_wm_base`, `xdg_wm_dialog_v1` and
/// `zxdg_decoration_manager_v1` -- carries its own blanket `Dispatch2` impl, which the file-wide
/// `delegate_dispatch2!(App)` at the bottom turns into the `Dispatch` half of `XdgShell::bind`'s and
/// `create_window`'s bounds. The same thing Phase 21 found for `PointerData` and `KeyboardData`;
/// this trait is the only half left to supply.
impl WindowHandler for App {
    /// `xdg_toplevel::close`, which is **a request and not a command**: "The client may choose to
    /// ignore this request", and § 6.2 makes that the config's call rather than the engine's -- the
    /// callback may decline by doing nothing, and the window stays open until the config sets
    /// `visible = false`.
    ///
    /// So this deliberately destroys nothing. Closing on behalf of a config that did not ask would
    /// take the decision away from the one place docs/adr/0049 decision 2 puts it, and would leave
    /// the scene's `visible` saying `true` about a window that no longer exists -- which the next
    /// re-resolve would answer by creating a second one.
    ///
    /// The Lua call has `fire_on_click`'s shape for `fire_on_click`'s reasons: the `Function` is
    /// cloned out of the resolved tree so no borrow of `self.client` is live while Lua runs inside
    /// it, and a raise is logged and swallowed rather than taking down a shell that is otherwise
    /// painting.
    fn request_close(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, window: &Window) {
        let Some(index) = self.index_of_surface(window.wl_surface()) else {
            return;
        };
        let surface_id = self.surfaces[index].surface_id.clone();
        let on_close = self
            .client
            .scene()
            .surface(&surface_id)
            .and_then(|tree| match tree.properties.get("on_close") {
                // § 6.2 leaves the key opaque to `layout::node` exactly as § 5.2 leaves `on_click`,
                // so this is the only place its type is ever checked. Anything that is not a
                // function simply is not a close handler.
                Some(Value::Function(on_close)) => Some(on_close.clone()),
                _ => None,
            });
        let Some(on_close) = on_close else {
            eprintln!("[oblisk-renderer] {surface_id}: the compositor asked it to close and no `on_close` declined or accepted; staying open");
            return;
        };
        if let Err(e) = on_close.call::<()>(()) {
            eprintln!("[oblisk-renderer] {surface_id}: on_close raised, ignoring it: {e}");
        }
    }

    /// One `xdg_surface.configure`, already acked by SCTK before this runs (see
    /// [`App::show_window`]). Everything after the size decision is `bind_and_clear`, shared
    /// verbatim with layer-shell.
    ///
    /// `WindowConfigure` carries three things layer-shell has no analogue for, and this handles
    /// exactly one of them:
    ///
    /// - `new_size`, whose axes are `Option` because a toplevel may be told to pick for itself.
    ///   [`toplevel_size_for`] is that decision.
    /// - `decoration_mode`, logged on the first configure of a mapping when the compositor granted
    ///   client-side decorations. Logged and nothing more: ADR-0040 decision 4 and § 6.2 both refuse
    ///   a client-side titlebar frame, so an undecorated window is the accepted outcome rather than
    ///   a failure. Only the first, because a configure repeats on every resize and the mode
    ///   practically never moves after the initial one.
    /// - `state` (`is_maximized`, `is_fullscreen`, `is_activated`, the tiled set) and
    ///   `capabilities`, deliberately unread. § 5.2 and § 6.2 give a config nothing to bind them to,
    ///   and their one consequence that matters -- a fullscreen or maximized configure is binding --
    ///   already reaches this shell as a `Some` axis of `new_size`, which is taken as given.
    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        let Some(index) = self.index_of_surface(window.wl_surface()) else {
            return;
        };
        let surface_id = self.surfaces[index].surface_id.clone();
        if configure.decoration_mode == DecorationMode::Client && self.surfaces[index].map_state == MapState::AwaitingConfigure {
            eprintln!(
                "[oblisk-renderer] {surface_id}: the compositor granted client-side decorations; carrying on undecorated, since this shell draws no titlebar of its own"
            );
        }
        let TrackedRole::Window { spec, .. } = &self.surfaces[index].role else {
            return;
        };
        let (width, height) = toplevel_size_for(configure.new_size, spec);
        self.bind_and_clear(index, width, height);
    }
}

/// `xdg_popup` for the `popup` role (build-steps.md Phase 22 items 2, 3 and 5; § 6.3).
///
/// No `delegate_xdg_popup!` accompanies it and none exists in this SCTK to add, for
/// [`WindowHandler`]'s reason exactly: `PopupData` carries its own blanket `Dispatch2` impls for
/// both `xdg_surface` and `xdg_popup`, which the file-wide `delegate_dispatch2!(App)` at the bottom
/// turns into the `Dispatch` half of `Popup::from_surface`'s bounds. This trait is the only half
/// left to supply, and it has exactly two methods.
impl PopupHandler for App {
    /// One `xdg_surface.configure`, already acked by SCTK before this runs (see
    /// [`App::show_popup`]). Everything after the size decision is `bind_and_clear`, shared verbatim
    /// with layer-shell and with the toplevel path.
    ///
    /// [`PopupConfigure`] carries two things this deliberately does not read.
    ///
    /// - `position`, the popup's offset from its parent's window geometry. The compositor places a
    ///   popup; the client neither needs nor may act on where it landed, and nothing in § 6.3 gives
    ///   a config anything to bind it to.
    /// - `kind`, which is `Initial` on every configure this shell will ever see. The other two
    ///   variants are `Reactive` (needs `xdg_positioner::set_reactive`, which
    ///   [`configure_positioner`] does not send) and `Reposition` (needs `xdg_popup.reposition`),
    ///   and build-steps.md Phase 22 defers both by name -- a fresh popup per open covers a dropdown
    ///   that opens under different buttons, and only an anchor that moves *while* a popup is open
    ///   needs either.
    fn configure(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, popup: &Popup, configure: PopupConfigure) {
        let Some(index) = self.index_of_surface(popup.wl_surface()) else {
            return;
        };
        let TrackedRole::Popup { spec, .. } = &self.surfaces[index].role else {
            return;
        };
        let (width, height) = popup_size_for((configure.width, configure.height), spec);
        self.bind_and_clear(index, width, height);
    }

    /// `xdg_popup.popup_done`, which is **not a request** and is the whole reason docs/adr/0040
    /// reached for a real `xdg_popup` instead of a second `panel`: the compositor dismissing the
    /// popup on click-outside, which layer-shell has no compositor-agnostic way to do.
    ///
    /// Three things happen, and the order matters. The object is destroyed, children first
    /// ([`App::hide_popup`]). docs/adr/0051 decision 2's latch is set, so no replacement appears
    /// unasked. Then § 6.3's `on_dismiss` fires, into a config that finds the popup already gone --
    /// which is the honest state, since it *is* gone.
    ///
    /// **Deliberately not [`WindowHandler::request_close`]'s rule, and docs/adr/0051 decision 2 says
    /// why.** `close` is a request the client may ignore, so that path destroys nothing and lets the
    /// config decide. `popup_done` is not a request: the object is already gone and the only
    /// question left is whether a replacement appears. The engine must not trust the config to
    /// answer it -- a config with no `on_dismiss` at all is not a config error, and without the
    /// latch it would be a livelock, with each re-resolve creating a popup for the same
    /// click-outside to dismiss.
    ///
    /// A grab the compositor *denied* arrives here too, immediately after `show_popup` asked for
    /// one, and needs no branch: § 6.3 calls that a normal outcome and nothing on this side of the
    /// wire distinguishes it from a click-outside (docs/adr/0051 decision 3).
    ///
    /// The Lua call has [`App::fire_on_click`]'s shape for its reasons: the `Function` is cloned out
    /// of the resolved tree so no borrow of `self.client` is live while Lua runs inside it, and a
    /// raise is logged and swallowed rather than taking down a shell that is otherwise painting.
    fn done(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, popup: &Popup) {
        let Some(index) = self.index_of_surface(popup.wl_surface()) else {
            return;
        };
        let surface_id = self.surfaces[index].surface_id.clone();
        eprintln!("[oblisk-renderer] {surface_id}: dismissed by the compositor");
        self.hide_popup(index);
        self.latch_popup(index);

        let on_dismiss = self
            .client
            .scene()
            .surface(&surface_id)
            .and_then(|tree| match tree.properties.get("on_dismiss") {
                // § 6.3 leaves the key opaque to `layout::node` exactly as § 5.2 leaves `on_click`
                // and § 6.2 leaves `on_close`, so this is the only place its type is ever checked.
                Some(Value::Function(on_dismiss)) => Some(on_dismiss.clone()),
                _ => None,
            });
        let Some(on_dismiss) = on_dismiss else {
            // Not a warning. A config with no `on_dismiss` is a config that does not care why the
            // popup closed, and the latch is what makes that safe rather than a livelock.
            return;
        };
        if let Err(e) = on_dismiss.call::<()>(()) {
            eprintln!("[oblisk-renderer] {surface_id}: on_dismiss raised, ignoring it: {e}");
        }
    }
}

/// `ext_session_lock_v1` for the session lock (build-steps.md Phase 23, docs/adr/0042,
/// docs/adr/0052).
///
/// No `delegate_session_lock!` accompanies it and none exists in this SCTK to add, for
/// [`WindowHandler`]'s reason exactly: `GlobalData`, `SessionLockData` and `SessionLockSurfaceData`
/// each carry their own blanket `Dispatch2` impl covering `ext_session_lock_manager_v1`,
/// `ext_session_lock_v1` and `ext_session_lock_surface_v1`, which the file-wide
/// `delegate_dispatch2!(App)` at the bottom turns into the `Dispatch` half of `SessionLockState::
/// new`'s, `lock`'s and `create_lock_surface`'s bounds the moment this trait is implemented. This
/// trait is the only half left to supply, and it has exactly three methods.
///
/// `SessionLockState` is also absent from `registry_handlers![OutputState, SeatState]`, and that is
/// correct rather than forgotten: it is not a `RegistryHandler`. It binds from the `GlobalList` once
/// in [`run`] and its `GlobalProxy` carries the "not advertised" case for [`App::set_session_lock`]
/// to report.
impl SessionLockHandler for App {
    /// The compositor granted the lock: the session is now locked, every other client's content is
    /// hidden, and this process is responsible for what is on screen until it unlocks
    /// (docs/adr/0042).
    ///
    /// The surface creation here is normally a no-op, and that is deliberate.
    /// [`App::set_session_lock`] already created one per output the moment `lock` succeeded, because
    /// the protocol asks clients to create them immediately and lets the compositor wait for them
    /// before sending this event, specifically so the user does not see a blank frame first. What
    /// this call catches is an output advertised inside that window, which
    /// [`App::ensure_lock_surfaces`] handles idempotently rather than by a second code path.
    ///
    /// The lock handle is re-stored rather than compared against the one `lock` returned. It is the
    /// same `Arc`, SCTK's dispatch flipped `is_locked()` on it before calling in here, and storing
    /// it costs a refcount bump while removing the only way the two could ever disagree.
    fn locked(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, session_lock: SessionLock) {
        self.session_lock = Some(session_lock);
        self.ensure_lock_surfaces(qh);
        let surfaces = self.surfaces.iter().filter(|tracked| matches!(tracked.role, TrackedRole::Lock { surface: Some(_), .. })).count();
        eprintln!("[oblisk-renderer] the session is locked; {surfaces} lock surface(s) up");
        self.report_lock(LockOutcome::Locked);
    }

    /// **Two different events**, told apart by [`finished_outcome`] and never swallowed
    /// (build-steps.md Phase 23 item 2). Arriving before any `locked`, the compositor denied the
    /// request. Arriving after one, it ended a lock that was really up, through its own secure
    /// mechanism.
    ///
    /// Both set `rescue`, per docs/adr/0052 decision 4, and the test that puts them there is not
    /// severity but whether there is a lock screen left to read a message on. There is not: a denial
    /// never put one up, and a teardown took the one that was up away, so in both cases the ordinary
    /// scene is what the user is looking at and `rescue` is what the ordinary scene renders.
    ///
    /// **Which teardown verb to send is decided by `is_locked()`, and the protocol leaves no
    /// choice.** `ext-session-lock-v1` says of `finished`: "the client should make either the
    /// destroy request or the unlock_and_destroy request, depending on whether or not the locked
    /// event was received on this object", and of `ext_session_lock_v1.destroy`: "it is a protocol
    /// error to make this request if the locked event was sent, the unlock_and_destroy request must
    /// be used instead". That is unconditional, so a post-`locked` `finished` answered with a plain
    /// `destroy` is an `invalid_destroy` every time, and losing the connection here is the worst of
    /// the available outcomes: the compositor has already decided the lock is over, so the session
    /// ends up unlocked *and* the shell is dead, with the `rescue` message set two lines below
    /// never reaching a surface.
    ///
    /// This is not the convenience path docs/adr/0042 forbids. That rule is about *initiating* an
    /// unlock, and the compositor initiated this one through its own secure mechanism -- `finished`
    /// is documented as "the compositor has decided that the session lock should be destroyed".
    /// `unlock_and_destroy` here is the cleanup verb for a lock that is already over, not a way to
    /// end one that is still up. The one path that ends a live lock is still
    /// [`App::release_session_lock`], reached only from a `SetSessionLock { locked: false }`.
    ///
    /// Both verbs are `type="destructor"`, and `wayland-backend` refuses a request on an
    /// already-destroyed object client-side rather than putting it on the wire, so
    /// `SessionLockInner::Drop`'s unconditional `destroy` after this `unlock` is a no-op rather than
    /// a second teardown. SCTK's own `SessionLock::unlock` is written against that same guarantee.
    fn finished(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, session_lock: SessionLock) {
        let outcome = finished_outcome(session_lock.is_locked());
        // Ahead of dropping the lock object, unlike the ordered-unlock path: there is no
        // fall-back-to-solid-colour window to worry about here, because the compositor has already
        // decided the lock is over, and the surfaces are the thing whose `wl_surface`s the EGL side
        // still points at.
        self.teardown_lock_surfaces();
        if session_lock.is_locked() {
            // `unlock_and_destroy`, which is the only verb the protocol accepts once `locked` has
            // been sent. See this method's doc comment: this ends an object, not a session.
            session_lock.unlock();
        }
        // For a denial (no `locked`), `SessionLockInner::Drop`'s plain `destroy` is the correct
        // verb and this is what sends it.
        self.session_lock = None;
        // Whichever of the two events this was, no lock is held now -- disarmed for
        // [`App::release_session_lock`]'s reason.
        self.client.set_session_locked(false);
        let reason = match &outcome {
            LockOutcome::Finished => LOCK_TORN_DOWN,
            _ => LOCK_DENIED,
        };
        eprintln!("[oblisk-renderer] the session lock ended: {reason}");
        self.client.set_rescue_state(true, reason);
        self.report_lock(outcome);
    }

    /// One `ext_session_lock_surface_v1.configure`, already acked by SCTK's own `Dispatch2` before
    /// this runs -- so nothing here acks, exactly as nothing in the `window` and `popup` paths acks
    /// their `xdg_surface`.
    ///
    /// Everything after the lookup is [`App::bind_and_clear`], shared verbatim with the other three
    /// roles. The size is taken as given with no [`toplevel_size_for`]-style negotiation, and there
    /// is nothing to negotiate: a lock surface covers its output, the compositor knows that output's
    /// size, and committing a buffer that does not match the acked size is the protocol's own
    /// `dimensions_mismatch` error.
    ///
    /// This is also the event that maps the surface. `ensure_lock_surfaces` left it in
    /// `MapState::AwaitingConfigure` and performed no initial commit, because
    /// `ext_session_lock_surface_v1` forbids one before the first ack; `bind_and_clear` flips that
    /// to `Mapped`, binds EGL, paints the resolved tree, and the `eglSwapBuffers` is the commit that
    /// carries the first buffer -- which is precisely what the protocol asks for in response to a
    /// configure.
    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(index) = self.index_of_surface(surface.wl_surface()) else {
            return;
        };
        let (width, height) = configure.new_size;
        self.bind_and_clear(index, width, height);
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
    fn a_supervisor_that_vanished_while_the_lock_was_up_reports_a_locked_session_and_not_a_lock_screen() {
        // docs/adr/0059 decision 2, and it is the trap docs/adr/0058 decision 4 already named once:
        // `lock_stays_authenticatable`'s refusal ends with "the lock screen that is on screen still
        // stands", which is true of a refused reload and false of this. This process is about to
        // exit, so what is left on the glass is the compositor's own fallback, and the message must
        // send the reader to a VT rather than to a password field that no longer exists.
        let report = supervisor_gone_report(true);
        assert!(report.contains("VT"), "the locked report must name the only way back in: {report}");
        assert!(!report.contains("still stands"), "nothing this process painted is still on screen: {report}");

        // And it must not read as an unlock. Exiting while holding the lock is what keeps the
        // session secure (SCTK's own `SessionLockInner::Drop` comment calls the same choice
        // "failing secure"), so a message saying the session was unlocked would be a lie about the
        // one fact a reader of this line most needs.
        assert!(!report.contains("unlocked"), "the exit does not unlock: {report}");
    }

    #[test]
    fn a_supervisor_that_vanished_with_no_lock_up_says_nothing_about_locks() {
        // The two cases cost different things and read differently. A Renderer that dies unlocked
        // costs a bar, which is the same split docs/adr/0058 decision 2 makes on the other side of
        // the boundary.
        let report = supervisor_gone_report(false);
        assert!(!report.contains("VT"), "no lock was up, so a VT switch is not the story: {report}");
        assert_ne!(report, supervisor_gone_report(true));
    }

    #[test]
    fn a_lock_is_refused_when_the_config_declares_no_lock_surface() {
        // docs/adr/0052 decision 3, and the refusal has to happen before `SessionLockState::lock` is
        // called: a lock that was granted and then painted nothing is a black screen with no
        // password field, and the protocol guarantees the compositor will not unlock on client
        // death, so the only way out would be a VT switch.
        assert_eq!(lock_command(true, false, false, false), LockCommand::Refuse(NO_LOCK_DECLARED));
        assert_eq!(lock_command(true, true, true, false), LockCommand::Acquire);
    }

    #[test]
    fn a_lock_screen_with_no_password_field_is_refused_as_loudly_as_no_lock_screen_at_all() {
        // `lock_spec` requires only an `id` and makes `child` optional, so `lock { id = "x" }` is a
        // legal declaration that resolves to an empty tree: a transparent buffer, an empty input
        // region, and nothing to type into. Granting the lock for it reaches docs/adr/0052 decision
        // 3's black screen *through* the guard instead of around it, so the tracked-surface test is
        // not enough on its own -- the tree has to hold a field that can actually reach PAM.
        assert_eq!(lock_command(true, true, false, false), LockCommand::Refuse(LOCK_CANNOT_AUTHENTICATE));
        // And the two refusals stay distinct: "you declared no lock screen" and "your lock screen
        // has no password field" are different edits to make to a config.
        assert_ne!(NO_LOCK_DECLARED, LOCK_CANNOT_AUTHENTICATE);
    }

    #[test]
    fn a_repeated_lock_or_unlock_command_touches_no_protocol_object() {
        // `locked = true` while already holding one would be a second `ext_session_lock_v1`, which
        // the compositor answers with an immediate `finished` on the new object -- reported as a
        // denial of a lock this process already has. `locked = false` while holding none would be
        // `unlock_and_destroy` on nothing, which is the protocol's `invalid_unlock` error.
        assert_eq!(lock_command(true, true, true, true), LockCommand::Nothing);
        assert_eq!(lock_command(false, true, true, false), LockCommand::Nothing);
        assert_eq!(lock_command(false, false, false, false), LockCommand::Nothing);
    }

    #[test]
    fn only_a_locked_false_command_against_a_held_lock_releases() {
        // The single `Release` in the whole table, and docs/adr/0042 is why it is worth a test of
        // its own: `unlock_and_destroy` has exactly one reachable caller in this process, and the
        // Supervisor sends the command that reaches it only on a `PamOutcome::Success`.
        assert_eq!(lock_command(false, true, true, true), LockCommand::Release);
        assert_eq!(lock_command(false, false, false, true), LockCommand::Release);
    }

    #[test]
    fn releasing_a_lock_the_compositor_never_granted_does_not_report_it_as_unlocked() {
        // The Supervisor's `lock::apply` moves its `active` flag on these reports alone, so an
        // `Unlocked` for a lock that was never `locked` tells it a transition happened that did
        // not: SCTK's `SessionLock::unlock` is a no-op below `is_locked()`, so nothing was
        // released and the session was never secured in the first place.
        assert_eq!(release_outcome(true), LockOutcome::Unlocked);
        assert_eq!(release_outcome(false), LockOutcome::Refused(LOCK_NEVER_GRANTED.to_string()));
    }

    #[test]
    fn finished_before_a_locked_is_a_denial_and_finished_after_one_is_a_teardown() {
        // build-steps.md Phase 23 item 2: one event, two meanings, neither of which may be
        // swallowed. A denial is a failure the user has to see; a teardown is a state change they
        // already lived through, and the Supervisor routes the two differently.
        assert_eq!(finished_outcome(false), LockOutcome::Refused(LOCK_DENIED.to_string()));
        assert_eq!(finished_outcome(true), LockOutcome::Finished);
    }

    #[test]
    fn a_declared_but_unlocked_lock_instance_neither_hangs_nor_joins_the_pba_ready_set() {
        // The bug the popup slice hit, asked of the fourth role. A `lock` instance owns zero Wayland
        // objects until `locked` arrives, so it reaches both PBA gates as `(null_buffered: false,
        // exists: false)` and `MapState::Unmapped` -- complete by construction for the staging gate
        // (nothing to stage), absent from the announced set (it will present no frame). Getting
        // either wrong is a `ready_timeout` hang or an `UnexpectedEvidence` abort, and the two
        // answers have to agree.
        assert!(candidate_has_staged([(false, false)].into_iter()));
        assert!(presenting_surface_ids([("screen-lock@eDP-1", MapState::Unmapped)].into_iter()).is_empty());
    }

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
    fn a_candidate_stages_when_every_surface_that_has_a_wayland_object_has_null_buffered() {
        // The `all(null_buffered)` gate this replaced was correct while every tracked surface was a
        // panel, because a panel always gets a configure -- it is created and initially committed
        // at startup even when `visible` is false.
        assert!(candidate_has_staged([(true, true), (true, true)].into_iter()));
        assert!(!candidate_has_staged([(true, true), (false, true)].into_iter()));
    }

    #[test]
    fn a_window_declared_invisible_has_nothing_to_stage_and_must_not_hold_the_ready_signal() {
        // docs/adr/0049 decision 1 creates no `xdg_toplevel` for it, so no configure is coming and
        // `null_buffered` would stay false forever. Under the old gate that is a `ready_timeout`
        // hang on every config declaring a hidden window, which `dev-config/oblisk/shell.lua`
        // already does.
        assert!(candidate_has_staged([("bar", true, true), ("settings", false, false)].into_iter().map(|(_, n, e)| (n, e))));
        assert!(candidate_has_staged([(false, false)].into_iter()), "a surface with no object at all is complete by construction");
    }

    fn settings_window() -> WindowSpec {
        WindowSpec {
            id: "settings".to_string(),
            title: "Oblisk settings".to_string(),
            app_id: "oblisk.settings".to_string(),
            min_size: None,
            max_size: None,
        }
    }

    fn nz(n: u32) -> Option<std::num::NonZeroU32> {
        std::num::NonZeroU32::new(n)
    }

    #[test]
    fn a_configured_toplevel_axis_is_the_compositors_and_is_taken_as_given() {
        // A tiling compositor sizes every window, and `xdg_toplevel::configure`'s own wording makes
        // a maximized or fullscreen size binding rather than advisory. On niri this is the only
        // branch that ever runs.
        let mut spec = settings_window();
        spec.min_size = Some(SizeHint { width: 320.0, height: 240.0 });
        spec.max_size = Some(SizeHint { width: 1280.0, height: 800.0 });
        assert_eq!(toplevel_size_for((nz(1920), nz(1168)), &spec), (1920, 1168), "the hints never override a configure");
    }

    #[test]
    fn an_unconfigured_toplevel_axis_takes_the_min_size_the_config_declared() {
        // "If this value is None, you may set the size of the window as you wish", which is the
        // ordinary first configure on a floating compositor. `min_size` is the only thing § 6.2
        // lets a config say about a window's size, so it is what the client says back.
        let mut spec = settings_window();
        spec.min_size = Some(SizeHint { width: 320.0, height: 240.0 });
        assert_eq!(toplevel_size_for((None, None), &spec), (320, 240));
        // One axis each way, which is the shape a compositor constraining only width produces.
        assert_eq!(toplevel_size_for((nz(800), None), &spec), (800, 240));
    }

    #[test]
    fn an_unconfigured_axis_with_no_min_size_falls_back_to_the_named_constant() {
        // Its `ponytail:` states the ceiling: § 6.2 gives a config nothing else to say here, and a
        // toplevel's root is forced to the surface, so no content size exists to prefer instead.
        assert_eq!(toplevel_size_for((None, None), &settings_window()), (640, 480));
    }

    #[test]
    fn the_size_this_client_picks_stays_under_the_max_size_the_config_declared() {
        let mut spec = settings_window();
        spec.min_size = Some(SizeHint { width: 900.0, height: 900.0 });
        spec.max_size = Some(SizeHint { width: 400.0, height: 0.0 });
        // A zero `max_size` axis is not a maximum of zero: `set_max_size`'s own "0 means no
        // expected maximum size in the given dimension", the same reading `node::window_spec`
        // applies when it refuses a maximum below a minimum.
        assert_eq!(toplevel_size_for((None, None), &spec), (400, 900));
    }

    #[test]
    fn a_re_resolve_that_changed_no_window_property_sends_no_requests_at_all() {
        let applied = settings_window();
        assert_eq!(window_update(&applied, &applied.clone()), WindowUpdate::default());
    }

    #[test]
    fn every_window_field_is_pushed_on_its_own_and_only_when_it_moved() {
        // All four, unlike a panel's diff: `xdg-shell.xml` says a `set_app_id` "can be sent after
        // the xdg_toplevel has been mapped to update the property", `set_title` is the same shape,
        // and both size hints are ordinary double-buffered requests. Nothing here is fixed at
        // creation the way a layer surface's namespace is, so a changed `title` is an in-place
        // update rather than a recreate.
        let applied = settings_window();

        let mut renamed = applied.clone();
        renamed.title = "Settings".to_string();
        assert_eq!(
            window_update(&applied, &renamed),
            WindowUpdate { title: Some("Settings".to_string()), ..WindowUpdate::default() }
        );

        let mut rematched = applied.clone();
        rematched.app_id = "oblisk.prefs".to_string();
        assert_eq!(
            window_update(&applied, &rematched),
            WindowUpdate { app_id: Some("oblisk.prefs".to_string()), ..WindowUpdate::default() }
        );

        let mut bounded = applied.clone();
        bounded.min_size = Some(SizeHint { width: 320.0, height: 240.0 });
        assert_eq!(
            window_update(&applied, &bounded),
            WindowUpdate { min_size: Some(Some(SizeHint { width: 320.0, height: 240.0 })), ..WindowUpdate::default() }
        );
    }

    #[test]
    fn a_size_hint_that_moved_to_absent_is_still_a_change_that_has_to_reach_the_wire() {
        // The reason the field is `Option<Option<_>>`: the outer layer is "did it move", the inner
        // one is § 6.2's absent-versus-present, and dropping a `max_size` from a config has to send
        // the protocol's zero (meaning unset) rather than leaving the old maximum standing.
        let mut applied = settings_window();
        applied.max_size = Some(SizeHint { width: 1280.0, height: 800.0 });
        let fresh = settings_window();

        assert_eq!(window_update(&applied, &fresh), WindowUpdate { max_size: Some(None), ..WindowUpdate::default() });
        assert_eq!(size_hint_pair(None), None, "which `Window::set_max_size` sends as the protocol's zero");
        assert_eq!(size_hint_pair(Some(SizeHint { width: 320.0, height: 240.0 })), Some((320, 240)));
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
        let armed = ArmedClick { instance_id: "bar@eDP-1".to_string(), rect, button: BTN_LEFT };

        assert!(release_completes_click(Some(&armed), "bar@eDP-1", Some(rect), BTN_LEFT));
        // Dragged off the button, then released: the release hits no button at all.
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", None, BTN_LEFT));
        // Dragged onto a different button on the same surface.
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", Some(moved), BTN_LEFT));
        // Same button geometry, different surface -- two panels can resolve identical rects.
        assert!(!release_completes_click(Some(&armed), "notification_area@eDP-1", Some(rect), BTN_LEFT));
        // A release with nothing armed (a press that hit no button, or a `leave` in between).
        assert!(!release_completes_click(None, "bar@eDP-1", Some(rect), BTN_LEFT));
    }

    #[test]
    fn only_the_three_buttons_a_config_can_name_are_handled_at_all() {
        assert_eq!(pointer_button_name(BTN_LEFT), Some("left"));
        assert_eq!(pointer_button_name(BTN_RIGHT), Some("right"));
        assert_eq!(pointer_button_name(BTN_MIDDLE), Some("middle"));
        // A side button, a browser-back button, and a code off the end of the mouse range. Each
        // answers `None`, which is what stops the press arming: a config cannot tell these apart,
        // so firing `on_click` for one would run a handler written for a button the user did not
        // press.
        assert_eq!(pointer_button_name(0x113), None);
        assert_eq!(pointer_button_name(0x116), None);
        assert_eq!(pointer_button_name(0), None);
    }

    #[test]
    fn a_release_ends_only_its_own_buttons_press() {
        // The bug this exists to stop: press left, press right, release left, release right, all
        // on one node. If the left release clears the slot, the right press is thrown away with
        // it and the right click never fires despite being a complete pair.
        let rect = LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 };
        let armed = ArmedClick { instance_id: "bar@eDP-1".to_string(), rect, button: BTN_RIGHT };

        assert!(release_ends_press(Some(&armed), BTN_RIGHT));
        assert!(!release_ends_press(Some(&armed), BTN_LEFT));
        // Ending it does not depend on it having fired: dragging off the node and releasing the
        // same button is over too, and the slot has to go.
        assert!(!release_ends_press(None, BTN_RIGHT));
    }

    #[test]
    fn a_release_fires_only_for_the_button_the_press_armed() {
        // Press right, release left, on the same node: two different clicks interleaved, and
        // neither completed. A mouse can hold more than one button down at a time, so this is a
        // real sequence rather than a hypothetical one.
        let rect = LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 };
        let armed = ArmedClick { instance_id: "bar@eDP-1".to_string(), rect, button: BTN_RIGHT };

        assert!(release_completes_click(Some(&armed), "bar@eDP-1", Some(rect), BTN_RIGHT));
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", Some(rect), BTN_LEFT));
        assert!(!release_completes_click(Some(&armed), "bar@eDP-1", Some(rect), BTN_MIDDLE));
    }

    /// One `secure_submit` destination, as the parsers hand it back.
    fn target(capability: &str, action: &str) -> node::SecureSubmitTarget {
        node::SecureSubmitTarget { capability: capability.to_string(), action: action.to_string() }
    }

    /// A focused field as [`App::focus_secure_submit`] stores one: a destination *and* the instance
    /// id of the surface it was declared on.
    fn field(surface_id: &str, capability: &str, action: &str) -> FocusedField {
        FocusedField { surface_id: surface_id.to_string(), target: target(capability, action) }
    }

    /// A `textfield` node carrying whatever the config wrote under `secure_submit`; `None` writes
    /// nothing, which is § 5.2 item 8's "optional even on a masked field".
    fn textfield(lua: &Lua, secure_submit: Option<Value>) -> layout::ResolvedNode {
        let mut node = hit_node(lua, "textfield", (0.0, 0.0, 40.0, 24.0), false);
        if let Some(value) = secure_submit {
            node.properties.insert("secure_submit".to_string(), value);
        }
        node
    }

    fn secure_submit_table(lua: &Lua, capability: &str, action: &str) -> Value {
        let table = lua.create_table().unwrap();
        table.set("capability", capability).unwrap();
        table.set("action", action).unwrap();
        Value::Table(table)
    }

    #[test]
    fn a_press_landing_on_no_textfield_leaves_no_destination_focused() {
        let lua = Lua::new();
        let button = hit_node(&lua, "button", (0.0, 0.0, 40.0, 24.0), true);
        let root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        assert_eq!(focused_target(&[&root, &button]).unwrap(), None);
    }

    #[test]
    fn the_innermost_textfield_on_the_path_is_the_one_that_owns_the_next_secret() {
        // Same deep-end scan `clickable_button` makes, and for the same reason (docs/adr/0050
        // decision 1): one traversal, two questions.
        let lua = Lua::new();
        let outer = textfield(&lua, Some(secure_submit_table(&lua, "outer", "ignored")));
        let inner = textfield(&lua, Some(secure_submit_table(&lua, "polkit", "authenticate")));
        let root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);

        assert_eq!(
            focused_target(&[&root, &outer, &inner]).unwrap(),
            Some(node::SecureSubmitTarget { capability: "polkit".to_string(), action: "authenticate".to_string() })
        );
    }

    #[test]
    fn a_textfield_with_no_secure_submit_focuses_with_no_destination() {
        let lua = Lua::new();
        let field = textfield(&lua, None);
        let root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        assert_eq!(focused_target(&[&root, &field]).unwrap(), None);
    }

    #[test]
    fn moving_focus_between_two_secure_submit_fields_zeroizes_what_the_first_accumulated() {
        // The credential leak this seam exists to close, and the one transition three separate
        // call sites used to get wrong: the lock screen's `("lock", "authenticate")` field
        // accumulates a login password, focus moves to the bar's `("network", "connect")` field
        // without an Enter in between, and the next submit carried `<login password><psk>` to the
        // network capability. Nothing may survive a change of destination.
        let mut focused = Some(field("screen@DP-1", "lock", "authenticate"));
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("hunter2");

        retarget_secure_submit(&mut focused, &mut buffer, Some(field("bar@DP-1", "network", "connect")));

        assert_eq!(focused, Some(field("bar@DP-1", "network", "connect")));
        assert!(buffer.is_empty(), "a password typed for one destination must not reach the next one's capability");
    }

    #[test]
    fn the_same_destination_on_a_different_surface_is_a_different_field() {
        // The surface half of the identity, and it is load-bearing rather than decorative. Two
        // surfaces may perfectly well both declare `("lock", "authenticate")` -- the lock screen on
        // each of two monitors does, since one declaration expands to one instance per output. With
        // the destination alone as the identity, focus moving between them compared equal and the
        // scrub was skipped, so the entry begun on one output carried on into the other.
        let mut focused = Some(field("screen@eDP-1", "lock", "authenticate"));
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("hunter2");

        retarget_secure_submit(&mut focused, &mut buffer, Some(field("screen@DP-1", "lock", "authenticate")));

        assert!(buffer.is_empty(), "a field is its surface as well as its destination");
    }

    #[test]
    fn clearing_focus_zeroizes_the_buffer_and_re_focusing_the_same_field_does_not() {
        // Two halves of the same rule. Clearing is `leave`/`capability_lost`, where no submit is
        // ever coming for the bytes, so they must not sit in `App` waiting for the next `enter` to
        // arm a destination for them. Re-arming the *same* destination is a press landing in the
        // field the user is already typing into (docs/adr/0050 decision 4 makes the press decide
        // focus unconditionally), and wiping there would delete half a password mid-entry.
        let mut focused = Some(field("screen@TEST", "lock", "authenticate"));
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("hunter2");

        retarget_secure_submit(&mut focused, &mut buffer, None);
        assert_eq!(focused, None);
        assert!(buffer.is_empty(), "focus leaving with no submit must scrub what it accumulated");

        focused = Some(field("screen@TEST", "lock", "authenticate"));
        buffer.push_str("hunter2");
        retarget_secure_submit(&mut focused, &mut buffer, Some(field("screen@TEST", "lock", "authenticate")));
        assert_eq!(buffer.expose_secret(), b"hunter2", "re-focusing the same field must not eat the entry in progress");
    }

    #[test]
    fn a_malformed_secure_submit_is_an_error_rather_than_a_guessed_destination() {
        let lua = Lua::new();
        let field = textfield(&lua, Some(Value::String(lua.create_string("polkit").unwrap())));
        let root = hit_node(&lua, "panel", (0.0, 0.0, 100.0, 32.0), false);
        assert!(focused_target(&[&root, &field]).is_err(), "a non-table secure_submit names no capability");
    }

    #[test]
    fn a_submit_with_a_focused_target_is_addressed_to_that_capability_and_action() {
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("hunter2");
        let target = node::SecureSubmitTarget { capability: "polkit".to_string(), action: "authenticate".to_string() };

        let frame = submit_frame_for(4, Some(&target), &mut buffer);

        assert_eq!(
            frame,
            Some(RendererFrame::SecureSubmit(SecureSubmit {
                generation_id: 4,
                capability: "polkit".to_string(),
                action: "authenticate".to_string(),
                secret: b"hunter2".to_vec(),
            }))
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_submit_with_no_focused_target_sends_nothing_and_still_zeroizes_the_buffer() {
        // docs/adr/0050 decision 4: the old placeholder addressed this to `"unknown"/"unknown"`,
        // which no Supervisor capability routes -- a password on the wire for no one. The scrub is
        // the half that is not optional.
        let mut buffer = shared::SecureBuffer::new();
        buffer.push_str("hunter2");

        assert_eq!(submit_frame_for(4, None, &mut buffer), None);
        assert!(buffer.is_empty(), "a dropped submit must still leave the accumulated secret scrubbed");
    }

    #[test]
    fn an_enter_on_an_empty_field_sends_nothing() {
        // Not free, which is why it is refused rather than merely useless: the Supervisor routes a
        // `("lock", "authenticate")` submit straight into PAM, so an Enter that said nothing spends
        // one of the user's counted attempts and one `pam_unix` failure delay.
        let mut buffer = shared::SecureBuffer::new();
        assert_eq!(submit_frame_for(4, Some(&target("lock", "authenticate")), &mut buffer), None);
    }

    #[test]
    fn keyboard_focus_arriving_on_nothing_typable_disarms_whatever_was_armed() {
        // Both of `KeyboardHandler::enter`'s "nothing to arm" cases, which used to be early returns
        // that moved `keyboard_focus` on and left the *previous* surface's field armed: keystrokes
        // then went on accumulating into that field's secret and could still be submitted to its
        // capability. `enter` now pushes this answer through `App::focus_secure_submit` whatever it
        // is, so `None` disarms and scrubs.
        let lua = Lua::new();
        let untypable = tree_with(&lua, vec![textfield(&lua, None)]);
        let armed = field("screen@TEST", "lock", "authenticate");
        assert_eq!(focus_on_enter(None, None, Some(&armed)), None, "an `enter` on a surface this process already destroyed");
        assert_eq!(focus_on_enter(Some("bar@TEST"), Some(&untypable), Some(&armed)), None, "a surface whose tree names no destination");

        let typable = tree_with(&lua, vec![textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")))]);
        assert_eq!(focus_on_enter(Some("screen@TEST"), Some(&typable), None), Some(armed.clone()));

        // What an `enter` must *not* undo: a press on a surface declaring two fields picked one the
        // sole-field rule refuses to pick, and the compositor's `enter` for that surface commonly
        // follows the press that caused it.
        let two_fields = tree_with(
            &lua,
            vec![
                textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate"))),
                textfield(&lua, Some(secure_submit_table(&lua, "polkit", "authenticate"))),
            ],
        );
        let pressed = field("screen@TEST", "polkit", "authenticate");
        assert_eq!(focus_on_enter(Some("screen@TEST"), Some(&two_fields), Some(&pressed)), Some(pressed));
        assert_eq!(
            focus_on_enter(Some("screen@TEST"), Some(&two_fields), Some(&field("bar@TEST", "network", "connect"))),
            None,
            "a field belonging to another surface is not this surface's to keep"
        );
    }

    #[test]
    fn a_field_is_armed_only_while_its_own_surface_holds_the_keyboard_and_still_exists() {
        // The one question every keystroke asks, in place of a clearing call bolted onto each of the
        // five or six sites that can take a surface away. The liveness half is the traced leak:
        // type a login password on the lock screen, the compositor sends `finished`,
        // `teardown_lock_surfaces` destroys the `wl_surface`, and no `leave` is required to follow
        // it -- so the plaintext stayed live in `App::secure_buffer`, still addressed to
        // `("lock", "authenticate")`, with later bar keystrokes appending to it.
        let armed = field("screen@TEST", "lock", "authenticate");
        assert!(focus_is_still_armed(&armed, Some("screen@TEST"), true));
        assert!(!focus_is_still_armed(&armed, Some("screen@TEST"), false), "its `wl_surface` is gone, whether or not a `leave` ever came");
        assert!(!focus_is_still_armed(&armed, Some("bar@TEST"), true), "another surface is the one receiving keys");
        assert!(!focus_is_still_armed(&armed, None, true), "the keyboard is on a surface this process does not own");
    }

    /// A `lock` tree as the scene hands one back: a root with the password field somewhere under it.
    fn tree_with(lua: &Lua, fields: Vec<layout::ResolvedNode>) -> layout::ResolvedNode {
        let mut root = hit_node(lua, "column", (0.0, 0.0, 1920.0, 1080.0), false);
        let mut inner = hit_node(lua, "column", (0.0, 0.0, 360.0, 200.0), false);
        inner.children = fields;
        root.children = vec![hit_node(lua, "label", (0.0, 0.0, 100.0, 20.0), false), inner];
        root
    }

    #[test]
    fn keyboard_focus_takes_the_one_secure_submit_field_a_surface_declares() {
        // The rule that makes a lock screen typable without a click. `focused_secure_submit` used
        // to be set only by a pointer press, so the one surface whose whole job is to accept a
        // password needed a mouse click before a keystroke could reach `SecureBuffer` at all.
        let lua = Lua::new();
        let tree = tree_with(&lua, vec![textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")))]);

        assert_eq!(
            sole_secure_submit(&tree),
            Some(node::SecureSubmitTarget { capability: "lock".to_string(), action: "authenticate".to_string() })
        );
    }

    #[test]
    fn two_secure_submit_fields_on_one_surface_focus_neither() {
        // Deliberately not "the first one": with two destinations there is no non-arbitrary answer
        // to "whose password is this?", which is the same question `submit_frame_for` refuses to
        // guess at (docs/adr/0050 decision 4). A press still picks one, because a press names a node.
        let lua = Lua::new();
        let tree = tree_with(
            &lua,
            vec![
                textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate"))),
                textfield(&lua, Some(secure_submit_table(&lua, "polkit", "authenticate"))),
            ],
        );
        assert_eq!(sole_secure_submit(&tree), None);

        // A field with no destination is not a candidate either -- it names nowhere to send to.
        let bare = tree_with(&lua, vec![textfield(&lua, None)]);
        assert_eq!(sole_secure_submit(&bare), None);
    }

    #[test]
    fn the_lock_admission_guard_and_the_keyboard_focus_rule_are_one_predicate() {
        // Defect D: `lock_command`'s `can_authenticate` asked whether *any* field unlocks, while
        // keyboard focus arms only a surface's *sole* field. A lock tree with two `secure_submit`
        // fields passed the guard, took the lock, and then armed nothing on `enter` -- on a
        // keyboard-only machine the session could not be left except by a VT switch. The two now
        // read the same answer out of the same function, so they cannot drift apart.
        let lua = Lua::new();
        let typable = tree_with(&lua, vec![textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate")))]);
        assert!(tree_can_authenticate(&typable));
        assert_eq!(sole_secure_submit(&typable), Some(target("lock", "authenticate")));

        let two_fields = tree_with(
            &lua,
            vec![
                textfield(&lua, Some(secure_submit_table(&lua, "lock", "authenticate"))),
                textfield(&lua, Some(secure_submit_table(&lua, "polkit", "authenticate"))),
            ],
        );
        assert!(!tree_can_authenticate(&two_fields), "a lock the keyboard cannot arm must not be granted the lock");
        assert_eq!(sole_secure_submit(&two_fields), None);

        // One field, but pointed somewhere the Supervisor does not route an unlock through.
        let wrong_destination = tree_with(&lua, vec![textfield(&lua, Some(secure_submit_table(&lua, "polkit", "authenticate")))]);
        assert!(!tree_can_authenticate(&wrong_destination));
    }

    #[test]
    fn only_the_lock_authenticate_pair_can_unlock_the_session() {
        // supervisor/src/main.rs routes `("lock", "authenticate")` to the PAM worker and nothing
        // else to it, so a lock screen whose field submits anywhere else can never unlock.
        assert!(unlocks_the_session(&target("lock", "authenticate")));
        assert!(!unlocks_the_session(&target("polkit", "authenticate")));
        assert!(!unlocks_the_session(&target("lock", "cancel")));
    }

    fn key(keysym: Keysym, utf8: Option<&str>) -> KeyEvent {
        KeyEvent { time: 0, raw_code: 0, keysym, utf8: utf8.map(str::to_string) }
    }

    #[test]
    fn a_focused_secure_field_reads_the_keyboard_directly() {
        // Phase 23 item 3's actual claim, which `zwp_text_input_v3` alone did not deliver: that
        // path only produces a `commit_string` when the compositor has an input method bound, so on
        // a session with no IME -- the normal case -- not one byte ever reached `SecureBuffer` and
        // the lock could not be authenticated out of.
        assert_eq!(secure_key_action(&key(Keysym::a, Some("a")), false), SecureKeyAction::Append("a"));
        assert_eq!(secure_key_action(&key(Keysym::Return, Some("\r")), false), SecureKeyAction::Submit);
        assert_eq!(secure_key_action(&key(Keysym::KP_Enter, Some("\r")), false), SecureKeyAction::Submit);
        assert_eq!(secure_key_action(&key(Keysym::BackSpace, Some("\u{8}")), false), SecureKeyAction::Backspace);
    }

    #[test]
    fn a_control_key_never_becomes_a_character_of_the_password() {
        // `utf8` is not empty for Escape, Tab or Return -- xkbcommon hands back the C0 control
        // character for each -- so an unfiltered append would silently put an ESC byte in the
        // middle of a secret that PAM then rejects with no visible reason.
        assert_eq!(secure_key_action(&key(Keysym::Tab, Some("\t")), false), SecureKeyAction::Ignore);
        assert_eq!(secure_key_action(&key(Keysym::Shift_L, None), false), SecureKeyAction::Ignore);
    }

    #[test]
    fn escape_throws_the_entry_away_instead_of_being_ignored() {
        // Escape used to reach the control-character filter above and be dropped, which left one
        // Backspace per character as the only way to abandon a mistyped password -- on the surface
        // where a wrong guess costs a counted PAM attempt and a `pam_unix` failure delay.
        assert_eq!(secure_key_action(&key(Keysym::Escape, Some("\u{1b}")), false), SecureKeyAction::Clear);
    }

    #[test]
    fn holding_enter_down_does_not_resubmit_an_already_scrubbed_buffer() {
        // A submit zeroizes the buffer as it reads it, so the second submit of a key repeat would
        // send an *empty* password to PAM and burn one of the user's attempts. Backspace and
        // ordinary characters repeat normally, which is what every text field does.
        assert_eq!(secure_key_action(&key(Keysym::Return, Some("\r")), true), SecureKeyAction::Ignore);
        assert_eq!(secure_key_action(&key(Keysym::BackSpace, Some("\u{8}")), true), SecureKeyAction::Backspace);
        assert_eq!(secure_key_action(&key(Keysym::a, Some("a")), true), SecureKeyAction::Append("a"));
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

    #[test]
    fn on_click_takes_the_button_name_as_a_second_argument_beside_the_rect() {
        // Second argument, not a fifth field on the rect table. Every handler written against the
        // one-argument form keeps working untouched, because Lua drops arguments a function does
        // not declare, and the table the config forwards to a `popup`'s `anchor_rect` stays four
        // fields wide instead of carrying a `button` into the positioner.
        let lua = Lua::new();
        let seen: Function = lua
            .load(r#"seen = {} return function(rect, button) seen.x, seen.w, seen.button = rect.x, rect.width, button end"#)
            .eval()
            .unwrap();
        call_on_click(&lua, &seen, LogicalRect { x: 12.0, y: 4.0, width: 40.0, height: 24.0 }, "right").unwrap();

        let recorded: Table = lua.globals().get("seen").unwrap();
        assert_eq!(recorded.get::<f32>("x").unwrap(), 12.0);
        assert_eq!(recorded.get::<f32>("w").unwrap(), 40.0);
        assert_eq!(recorded.get::<String>("button").unwrap(), "right");
    }

    #[test]
    fn a_one_argument_on_click_still_runs_unchanged() {
        // docs/adr/0050 decision 3's exact worked example, which every config in the tree uses.
        let lua = Lua::new();
        let anchor: Function = lua.load(r#"anchor = nil return function(rect) anchor = rect end"#).eval().unwrap();
        call_on_click(&lua, &anchor, LogicalRect { x: 40.0, y: 0.0, width: 86.0, height: 24.0 }, "left").unwrap();

        let recorded: Table = lua.globals().get("anchor").unwrap();
        assert_eq!(recorded.get::<f32>("x").unwrap(), 40.0);
        assert_eq!(recorded.get::<f32>("height").unwrap(), 24.0);
    }

    // --- `popup` (§ 6.3, docs/adr/0049, docs/adr/0051) ---

    fn popup_spec_fixture() -> PopupSpec {
        PopupSpec {
            id: "menu".to_string(),
            parent: "bar".to_string(),
            anchor_rect: LogicalRect { x: 997.0, y: 4.0, width: 86.0, height: 24.0 },
            width: 200.0,
            height: 120.0,
            anchor: PopupAnchor::BottomLeft,
            gravity: PopupAnchor::BottomRight,
            constraint_adjustment: ConstraintAdjustment::default(),
            offset: node::PopupOffset { x: 0.0, y: 4.0 },
            grab: true,
        }
    }

    fn lock_spec_fixture() -> node::LockSpec {
        node::LockSpec { id: "lock_screen".to_string() }
    }

    fn window(id: &str) -> WindowSpec {
        WindowSpec {
            id: id.to_string(),
            title: String::new(),
            app_id: format!("oblisk-{id}"),
            min_size: None,
            max_size: None,
        }
    }

    #[test]
    fn a_failed_startup_apply_leaves_panels_up_and_windows_and_popups_closed() {
        // The apply rolls its whole surface map back on error, so this is what every instance sees
        // at once. A panel comes up painting nothing, which is the "keep the shell up" fallback the
        // rest of this file follows; a window or popup created here would be a Wayland object the
        // config never asked for, and with the dev config that is an empty `settings` window
        // claiming a tile and taking focus.
        assert!(starting_visible(None, &SurfaceSpec::Panel(panel("bar"))));
        assert!(!starting_visible(None, &SurfaceSpec::Window(window("settings"))));
        assert!(!starting_visible(None, &SurfaceSpec::Popup(popup_spec_fixture())));
        assert!(!starting_visible(None, &SurfaceSpec::Lock(lock_spec_fixture())));
    }

    #[test]
    fn a_resolved_tree_answers_visible_for_every_role_and_the_fallback_never_runs() {
        for roster in [
            SurfaceSpec::Panel(panel("bar")),
            SurfaceSpec::Window(window("settings")),
            SurfaceSpec::Popup(popup_spec_fixture()),
            SurfaceSpec::Lock(lock_spec_fixture()),
        ] {
            assert!(starting_visible(Some(true), &roster));
            assert!(!starting_visible(Some(false), &roster), "a declared-closed surface stays closed whatever its role");
        }
    }

    #[test]
    fn a_new_surfaces_spec_comes_from_the_resolved_tree_not_the_evaluations_roster() {
        // docs/adr/0049's second amendment at the one site that was still reading the roster. The
        // roster's `anchor_rect` is `DEFERRED_POPUP_EXTENT`'s 1x1 placeholder whenever the config
        // signal-bound it, and a popup shown from that keeps it for its whole life: the positioner
        // is consumed by `get_popup` and `xdg_popup.reposition` is not built.
        let lua = Lua::new();
        let rect = rect_table(&lua, LogicalRect { x: 40.0, y: 4.0, width: 86.0, height: 24.0 }).unwrap();
        let properties = HashMap::from([
            ("id".to_string(), Value::String(lua.create_string("menu").unwrap())),
            ("parent".to_string(), Value::String(lua.create_string("bar").unwrap())),
            ("anchor_rect".to_string(), Value::Table(rect)),
            ("width".to_string(), Value::Number(200.0)),
            ("height".to_string(), Value::Number(120.0)),
        ]);
        let mut placeholder = popup_spec_fixture();
        placeholder.anchor_rect = LogicalRect { x: 0.0, y: 0.0, width: 1.0, height: 1.0 };

        let (role, spec) = resolved_surface_spec(&SurfaceSpec::Popup(placeholder), &properties);
        assert_eq!(role, "popup");
        let SurfaceSpec::Popup(spec) = spec.unwrap() else { panic!("the role comes from the roster, not from the properties") };
        assert_eq!(spec.anchor_rect, LogicalRect { x: 40.0, y: 4.0, width: 86.0, height: 24.0 });
    }

    #[test]
    fn resolved_properties_that_do_not_parse_name_the_role_and_leave_the_roster_spec_standing() {
        // Same shape `apply_resolved_state` logs on every later pass: the caller keeps the last
        // applied spec rather than building a surface out of protocol defaults.
        let lua = Lua::new();
        let properties = HashMap::from([
            ("id".to_string(), Value::String(lua.create_string("bar").unwrap())),
            ("exclusive".to_string(), Value::Number(32.0)),
        ]);
        let (role, spec) = resolved_surface_spec(&SurfaceSpec::Panel(panel("bar")), &properties);
        assert_eq!(role, "panel");
        assert!(spec.is_err());
    }

    #[test]
    fn a_visible_popup_with_no_object_is_created_unless_the_latch_is_set() {
        assert_eq!(popup_visibility_action(true, false, None, 4), PopupAction::Create);
        // docs/adr/0051 decision 2, and the one row the whole latch exists for: a compositor
        // dismissal leaves the resolved tree still saying `visible = true`, so without this the
        // next re-resolve creates a second popup for the same click-outside to dismiss, forever.
        assert_eq!(popup_visibility_action(true, false, Some(4), 4), PopupAction::Nothing);
    }

    #[test]
    fn a_dismissal_with_no_pointer_input_since_holds_the_latch_for_the_generations_life() {
        // The livelock docs/adr/0051 decision 2 exists to stop, in the config that has no
        // `on_dismiss` at all. Nothing new arrives, so the counter never moves and no re-resolve
        // ever creates a replacement -- not for one turn, but forever.
        for _ in 0..1000 {
            assert_eq!(popup_visibility_action(true, false, Some(9), 9), PopupAction::Nothing);
        }
    }

    #[test]
    fn a_click_arriving_after_the_dismissal_clears_the_latch_in_the_same_turn() {
        // docs/adr/0051's first amendment. Under a grab niri delivers the closing click to the
        // parent bar as well, so `popup_done` and the button's `on_click` land in one batch:
        // `on_dismiss` writes false, `on_click` writes true, and the end-of-turn sample reads true.
        // The value of `visible` cannot separate the two cases; whether the user asked again can,
        // and `popup_done` is dispatched before the pointer events that follow it, so the counter
        // has already moved by the time visibility is applied.
        assert_eq!(popup_visibility_action(true, false, Some(9), 10), PopupAction::Create);
    }

    #[test]
    fn a_popup_that_is_already_open_is_left_alone_on_every_later_re_resolve() {
        // Not a degenerate case: ADR-0044 decision 2's dirty flag is one flag for the whole scene,
        // so `apply_resolved_state` runs for every surface on every capability push, and an open
        // popup passes through here several times a second.
        assert_eq!(popup_visibility_action(true, true, None, 4), PopupAction::Nothing);
    }

    #[test]
    fn visible_going_false_destroys_an_open_popup_and_asks_for_nothing_from_a_closed_one() {
        assert_eq!(popup_visibility_action(false, true, None, 4), PopupAction::Destroy);
        assert_eq!(popup_visibility_action(false, true, Some(4), 4), PopupAction::Destroy);
        // The row that reopens the path. Nothing is destroyed because the compositor already did
        // it; the caller clears the latch on this same edge, which is what lets an `on_dismiss`
        // writing `visible = false` make the popup openable again immediately.
        assert_eq!(popup_visibility_action(false, false, Some(4), 4), PopupAction::Nothing);
        assert_eq!(popup_visibility_action(false, false, None, 4), PopupAction::Nothing);
    }

    #[test]
    fn a_popup_anchors_to_the_parent_instance_the_arming_click_landed_on() {
        // docs/adr/0051 decision 1. Two monitors, one declared `bar`, and the click decides.
        let instances = ["bar@eDP-1", "bar@DP-1", "menu"];
        assert_eq!(parent_instance_index(instances.into_iter(), "bar", Some("bar@DP-1")), Some(1));
        assert_eq!(parent_instance_index(instances.into_iter(), "bar", Some("bar@eDP-1")), Some(0));
    }

    #[test]
    fn a_popup_with_nothing_armed_falls_back_to_the_first_instance_of_its_parent() {
        // The `grab = false` popup opened by a D-Bus notification. There is no better answer
        // available -- § 6.3 gives such a popup no way to say which monitor it means -- and the
        // ponytail on `parent_instance_index` names the upgrade path.
        let instances = ["bar@eDP-1", "bar@DP-1"];
        assert_eq!(parent_instance_index(instances.into_iter(), "bar", None), Some(0));
    }

    #[test]
    fn a_click_on_some_other_surface_still_falls_back_to_the_first_parent_instance() {
        // A popup opened by a click on the *notification area* while naming `bar` as its parent.
        // The armed surface is not a candidate at all, so the fallback is the only answer left.
        let instances = ["bar@eDP-1", "bar@DP-1", "notification_area@DP-1"];
        assert_eq!(parent_instance_index(instances.into_iter(), "bar", Some("notification_area@DP-1")), Some(0));
    }

    #[test]
    fn a_popup_whose_parent_is_declared_nowhere_gets_no_index() {
        let instances = ["bar@eDP-1", "settings"];
        assert_eq!(parent_instance_index(instances.into_iter(), "launcher", Some("bar@eDP-1")), None);
    }

    #[test]
    fn a_popup_parents_to_a_window_by_its_bare_instance_id() {
        // § 6.3: a popup parents to either a `panel` or a `window`, and a window's instance carries
        // no `@output` because the compositor places it.
        let instances = ["bar@eDP-1", "settings"];
        assert_eq!(parent_instance_index(instances.into_iter(), "settings", None), Some(1));
    }

    #[test]
    fn a_popup_configure_is_taken_as_given_because_the_compositor_may_have_constrained_it() {
        // § 6.3's `constraint_adjustment` lets the compositor slide, flip or resize the popup to
        // keep it on screen, and the size it lands on is the one that has to be painted.
        assert_eq!(popup_size_for((180, 90), &popup_spec_fixture()), (180, 90));
    }

    #[test]
    fn a_popup_configure_with_no_size_falls_back_to_what_the_positioner_asked_for() {
        // `PopupInner` seeds its pending dimensions at `-1` and reports whatever they hold when the
        // wrapping `xdg_surface.configure` arrives. xdg-shell requires an `xdg_popup.configure`
        // first, but a `-1` reaching `WlEglSurface::new` is a crash and the requested size is right
        // there.
        assert_eq!(popup_size_for((-1, -1), &popup_spec_fixture()), (200, 120));
        assert_eq!(popup_size_for((180, 0), &popup_spec_fixture()), (180, 120), "per axis, not all or nothing");
    }

    #[test]
    fn a_popup_never_takes_a_zero_sized_buffer() {
        let mut spec = popup_spec_fixture();
        spec.width = 0.0;
        spec.height = 0.0;
        assert_eq!(popup_size_for((0, 0), &spec), (1, 1), "a wl_egl_window of 0 is invalid");
    }

    #[test]
    fn every_popup_anchor_maps_to_its_protocol_anchor_and_gravity() {
        for (ours, anchor, gravity) in [
            (PopupAnchor::Top, xdg_positioner::Anchor::Top, xdg_positioner::Gravity::Top),
            (PopupAnchor::Bottom, xdg_positioner::Anchor::Bottom, xdg_positioner::Gravity::Bottom),
            (PopupAnchor::Left, xdg_positioner::Anchor::Left, xdg_positioner::Gravity::Left),
            (PopupAnchor::Right, xdg_positioner::Anchor::Right, xdg_positioner::Gravity::Right),
            (PopupAnchor::TopLeft, xdg_positioner::Anchor::TopLeft, xdg_positioner::Gravity::TopLeft),
            (PopupAnchor::TopRight, xdg_positioner::Anchor::TopRight, xdg_positioner::Gravity::TopRight),
            (PopupAnchor::BottomLeft, xdg_positioner::Anchor::BottomLeft, xdg_positioner::Gravity::BottomLeft),
            (PopupAnchor::BottomRight, xdg_positioner::Anchor::BottomRight, xdg_positioner::Gravity::BottomRight),
        ] {
            assert_eq!(positioner_anchor(ours), anchor);
            assert_eq!(positioner_gravity(ours), gravity);
        }
    }

    #[test]
    fn section_6_3s_center_is_the_protocols_none_on_both_requests() {
        // The one value with no entry of its own in either protocol enum. The XML is what makes
        // this a translation rather than a fudge: with no edge specified the anchor point is "in
        // the center of the anchor rectangle", and a gravity of `none` centers the surface "over
        // the anchor point on any axis that had no gravity specified".
        assert_eq!(positioner_anchor(PopupAnchor::Center), xdg_positioner::Anchor::None);
        assert_eq!(positioner_gravity(PopupAnchor::Center), xdg_positioner::Gravity::None);
    }

    #[test]
    fn constraint_adjustment_booleans_map_to_the_matching_bitmask() {
        assert_eq!(
            positioner_constraint(ConstraintAdjustment::NONE),
            xdg_positioner::ConstraintAdjustment::None,
            "an explicitly empty array is the protocol's own no-adjustment"
        );
        assert_eq!(
            positioner_constraint(ConstraintAdjustment::default()),
            xdg_positioner::ConstraintAdjustment::FlipY | xdg_positioner::ConstraintAdjustment::SlideX,
            "§ 6.3's default is dropdown behaviour, not the protocol's"
        );
        assert_eq!(
            positioner_constraint(ConstraintAdjustment {
                slide_x: true,
                slide_y: true,
                flip_x: true,
                flip_y: true,
                resize_x: true,
                resize_y: true,
            }),
            xdg_positioner::ConstraintAdjustment::SlideX
                | xdg_positioner::ConstraintAdjustment::SlideY
                | xdg_positioner::ConstraintAdjustment::FlipX
                | xdg_positioner::ConstraintAdjustment::FlipY
                | xdg_positioner::ConstraintAdjustment::ResizeX
                | xdg_positioner::ConstraintAdjustment::ResizeY
        );
    }
}
