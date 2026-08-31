//! `xdg_shell`'s two client-picked roles: `window` (`xdg_toplevel`, § 6.2) and `popup`
//! (`xdg_popup`, § 6.3), including positioner construction, size negotiation, and the
//! click-outside/dismiss latch docs/adr/0049 and docs/adr/0051 describe.
//!
//! Creation, in-place spec updates and the two roles' configure/close/dismiss callbacks all live
//! here; the generic bind/paint/(un)map machinery both share with every other role stays in
//! `surface`.

use super::*;
use crate::wayland::surface::MapState;
use crate::wayland::surface::PopupParent;
use crate::wayland::surface::PopupRefusal;
use crate::wayland::surface::TrackedRole;

/// What one resolution of a `popup`'s `visible` does, given whether its `xdg_popup` currently
/// exists and whether docs/adr/0051 decision 2's latch is set (§ 5.1's `visible`, docs/adr/0049
/// decision 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopupAction {
    Create,
    Destroy,
    Nothing,
}
/// Decision 2's latch as a state machine, split out because it is the pure part of the popup
/// path, where a mistake is a livelock rather than a missing window.
///
/// `dismissed_at` is [`App::pointer_input_count`] when the compositor dismissed this popup;
/// `pointer_input` is what that counter reads now. A popup is latched only while no pointer
/// input has arrived since its dismissal (docs/adr/0051's first amendment): the `visible = false`
/// edge decision 2 named is unobservable in the case that matters, so a latch keyed on it alone
/// would be permanent.
///
/// The latch is read here and cleared by the caller on the same `visible = false` this returns
/// `Destroy` or `Nothing` for -- this function makes no writes.
///
/// - `visible = true`, no object, dismissed with the counter unmoved: **nothing** (decision 2).
///   Without the latch, the next re-resolve would recreate the popup for the same click-outside
///   to dismiss, forever, and a config with no `on_dismiss` is not a config error.
/// - `visible = true`, no object, dismissed but the counter moved: **create**. The user clicked
///   again.
/// - `visible = false`, no object: **nothing**, and the caller still clears the latch.
/// - `visible = true`, object already exists: **nothing**. Every re-resolve runs
///   `apply_resolved_state` for every surface, so an already-open popup passes through here on
///   every capability push.
fn popup_visibility_action(visible: bool, exists: bool, dismissed_at: Option<u64>, pointer_input: u64) -> PopupAction {
    let latched = dismissed_at == Some(pointer_input);
    match (visible, exists) {
        (true, false) if !latched => PopupAction::Create,
        (false, true) => PopupAction::Destroy,
        _ => PopupAction::Nothing,
    }
}
/// Which tracked surface a popup roots under (docs/adr/0051 decision 1), as an index into the same
/// iterator's order.
///
/// § 6.3's `parent` names a declared `id`, not one surface: `monitor = "All"` expands a `panel`
/// per output (docs/adr/0038 decision 3), so `parent = "bar"` on a two-monitor session names two
/// layer surfaces and `get_popup` takes exactly one. The tie-break is the click that armed the
/// grab: a dropdown belongs to the monitor it was opened on, which costs a field on [`ArmedSerial`]
/// rather than a second mechanism.
///
/// ponytail: with nothing armed -- a `grab = false` popup opened by a D-Bus notification -- this
/// falls back to the *first* instance of the named parent, which on a multi-monitor session is
/// whichever output `expand_instances` listed first. There is no better answer available: nothing
/// in § 6.3 lets such a popup say which monitor it means. The upgrade path is a `monitor` property
/// on `popup`, at which point this takes a third argument and the fallback becomes a real choice.
fn parent_instance_index<'a>(
    instance_ids: impl Iterator<Item = &'a str>,
    parent: &str,
    armed: Option<&str>,
) -> Option<usize> {
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
/// A configure's `width`/`height` are the compositor's answer, taken as given: it may have slid,
/// flipped or resized the popup to keep it on screen (§ 6.3's `constraint_adjustment`), and the
/// size it lands on is what has to be painted.
///
/// A non-positive axis falls back to the size the positioner asked for -- a guard against SCTK,
/// not the compositor: `PopupInner` seeds its pending dimensions at `-1` and reports whatever they
/// hold when `xdg_surface.configure` arrives, so a configure reaching `xdg_surface` without an
/// `xdg_popup.configure` first would hand this `-1`, and that reaching `WlEglSurface::new` is a
/// crash-shaped failure.
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
/// Every field is sent, including ones equal to the protocol default: the positioner is built
/// fresh per open and destroyed with the call, so there is no live object to diff against.
///
/// Rounded, not truncated, on the way to `i32`: these are logical pixels a `button`'s resolved
/// rect handed the config through `on_click` (docs/adr/0050 decision 3), so `x = 996.6` belongs
/// one pixel right of `996`, not on it.
///
/// The two sizes are clamped to at least 1: `node::parse_popup_extent`/`parse_anchor_rect` already
/// refuse a zero, so this only catches a positive value that rounds to zero.
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
/// The size a toplevel's buffer takes for one `xdg_toplevel` configure.
///
/// A `Some` axis is the compositor's, taken as given: `xdg_toplevel::configure`'s wording makes a
/// maximized or fullscreen size binding, and a tiling compositor sizes every window this way, so
/// on niri this is the only branch that ever runs.
///
/// A `None` axis is "the client picks" ("If this value is None, you may set the size of the window
/// as you wish"), the ordinary first configure on a floating compositor. It picks the config's own
/// `min_size` for that axis, falling back to [`UNCONFIGURED_WINDOW_SIZE`], then clamped by
/// `max_size`: § 6.2's "advisory" caveat is about what the compositor may do with these numbers,
/// not a licence to ignore them when nobody else has chosen.
///
/// A zero `max_size` axis is not a maximum of zero: `set_max_size`'s own "0 means no expected
/// maximum size in the given dimension", the same reading [`node::window_spec`]'s parser applies.
///
/// At least 1 on both axes: a `wl_egl_window` of 0 is invalid.
fn toplevel_size_for(
    new_size: (Option<std::num::NonZeroU32>, Option<std::num::NonZeroU32>),
    spec: &WindowSpec,
) -> (u32, u32) {
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
/// The `xdg_toplevel` requests one live toplevel needs after a re-resolve changed its `window`
/// properties (§ 6.2, docs/adr/0049's second amendment). `None` per field means "unchanged, send
/// nothing", as `layer::SpecUpdate` does for a panel: all four are double-buffered, so re-sending an
/// unchanged value is noise, not an error.
///
/// Every one of § 6.2's protocol-facing fields is here, unlike a panel: `SpecUpdate` omits
/// [`node::SurfaceTopology`]'s five because `get_layer_surface` fixes them at creation, but a
/// toplevel has no such set (`xdg-shell.xml` allows `set_app_id`/`set_title` after mapping, and
/// both size hints are ordinary double-buffered requests). So a changed `title` is always an
/// in-place update, and the only field left out is `id`, the reconcile identity rather than a
/// protocol field.
///
/// `Option<Option<SizeHint>>` is the honest type: the outer layer is "did it move", the inner one
/// is § 6.2's own absent-versus-present distinction, and "moved to absent" is a real transition
/// that has to reach `set_min_size(None)`, which the protocol spells as a zero, meaning unset.
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

impl App {
    /// [`App::create_surfaces`]'s `window` arm: the tracking entry always, the `xdg_toplevel` only
    /// if this window is already shown (docs/adr/0049 decision 1).
    ///
    /// The entry exists either way because it is what makes the window reachable at all: the poll
    /// loop's [`App::apply_resolved_surface_state`] walks `self.surfaces`, and a window with no
    /// entry would never have its `visible` looked at, so it could never open.
    pub(super) fn create_window(
        &mut self,
        qh: &QueueHandle<App>,
        spec: &WindowSpec,
        instance: &SurfaceInstance,
        visible: bool,
    ) {
        self.surfaces.push(TrackedSurface {
            role: TrackedRole::Window { window: None, spec: spec.clone() },
            bound: None,
            surface_id: instance.instance_id.clone(),
            map_state: MapState::Unmapped,
            null_buffered: false,
            configured_size: (0, 0),
            last_painted: None,
        });
        if visible {
            let index = self.surfaces.len() - 1;
            self.show_window(qh, index);
        }
    }

    /// [`App::create_surfaces`]'s `popup` arm: the tracking entry always, the `xdg_popup` only if
    /// this popup is already shown (docs/adr/0049 decision 1, docs/adr/0051 decision 1).
    ///
    /// The entry exists for [`App::create_window`]'s reason: it is what makes the scene resolve the
    /// popup's tree, which `visible` is read off. Twenty declared popups still cost twenty retained
    /// nodes and zero Wayland objects.
    ///
    /// A popup declared `visible = true` at startup with § 6.3's default `grab = true` is refused by
    /// [`App::show_popup`] and says so once, which is correct, not a startup failure: nothing has been
    /// clicked, so there is no serial, and a dropdown that cannot be dismissed by clicking outside it
    /// is worse than one that did not open (docs/adr/0049's amendment).
    pub(super) fn create_popup(
        &mut self,
        qh: &QueueHandle<App>,
        spec: &PopupSpec,
        instance: &SurfaceInstance,
        visible: bool,
    ) {
        self.surfaces.push(TrackedSurface {
            role: TrackedRole::Popup { popup: None, spec: spec.clone(), dismissed_at: None, refusal_logged: None },
            bound: None,
            surface_id: instance.instance_id.clone(),
            map_state: MapState::Unmapped,
            null_buffered: false,
            configured_size: (0, 0),
            last_painted: None,
        });
        if visible {
            let index = self.surfaces.len() - 1;
            self.show_popup(qh, index);
        }
    }

    /// Diffs one toplevel's freshly resolved `window` spec against the one its `xdg_toplevel` state
    /// was last set from and sends only what moved (§ 6.2; see [`window_update`] for which fields).
    ///
    /// Sends nothing while the window is not shown, and stores the spec anyway -- not a dropped
    /// update: `visible = false` means there is no `xdg_toplevel` to send a request to (docs/adr/0049
    /// decision 1), and [`App::show_window`] builds the next one out of exactly this stored spec. So
    /// a `title` that changed three times while closed opens with the third one.
    pub(super) fn apply_window_change(&mut self, index: usize, fresh: WindowSpec) {
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

    /// [`App::apply_visibility`]'s `popup` arm (docs/adr/0049 decision 2, docs/adr/0051 decision 2).
    ///
    /// The decision itself is [`popup_visibility_action`], which is pure and tested; this is the
    /// writes it does not make. The latch clear is unconditional on the `visible = false` edge rather
    /// than paired with a `Destroy`: a popup the compositor dismissed is already objectless, so the
    /// false edge that reopens its path is exactly the one where there is nothing left to destroy.
    ///
    /// That edge is kept even though docs/adr/0051's first amendment made it no longer the only way
    /// out of the latch: it is still the ordinary case, since a config whose `on_dismiss` writes
    /// `visible = false` reopens the path on the turn it does so, without waiting for the pointer
    /// count to move.
    pub(super) fn apply_popup_visibility(&mut self, index: usize, visible: bool) {
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
    /// (§ 6.2, docs/adr/0040 decisions 4 and 5, docs/adr/0049 decision 1).
    ///
    /// The whole sequence is the layer-shell one with a different constructor: create the surface,
    /// send the role's state, commit with no buffer attached, and wait for the configure before
    /// anything may be drawn. `MapState::AwaitingConfigure` is that wait, shared verbatim with the
    /// panel path.
    ///
    /// SCTK does two things it would be easy to get wrong here. It acks each `xdg_surface.configure`
    /// itself, through the wrapping `xdg_surface` rather than the role object
    /// (`shell/xdg/window/inner.rs`'s `Dispatch2<XdgSurface, _>`), so nothing in this file acks; and
    /// `XdgShell::bind` already picked up `zxdg_decoration_manager_v1` alongside `xdg_wm_base`, so
    /// `WindowDecorations::RequestServer` plus [`Window::request_decoration_mode`] is the whole of
    /// decoration handling and there is no second global to bind.
    ///
    /// No `set_window_geometry`: `xdg_surface`'s own default is the bounding box of the surface and
    /// its subsurfaces, this shell draws its content edge to edge with no client-side shadow to
    /// exclude, and there are no subsurfaces.
    ///
    /// A compositor with no xdg-shell leaves the window unbuilt, logged once per attempt: not fatal,
    /// on the same "keep the shell up" principle every other failure in this file follows.
    pub(super) fn show_window(&mut self, qh: &QueueHandle<App>, index: usize) {
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
        // Asked for explicitly as well as through `WindowDecorations::RequestServer`: the two reach
        // different objects, the constructor argument decides whether a
        // `zxdg_toplevel_decoration_v1` is created at all, and this is the `set_mode` on it.
        // Whatever the compositor answers with is accepted -- `WindowHandler::configure` logs a
        // client-side grant and carries on undecorated rather than faking a frame.
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
    /// Teardown order is [`App::destroy_surface_by_id`]'s: EGL surface by hand, then the
    /// `wl_egl_window`, then the role object. Dropping the [`Window`] handle is that last step --
    /// SCTK's `WindowInner::drop` destroys the decoration object, then the `xdg_toplevel`, then the
    /// `xdg_surface`, then the `wl_surface`, the order xdg-shell requires.
    ///
    /// `configured_size` is cleared with the binding (inside `release_bound`), because the next
    /// toplevel gets its own configure and must not paint into a stale one.
    pub(super) fn hide_window(&mut self, index: usize) {
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
    /// takes the grab if § 6.3 asked for one, and performs the initial commit (§ 6.3, docs/adr/0040
    /// decision 2, docs/adr/0049 decisions 1-2, docs/adr/0051 decisions 1 and 3).
    ///
    /// The order below is the protocol's and every step of it is load-bearing. Build the positioner
    /// and set every field, because `get_popup` reads it once and consumes it. Create the
    /// `wl_surface` and the popup with [`Popup::from_surface`], not [`Popup::new`] -- `new` sends the
    /// initial commit for you, which is fatal for a `panel` parent whose rooting request has not been
    /// sent yet ("If you do not specify a parent surface, you must configure the parent using an
    /// alternate function such as `LayerSurface::get_popup` prior to committing the surface, or you
    /// will get an `invalid_popup_parent` protocol error"). Root it. Take the grab, which "must be
    /// requested before the popup is mapped" or the compositor raises `invalid_grab`. Then commit,
    /// and wait for the configure in [`MapState::AwaitingConfigure`] exactly as the other two roles do.
    ///
    /// `grab = true` with nothing armed refuses to create the popup at all, rather than creating one
    /// without its grab (docs/adr/0049's amendment, docs/adr/0051 decision 3). A dropdown that cannot
    /// be dismissed by clicking outside it is worse than one that did not open. A grab the compositor
    /// refuses is a different thing and needs no branch here: it arrives as an immediate `popup_done`
    /// and goes through [`PopupHandler::done`] like a click-outside, which § 6.3 treats as a normal
    /// outcome.
    ///
    /// SCTK acks each `xdg_surface.configure` itself before calling [`PopupHandler::configure`]
    /// (`shell/xdg/popup.rs`'s `Dispatch2<XdgSurface, _>`), so nothing here acks. The grab is the one
    /// request it does not wrap, reached through `Popup::xdg_popup()` -- the same raw-object escape
    /// hatch docs/adr/0009 established for `wp-text-input-v3`.
    fn show_popup(&mut self, qh: &QueueHandle<App>, index: usize) {
        let surface_id = self.surfaces[index].surface_id.clone();
        let Some(xdg_shell) = self.xdg_shell.as_ref() else {
            eprintln!(
                "[oblisk-renderer] {surface_id}: this compositor advertises no xdg_wm_base, so no popup can be created for it"
            );
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
                    eprintln!(
                        "[oblisk-renderer] {surface_id}: `grab = true` and this compositor advertises no seat, so it is not opened"
                    );
                }
                return;
            };
            Some((seat, armed))
        } else {
            None
        };

        // The armed surface is consulted whether or not a grab was asked for: docs/adr/0051
        // decision 1 is about which instance of the declared parent a dropdown belongs to, and a
        // `grab = false` popup opened by a click belongs to that monitor just as much as a
        // grabbing one does.
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
        // `positioner` drops at the end of this function, destroying the `xdg_positioner` -- the
        // protocol's own lifecycle, since `get_popup` has already copied its state.

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
    /// Children first: `xdg_popup`'s own description makes destroying a parent before its child a
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
    /// Every path that destroys a surface owes this call, and the protocol is why: wlroots rejects
    /// destroying an `xdg_surface` whose popup list is non-empty, which takes the Wayland connection
    /// and the whole shell down with it. [`App::hide_popup`] was once the only path that did it, so a
    /// `popup { parent = "settings" }` that was open when the config wrote `settings_open = false`
    /// destroyed its parent's `xdg_toplevel` underneath a live `xdg_popup`; output removal did the
    /// same to a popup parented to a per-output panel. On a compositor that tolerates it the popup
    /// was instead left with an object and `MapState::Mapped`, which [`popup_visibility_action`]
    /// answers `Nothing` to forever.
    ///
    /// The latch is deliberately not set here, and only `hide_popup` sets it on what this returns. A
    /// parent going away is not a compositor dismissal: the child's own `visible` never moved, so the
    /// declarative answer is that it reappears the moment its parent does, on the first re-resolve
    /// after that. Latching would make it wait for pointer input instead, which a parent window
    /// reopened by a D-Bus notification never produces. The cost is that each re-resolve while the
    /// parent is away runs a [`App::show_popup`] that refuses at the `parent` check, a handful of
    /// branches and one throttled log line ([`PopupRefusal`]).
    pub(super) fn drop_child_popups(&mut self, index: usize) -> Vec<usize> {
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

    /// Every currently shown popup rooted under the surface at `index`, appended deepest-first -- the
    /// order [`App::hide_popup`] destroys in.
    ///
    /// Post-order over the parent tree, so a grandchild is appended before its parent and a parent
    /// before `index` itself (never appended; the caller owns that). Siblings come out in tracked
    /// order, which is fine: xdg-shell constrains a popup against its parent, and two popups under
    /// one parent constrain each other not at all.
    ///
    /// Cannot recurse forever, not because of a depth guard: a popup is only counted here while it
    /// holds an object, and it cannot hold one unless its parent held one first ([`App::show_popup`]
    /// refuses otherwise), so a `parent` cycle in a config -- including a popup naming itself -- has
    /// no member that ever opens.
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
}

/// `xdg_toplevel` for the `window` role (§ 6.2). See `delegate_dispatch2!(App)` at the bottom of
/// this file for why no `delegate_xdg_shell!`/`delegate_xdg_window!` call accompanies this.
impl WindowHandler for App {
    /// `xdg_toplevel::close`, a request, not a command: "The client may choose to ignore this
    /// request", and § 6.2 makes that the config's call -- the callback may decline by doing
    /// nothing, and the window stays open until the config sets `visible = false`.
    ///
    /// So this deliberately destroys nothing: closing on behalf of a config that did not ask would
    /// take the decision away from docs/adr/0049 decision 2 and leave the scene's `visible` saying
    /// `true` about a window that no longer exists, which the next re-resolve would answer by
    /// creating a second one.
    ///
    /// The Lua call has `fire_on_click`'s shape: the `Function` is cloned out of the resolved tree
    /// so no borrow of `self.client` is live while Lua runs, and a raise is logged and swallowed
    /// rather than taking down a shell that is otherwise painting.
    fn request_close(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, window: &Window) {
        let Some(index) = self.index_of_surface(window.wl_surface()) else {
            return;
        };
        let surface_id = self.surfaces[index].surface_id.clone();
        let on_close =
            self.client.scene().surface(&surface_id).and_then(|tree| match tree.properties.get("on_close") {
                // § 6.2 leaves the key opaque to `layout::node` exactly as § 5.2 leaves `on_click`,
                // so this is the only place its type is ever checked. Anything that is not a
                // function simply is not a close handler.
                Some(Value::Function(on_close)) => Some(on_close.clone()),
                _ => None,
            });
        let Some(on_close) = on_close else {
            eprintln!(
                "[oblisk-renderer] {surface_id}: the compositor asked it to close and no `on_close` declined or accepted; staying open"
            );
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
    /// exactly one:
    ///
    /// - `new_size`, whose axes are `Option` because a toplevel may be told to pick for itself.
    ///   [`toplevel_size_for`] is that decision.
    /// - `decoration_mode`, logged on the first configure of a mapping when the compositor granted
    ///   client-side decorations. Logged and nothing more: ADR-0040 decision 4 and § 6.2 both refuse
    ///   a client-side titlebar frame, so an undecorated window is the accepted outcome. Only the
    ///   first, since a configure repeats on every resize and the mode rarely moves after that.
    /// - `state` and `capabilities`, deliberately unread: § 5.2 and § 6.2 give a config nothing to
    ///   bind them to, and their one consequence that matters -- a fullscreen or maximized
    ///   configure is binding -- already reaches this shell as a `Some` axis of `new_size`.
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
        if configure.decoration_mode == DecorationMode::Client
            && self.surfaces[index].map_state == MapState::AwaitingConfigure
        {
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

/// `xdg_popup` for the `popup` role (§ 6.3). See `delegate_dispatch2!(App)` at the bottom of this
/// file for why no `delegate_xdg_popup!` call accompanies this.
impl PopupHandler for App {
    /// One `xdg_surface.configure`, already acked by SCTK before this runs (see
    /// [`App::show_popup`]). Everything after the size decision is `bind_and_clear`, shared verbatim
    /// with layer-shell and the toplevel path.
    ///
    /// [`PopupConfigure`] carries two things this deliberately does not read.
    ///
    /// - `position`, the popup's offset from its parent's window geometry. The compositor places a
    ///   popup; the client neither needs nor may act on where it landed, and nothing in § 6.3 gives
    ///   a config anything to bind it to.
    /// - `kind`, which is `Initial` on every configure this shell will ever see. The other two
    ///   variants are `Reactive` (needs `xdg_positioner::set_reactive`, which
    ///   [`configure_positioner`] does not send) and `Reposition` (needs `xdg_popup.reposition`),
    ///   both deliberately not built -- a fresh popup per open covers a dropdown that opens under
    ///   different buttons, and only an anchor that moves while a popup is open needs either.
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

    /// `xdg_popup.popup_done`, not a request, and the whole reason docs/adr/0040 reached for a real
    /// `xdg_popup` instead of a second `panel`: the compositor dismissing the popup on click-outside,
    /// which layer-shell has no compositor-agnostic way to do.
    ///
    /// Three things happen, and the order matters. The object is destroyed, children first
    /// ([`App::hide_popup`]). docs/adr/0051 decision 2's latch is set, so no replacement appears
    /// unasked. Then § 6.3's `on_dismiss` fires, into a config that finds the popup already gone.
    ///
    /// Deliberately not [`WindowHandler::request_close`]'s rule: `close` is a request the client may
    /// ignore, so that path destroys nothing and lets the config decide. `popup_done` is not a
    /// request: the object is already gone, and the engine must not trust the config to answer
    /// whether a replacement appears -- a config with no `on_dismiss` is not a config error, and
    /// without the latch it would be a livelock, each re-resolve creating a popup for the same
    /// click-outside to dismiss.
    ///
    /// A grab the compositor denied arrives here too, immediately after `show_popup` asked for one,
    /// and needs no branch: § 6.3 calls that a normal outcome (docs/adr/0051 decision 3).
    ///
    /// The Lua call has [`App::fire_on_click`]'s shape: the `Function` is cloned out of the resolved
    /// tree so no borrow of `self.client` is live while Lua runs, and a raise is logged and
    /// swallowed rather than taking down a shell that is otherwise painting.
    fn done(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, popup: &Popup) {
        let Some(index) = self.index_of_surface(popup.wl_surface()) else {
            return;
        };
        let surface_id = self.surfaces[index].surface_id.clone();
        eprintln!("[oblisk-renderer] {surface_id}: dismissed by the compositor");
        self.hide_popup(index);
        self.latch_popup(index);

        let on_dismiss =
            self.client.scene().surface(&surface_id).and_then(|tree| match tree.properties.get("on_dismiss") {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // A tiling compositor sizes every window: `xdg_toplevel::configure` makes a maximized or
        // fullscreen size binding, not advisory. On niri this is the only branch that ever runs.
        let mut spec = settings_window();
        spec.min_size = Some(SizeHint { width: 320.0, height: 240.0 });
        spec.max_size = Some(SizeHint { width: 1280.0, height: 800.0 });
        assert_eq!(
            toplevel_size_for((nz(1920), nz(1168)), &spec),
            (1920, 1168),
            "the hints never override a configure"
        );
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
        // expected maximum size in the given dimension".
        assert_eq!(toplevel_size_for((None, None), &spec), (400, 900));
    }

    #[test]
    fn a_re_resolve_that_changed_no_window_property_sends_no_requests_at_all() {
        let applied = settings_window();
        assert_eq!(window_update(&applied, &applied.clone()), WindowUpdate::default());
    }

    #[test]
    fn every_window_field_is_pushed_on_its_own_and_only_when_it_moved() {
        // All four, unlike a panel's diff: `xdg-shell.xml` allows `set_app_id`/`set_title` after
        // mapping, and both size hints are ordinary double-buffered requests, so a changed `title`
        // is an in-place update, not a recreate.
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
        // parent bar too, so `popup_done` and the button's `on_click` land in one batch and
        // `visible` alone cannot separate the two cases -- but `popup_done` dispatches before the
        // pointer events that follow it, so the counter has already moved.
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
        // The `grab = false` popup opened by a D-Bus notification: § 6.3 gives it no way to say
        // which monitor it means (see `parent_instance_index`'s ponytail).
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
        // `PopupInner` seeds its pending dimensions at `-1` and reports whatever they hold when
        // `xdg_surface.configure` arrives; a `-1` reaching `WlEglSurface::new` is a crash and the
        // requested size is right there.
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
        // The XML is what makes this a translation, not a fudge: with no edge specified the anchor
        // point is "in the center of the anchor rectangle", and a gravity of `none` centers the
        // surface "over the anchor point on any axis that had no gravity specified".
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
