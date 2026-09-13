//! `xdg_shell`'s client-picked `window` (`xdg_toplevel`) and `popup` (`xdg_popup`) roles,
//! including positioners, size negotiation, and the ADR-0049/0051 dismissal latch. Shared
//! bind/paint/(un)map logic is in `surface`.

use super::*;
use crate::wayland::surface::MapState;
use crate::wayland::surface::Placement;
use crate::wayland::surface::PopupParent;
use crate::wayland::surface::PopupRefusal;
use crate::wayland::surface::TrackedRole;

/// One `popup` visibility decision from object existence and the ADR-0051 latch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopupAction {
    Create,
    Destroy,
    Nothing,
}
/// ADR-0051 decision 2's pure latch machine for `visible`. `dismissed_at` is the pointer
/// count at dismissal; it holds while that count is unchanged (first amendment), since the `visible
/// = false` edge is otherwise unobservable and a bool would latch forever. The caller clears it on
/// that edge.
///
/// `visible=true` with no object creates unless the count is latched; a moved count permits a fresh
/// click. `visible=false` destroys an object and otherwise does nothing. An existing object stays,
/// and this no-op runs for every surface on every capability push.
fn popup_visibility_action(visible: bool, exists: bool, dismissed_at: Option<u64>, pointer_input: u64) -> PopupAction {
    let latched = dismissed_at == Some(pointer_input);
    match (visible, exists) {
        (true, false) if !latched => PopupAction::Create,
        (false, true) => PopupAction::Destroy,
        _ => PopupAction::Nothing,
    }
}
/// Parent instance for a popup (ADR-0051 decision 1). A declared id can expand to one panel per
/// output (ADR-0038 decision 3), so use the arming click's instance. ponytail: without an armed
/// click, such as a `grab = false` D-Bus popup, use the first parent instance. Upgrade: add popup
/// `monitor` and a third selector argument.
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
/// Popup buffer size. Positive configure axes are authoritative because the compositor may slide,
/// flip, or resize for positioner constraints. Non-positive axes use the requested size: SCTK's
/// `PopupInner` starts pending dimensions at `-1`, which would crash `WlEglSurface::new`; clamp to
/// at least 1.
///
/// `requested` is what the positioner was given, not what the spec says: a `Content` axis has no
/// number in the spec at all (`surface::popup_requested_size`).
fn popup_size_for(configured: (i32, i32), requested: (f32, f32)) -> (u32, u32) {
    let axis = |configured: i32, requested: f32| -> u32 {
        if configured > 0 {
            return configured as u32;
        }
        (requested.max(1.0)) as u32
    };
    (axis(configured.0, requested.0), axis(configured.1, requested.1))
}
/// `anchor` to `xdg_positioner`; `Center` is protocol `none`, centered in the anchor rectangle.
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
/// `gravity` to its separate but identical protocol enum; `Center` again maps to `none`,
/// which centers the surface over the anchor point on axes without specified gravity.
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
/// Six independent `constraint_adjustment` booleans to the protocol bitmask; precedence is
/// compositor-defined, so config array order carries no meaning.
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
/// Sends all positioner fields in protocol order, from the one value that is also what a live
/// popup remembers being given ([`Placement`]), so a field cannot be sent without being compared.
/// An open popup's positioner is replaced through `xdg_popup.reposition`; a fresh one gets its at
/// `get_popup`, which consumes it.
///
/// Logical pixels round to `i32` (`x=996.6` becomes 997) because the anchor came from `on_click`
/// (ADR-0050 decision 3). The size `ceil`s instead: an anchor rect names a point and a size names
/// room, and a card measuring 252.48 given 252 loses the half pixel to the surface's own edge,
/// which is the clipping content sizing exists to end. Both clamp to 1, since `set_size` raises
/// `invalid_input` on a zero.
fn configure_positioner(positioner: &XdgPositioner, placement: &Placement) {
    let round = |n: f32| n.round() as i32;
    positioner.set_size((placement.size.0.ceil() as i32).max(1), (placement.size.1.ceil() as i32).max(1));
    positioner.set_anchor_rect(
        round(placement.anchor_rect.x),
        round(placement.anchor_rect.y),
        round(placement.anchor_rect.width).max(1),
        round(placement.anchor_rect.height).max(1),
    );
    positioner.set_anchor(positioner_anchor(placement.anchor));
    positioner.set_gravity(positioner_gravity(placement.gravity));
    positioner.set_constraint_adjustment(positioner_constraint(placement.constraint_adjustment));
    positioner.set_offset(round(placement.offset.x), round(placement.offset.y));
}

/// `xdg_popup.reposition` arrived in xdg-shell version 3. SCTK's `Popup::reposition` silently does
/// nothing below it, which would leave a popup quietly the wrong size, so ask first and say so.
const REPOSITION_SINCE: u32 = 3;
/// Fallback for a compositor-selected window axis without `min_size`. ponytail: fixed 640x480;
/// with no min size, that is the opening size when the first configure leaves an axis zero.
/// Upgrade: an advisory initial size property, or solver-backed `Content` sizing (ADR-0077).
const UNCONFIGURED_WINDOW_SIZE: (f32, f32) = (640.0, 480.0);
/// Toplevel buffer size. `xdg_toplevel::configure` binds maximized and fullscreen sizes, so `Some`
/// axes are authoritative; tiling compositors, including niri, always take this branch. A `None`
/// axis means "the client picks", the ordinary first configure on a floating compositor. Choose
/// `min_size`, then 640x480, then clamp by positive `max_size`; a zero max means unset per
/// `set_max_size`. Clamp both axes to 1 because a zero `wl_egl_window` is invalid.
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
/// Live `xdg_toplevel` changes (ADR-0049 amendment). All protocol fields are diffed because
/// title/app-id and size hints remain double-buffered after mapping; `id` is only reconcile
/// identity. `Option<Option<SizeHint>>` distinguishes unchanged from changed-to-absent, which
/// must reach `set_min_size(None)`/`set_max_size(None)` as protocol unset.
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
/// A [`SizeHint`] in protocol units; `None` remains unset, sent as protocol zero.
fn size_hint_pair(hint: Option<SizeHint>) -> Option<(u32, u32)> {
    hint.map(|hint| (hint.width.max(0.0) as u32, hint.height.max(0.0) as u32))
}

impl App {
    /// [`App::create_surfaces`]'s `window` arm: always track it, but create `xdg_toplevel` only
    /// when visible (ADR-0049 decision 1). The entry lets later re-resolves observe `visible`.
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
            stale: false,
            blur_effect: None,
            last_blur_region: Vec::new(),
        });
        if visible {
            let index = self.surfaces.len() - 1;
            self.show_window(qh, index);
        }
    }

    /// [`App::create_surfaces`]'s `popup` arm: always track it, but create `xdg_popup` only when
    /// visible (ADR-0049/0051). Twenty declared popups cost twenty retained nodes and zero objects.
    /// A startup-visible default-grab popup is refused and logged once because no click supplied a
    /// serial; an undismissable dropdown is worse than a closed one (ADR-0049 amendment). This is
    /// expected and does not fail startup.
    pub(super) fn create_popup(
        &mut self,
        qh: &QueueHandle<App>,
        spec: &PopupSpec,
        instance: &SurfaceInstance,
        visible: bool,
    ) {
        self.surfaces.push(TrackedSurface {
            role: TrackedRole::Popup {
                popup: None,
                // The declaration's own numbers, which for a `Content` axis is zero until the first
                // resolve measures one. Nothing opens before then; `apply_resolved_state` writes
                // the real pair on every pass.
                requested: super::surface::popup_requested_size(
                    spec,
                    LogicalRect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
                ),
                // Nothing is open, so no positioner has been given anything yet.
                positioned: None,
                spec: spec.clone(),
                dismissed_at: None,
                refusal_logged: None,
            },
            bound: None,
            surface_id: instance.instance_id.clone(),
            map_state: MapState::Unmapped,
            null_buffered: false,
            configured_size: (0, 0),
            last_painted: None,
            stale: false,
            blur_effect: None,
            last_blur_region: Vec::new(),
        });
        if visible {
            let index = self.surfaces.len() - 1;
            self.show_popup(qh, index);
        }
    }

    /// Diff a fresh `window` spec and send changed fields. Hidden windows have no object to
    /// update, but retain the latest spec so a title changed three times while closed opens with
    /// the third value (ADR-0049 decision 1).
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
        // Minimum before maximum avoids a transient inverted pair and `invalid_size`; the parser
        // already rejects the final pairing.
        if let Some(min_size) = update.min_size {
            window.set_min_size(size_hint_pair(min_size));
        }
        if let Some(max_size) = update.max_size {
            window.set_max_size(size_hint_pair(max_size));
        }
    }

    /// Apply [`popup_visibility_action`] (ADR-0049 decision 2, ADR-0051 decision 2). Clear the
    /// latch on every `visible = false`, even if the object is already gone: `on_dismiss` may write
    /// false in the same turn and must reopen without waiting for pointer input.
    pub(super) fn apply_popup_visibility(&mut self, index: usize, visible: bool) {
        let TrackedRole::Popup { popup, dismissed_at, spec, requested, positioned, .. } = &self.surfaces[index].role
        else {
            return;
        };
        // An open popup whose placement has moved since its positioner was given one. `Nothing`
        // used to be the whole of the already-open case, which is why a popup kept the size it
        // opened at for as long as it stayed open.
        let moved = (visible && popup.is_some())
            .then(|| Placement {
                size: *requested,
                ..Placement::of(spec, LogicalRect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 })
            })
            .filter(|placement| placement.is_measured() && Some(*placement) != *positioned);
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
            PopupAction::Nothing => {
                if let Some(placement) = moved {
                    self.reposition_popup(index, placement);
                }
            }
        }
    }

    /// Creates the toplevel and its required initial unbuffered commit (ADR-0040 decisions
    /// 4-5, ADR-0049 decision 1), then waits in `AwaitingConfigure`. SCTK acks configure before
    /// the handler. `XdgShell::bind` already picked up `zxdg_decoration_manager_v1` with
    /// `xdg_wm_base`, so `WindowDecorations::RequestServer` plus
    /// [`Window::request_decoration_mode`] is the whole decoration path, with no second global.
    /// No geometry request is needed: the default bounding box fits this edge-to-edge shell, with
    /// no shadow to exclude and no subsurfaces. Missing xdg-shell logs once per attempt and leaves
    /// the window absent, not fatal.
    pub(super) fn show_window(&mut self, qh: &QueueHandle<App>, index: usize) {
        let Some(xdg_shell) = self.xdg_shell.as_ref() else {
            eprintln!(
                "[obelisk-renderer] {}: this compositor advertises no xdg_wm_base, so no window can be created for it",
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
        // The constructor decides whether the decoration object exists; this sets its mode.
        // Accept the compositor's answer; configure logs client-side decoration and remains bare.
        window.request_decoration_mode(Some(DecorationMode::Server));
        window.set_title(spec.title.clone());
        window.set_app_id(spec.app_id.clone());
        // Hints do not clamp layout, but bound the size chosen for `None` configure axes.
        window.set_min_size(size_hint_pair(spec.min_size));
        window.set_max_size(size_hint_pair(spec.max_size));
        // Required initial commit without a buffer; configure then permits attachment.
        window.commit();

        if let TrackedRole::Window { window: slot, .. } = &mut self.surfaces[index].role {
            *slot = Some(window);
        }
        self.surfaces[index].map_state = MapState::AwaitingConfigure;
        eprintln!("[obelisk-renderer] {} creating: visible = true", self.surfaces[index].surface_id);
    }

    /// Destroys the toplevel but keeps tracking so a later `visible = true` rebuilds it
    /// (ADR-0049 decision 1). Clear `configured_size` with the binding; the next toplevel gets its
    /// own configure and cannot paint into a stale one. Release EGL/`wl_egl_window` first; dropping
    /// `Window` then destroys decoration, toplevel, xdg-surface, and wl-surface in protocol order.
    pub(super) fn hide_window(&mut self, index: usize) {
        self.release_blur_effect(index);
        // Child popups must die before their parent xdg-surface.
        self.drop_child_popups(index);
        self.release_bound(index);
        if let TrackedRole::Window { window, .. } = &mut self.surfaces[index].role {
            drop(window.take());
        }
        self.surfaces[index].map_state = MapState::Unmapped;
        self.surfaces[index].null_buffered = false;
        eprintln!("[obelisk-renderer] {} destroyed: visible = false", self.surfaces[index].surface_id);
    }

    /// Creates positioner and popup in protocol order (ADR-0040 decision 2, ADR-0049
    /// decisions 1-2, ADR-0051 decisions 1 and 3). `get_popup` consumes every positioner field.
    /// Use [`Popup::from_surface`], not `Popup::new`: the latter commits before a layer parent is
    /// rooted and causes `invalid_popup_parent`. Request grabs before mapping or get
    /// `invalid_grab`.
    /// SCTK's `Dispatch2<XdgSurface, _>` acks `xdg_surface.configure` before
    /// [`PopupHandler::configure`] (`shell/xdg/popup.rs`), so nothing here acks. The grab is the
    /// one request not wrapped by SCTK: `Popup::xdg_popup()` is the raw-object escape hatch for
    /// `wp-text-input-v3` established by ADR-0009, and is used here only for grab.
    /// An unarmed `grab = true` is refused, producing normal immediate `popup_done` rather than an
    /// undismissable popup (ADR-0049 amendment, ADR-0051 decision 3).
    fn show_popup(&mut self, qh: &QueueHandle<App>, index: usize) {
        let surface_id = self.surfaces[index].surface_id.clone();
        let Some(xdg_shell) = self.xdg_shell.as_ref() else {
            eprintln!(
                "[obelisk-renderer] {surface_id}: this compositor advertises no xdg_wm_base, so no popup can be created for it"
            );
            return;
        };
        let TrackedRole::Popup { spec, requested, .. } = &self.surfaces[index].role else {
            return;
        };
        let spec = spec.clone();
        let placement = Placement::of(&spec, LogicalRect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 });
        // `requested` is what `apply_resolved_state` measured; `Placement::of` above cannot know
        // it, so take the measured pair and keep the placement fields it did read.
        let placement = Placement { size: *requested, ..placement };

        // Nothing measured on a `Content` axis yet, so there is no size to ask for. Decline and
        // let the next pass open it, rather than inventing one the surface would then cut.
        if !placement.is_measured() {
            if self.refusal_is_new(index, PopupRefusal::Unmeasured) {
                eprintln!(
                    "[obelisk-renderer] {surface_id}: sized {:?}, so it is not opened yet. An omitted `width`/`height` \
                     is measured off the resolved tree, and this one has measured nothing on that axis. \
                     Logged once until it opens or `visible` resolves false.",
                    placement.size
                );
            }
            return;
        }

        // Refuse before creating protocol objects.
        let grab = if spec.grab {
            let Some(armed) = self.input_serial.clone() else {
                if self.refusal_is_new(index, PopupRefusal::Unarmed) {
                    eprintln!(
                        "[obelisk-renderer] {surface_id}: `grab = true` and no input event armed a serial this turn, so it is not opened. \
                         A popup may only be opened in response to real user input; open it from an `on_click`, or declare `grab = false`. \
                         Logged once until it opens or `visible` resolves false."
                    );
                }
                return;
            };
            let Some(seat) = self.seat_state.seats().next() else {
                if self.refusal_is_new(index, PopupRefusal::Seatless) {
                    eprintln!(
                        "[obelisk-renderer] {surface_id}: `grab = true` and this compositor advertises no seat, so it is not opened"
                    );
                }
                return;
            };
            Some((seat, armed))
        } else {
            None
        };

        // Parent selection applies with or without a grab; the arming click still decides it
        // (ADR-0051 decision 1).
        let parent_index = parent_instance_index(
            self.surfaces.iter().map(|tracked| tracked.surface_id.as_str()),
            &spec.parent,
            self.input_serial.as_ref().map(|armed| armed.instance_id.as_str()),
        );
        let Some(parent) = parent_index.and_then(|parent| self.surfaces[parent].role.as_popup_parent()) else {
            if self.refusal_is_new(index, PopupRefusal::HiddenParent) {
                eprintln!(
                    "[obelisk-renderer] {surface_id}: its `parent` {:?} names no surface that is currently shown, so it is not opened",
                    spec.parent
                );
            }
            return;
        };
        // Log the selected parent, observable mainly on multi-monitor sessions.
        let parent_id = parent_index.map_or("<none>", |parent| self.surfaces[parent].surface_id.as_str()).to_string();

        let positioner = match XdgPositioner::new(xdg_shell) {
            Ok(positioner) => positioner,
            Err(err) => {
                log_bind_failure(&surface_id, "xdg_wm_base::create_positioner", err);
                return;
            }
        };
        configure_positioner(&positioner, &placement);

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
            // Layer-shell roots the raw popup before the commit, or `invalid_popup_parent`.
            layer.get_popup(popup.xdg_popup());
        }
        if let Some((seat, armed)) = &grab {
            popup.xdg_popup().grab(seat, armed.serial);
        }
        // Required initial unbuffered commit, after all rooting/grab requests.
        popup.wl_surface().commit();
        // `get_popup` copied the positioner, so dropping it is correct.

        if let TrackedRole::Popup { popup: slot, refusal_logged, .. } = &mut self.surfaces[index].role {
            *slot = Some(popup);
            *refusal_logged = None;
        }
        self.surfaces[index].map_state = MapState::AwaitingConfigure;
        // What this popup's positioner now holds. Every later pass compares against it.
        if let TrackedRole::Popup { positioned, .. } = &mut self.surfaces[index].role {
            *positioned = Some(placement);
        }
        eprintln!(
            "[obelisk-renderer] {surface_id} creating: visible = true, anchored to {parent_id}, grab {}",
            if grab.is_some() { "taken" } else { "not requested" }
        );
    }

    /// Give an open popup a new positioner (`xdg_popup.reposition`), which is the only way to
    /// change a size or a placement that `get_popup` already consumed.
    ///
    /// The token is ours to choose and comes back on the resulting `PopupConfigure` as
    /// `ConfigureKind::Reposition`; nothing here needs to correlate them, because the ordinary
    /// configure path already takes whatever size arrives and resizes the EGL window to it. It is
    /// sent anyway rather than left at zero so a compositor's own logs can pair request to answer.
    ///
    /// `positioned` moves forward on the request, not on the answer: it records what this popup's
    /// positioner was told, and a second identical request would be no more true for waiting. The
    /// configure that follows is what actually resizes anything.
    fn reposition_popup(&mut self, index: usize, placement: Placement) {
        let surface_id = self.surfaces[index].surface_id.clone();
        // Read the version out before anything wants `&mut self`; a `Popup` borrow of the role
        // would otherwise outlive the throttle check below.
        let TrackedRole::Popup { popup: Some(popup), .. } = &self.surfaces[index].role else {
            return;
        };
        let version = popup.xdg_popup().version();
        if version < REPOSITION_SINCE {
            if self.refusal_is_new(index, PopupRefusal::Unrepositionable) {
                eprintln!(
                    "[obelisk-renderer] {surface_id}: this compositor bound xdg_popup v{version}, and `reposition` needs \
                     v{REPOSITION_SINCE}, so it keeps the size and place it opened at until it closes. \
                     Logged once until it opens again or `visible` resolves false."
                );
            }
            return;
        }
        let Some(xdg_shell) = self.xdg_shell.as_ref() else {
            return;
        };
        let positioner = match XdgPositioner::new(xdg_shell) {
            Ok(positioner) => positioner,
            Err(err) => {
                log_bind_failure(&surface_id, "xdg_wm_base::create_positioner", err);
                return;
            }
        };
        configure_positioner(&positioner, &placement);
        self.reposition_token = self.reposition_token.wrapping_add(1);
        let token = self.reposition_token;
        let was = if let TrackedRole::Popup { popup: Some(popup), positioned, .. } = &mut self.surfaces[index].role {
            popup.reposition(&positioner, token);
            positioned.replace(placement)
        } else {
            None
        };
        // Rare enough to say every time: a popup only repositions when its content or its anchor
        // actually moved, and if that starts happening on every pass this line is the evidence.
        eprintln!(
            "[obelisk-renderer] {surface_id} repositioned to {:?} from {:?} (token {token})",
            placement.size,
            was.map(|placement| placement.size)
        );
    }

    /// Destroys this popup and nested popups, children first because xdg-shell rejects parent-first
    /// teardown, leaving tracking entries so a later `visible = true` builds fresh objects. Latch
    /// children removed with the parent even without their own `popup_done`; their object is gone
    /// while `visible` remains true and their parent is absent. The latch clears on their
    /// `visible = false` edge (ADR-0051 decision 2).
    fn hide_popup(&mut self, index: usize) {
        for child in self.drop_child_popups(index) {
            self.latch_popup(child);
        }
        self.drop_popup_object(index);
    }

    /// Returns shown descendants deepest-first and destroys their objects. Every surface teardown
    /// needs this because wlroots rejects a parent xdg-surface with live popups. Do not latch here:
    /// parent teardown is not compositor dismissal, and a child should return when its parent does,
    /// including parents reopened by D-Bus without pointer input. While absent, each re-resolve
    /// retries and emits one throttled [`PopupRefusal`].
    pub(super) fn drop_child_popups(&mut self, index: usize) -> Vec<usize> {
        let mut nested = Vec::new();
        self.shown_popups_under(index, &mut nested);
        for &child in &nested {
            self.drop_popup_object(child);
        }
        nested
    }

    /// One popup object teardown, without changing its latch or nesting. Release EGL and
    /// `wl_egl_window`, then drop [`Popup`], whose `Drop` sends `xdg_popup.destroy`.
    fn drop_popup_object(&mut self, index: usize) {
        self.release_bound(index);
        if let TrackedRole::Popup { popup, positioned, .. } = &mut self.surfaces[index].role {
            drop(popup.take());
            // No object, no positioner to have been given anything. The next open sends afresh.
            *positioned = None;
        }
        self.surfaces[index].map_state = MapState::Unmapped;
        self.surfaces[index].null_buffered = false;
        eprintln!("[obelisk-renderer] {} destroyed", self.surfaces[index].surface_id);
    }

    /// Whether this is a new refusal for the popup (ADR-0049 amendment). Different reasons each
    /// log, so fixing `grab` and then hitting a hidden parent is visible.
    fn refusal_is_new(&mut self, index: usize, refusal: PopupRefusal) -> bool {
        let TrackedRole::Popup { refusal_logged, .. } = &mut self.surfaces[index].role else {
            return false;
        };
        refusal_logged.replace(refusal) != Some(refusal)
    }

    /// Stamp ADR-0051 decision 2's latch with the current pointer count; it holds until that count
    /// advances, so dismissal followed by nothing stays latched and a later click reopens.
    fn latch_popup(&mut self, index: usize) {
        let stamp = self.pointer_input_count;
        if let TrackedRole::Popup { dismissed_at, .. } = &mut self.surfaces[index].role {
            *dismissed_at = Some(stamp);
        }
    }

    /// Append shown descendants deepest-first, matching [`App::hide_popup`]. Siblings stay in
    /// tracked order, which is safe because xdg-shell constrains a popup against its parent, not
    /// against siblings under one parent. A popup can hold an object only after its parent does, so
    /// parent cycles, including self-parenting, never open and cannot recurse through this walk.
    pub(super) fn shown_popups_under(&self, index: usize, out: &mut Vec<usize>) {
        let parent_id = self.surfaces[index].surface_id.clone();
        for child in (0..self.surfaces.len()).filter(|&child| child != index).filter(|&child| {
            matches!(&self.surfaces[child].role, TrackedRole::Popup { popup: Some(_), spec, .. } if is_instance_of(&parent_id, &spec.parent))
        }) {
            self.shown_popups_under(child, out);
            out.push(child);
        }
    }
}

/// `xdg_toplevel` handler for `window`.
impl WindowHandler for App {
    /// `xdg_toplevel::close` is a request, not a command: config may ignore it. Do not destroy
    /// here; doing so would leave `visible=true` describing a missing window and the next resolve
    /// would create a second one (ADR-0049 decision 2). Clone the callback before Lua; log and
    /// swallow raises like `fire_on_click`.
    fn request_close(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, window: &Window) {
        let Some(index) = self.index_of_surface(window.wl_surface()) else {
            return;
        };
        let surface_id = self.surfaces[index].surface_id.clone();
        let on_close =
            self.client.scene().surface(&surface_id).and_then(|tree| match tree.properties.get("on_close") {
                // This key is opaque to `layout::node`, as `on_click` also is; this
                // is its only type check.
                Some(Value::Function(on_close)) => Some(on_close.clone()),
                _ => None,
            });
        let Some(on_close) = on_close else {
            eprintln!(
                "[obelisk-renderer] {surface_id}: the compositor asked it to close and no `on_close` declined or accepted; staying open"
            );
            return;
        };
        if let Err(e) = on_close.call::<()>(()) {
            eprintln!("[obelisk-renderer] {surface_id}: on_close raised, ignoring it: {e}");
        }
    }

    /// SCTK has acked this configure. `new_size` may leave axes to the client; client-side
    /// decoration is logged but not drawn (ADR-0040 decision 4); `state`/`capabilities` have no
    /// config binding, while fullscreen/maximized sizes arrive as `Some` axes.
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
                "[obelisk-renderer] {surface_id}: the compositor granted client-side decorations; carrying on undecorated, since this shell draws no titlebar of its own"
            );
        }
        let TrackedRole::Window { spec, .. } = &self.surfaces[index].role else {
            return;
        };
        let (width, height) = toplevel_size_for(configure.new_size, spec);
        self.bind_and_clear(index, width, height);
    }
}

/// `xdg_popup` handler for `popup`.
impl PopupHandler for App {
    /// SCTK has acked the configure. Ignore compositor placement because config has no
    /// binding for it. Only `Initial` is built; `Reactive` needs `set_reactive` and `Reposition`
    /// needs `xdg_popup.reposition`, while a fresh popup per open handles changing anchors.
    fn configure(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, popup: &Popup, configure: PopupConfigure) {
        let Some(index) = self.index_of_surface(popup.wl_surface()) else {
            return;
        };
        let TrackedRole::Popup { requested, .. } = &self.surfaces[index].role else {
            return;
        };
        let (width, height) = popup_size_for((configure.width, configure.height), *requested);
        self.bind_and_clear(index, width, height);
    }

    /// `popup_done` is compositor dismissal, not a request. It is why ADR-0040 uses a real
    /// `xdg_popup` instead of a second `panel`: layer-shell has no compositor-agnostic
    /// click-outside dismissal. Then destroy children/object, latch ADR-0051 decision 2, and call
    /// `on_dismiss` against the already-gone popup. A denied grab arrives here as normal
    /// `popup_done` (ADR-0051 decision 3); clone callbacks and swallow raises. The latch, not this
    /// callback, prevents the re-resolve livelock.
    fn done(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, popup: &Popup) {
        let Some(index) = self.index_of_surface(popup.wl_surface()) else {
            return;
        };
        let surface_id = self.surfaces[index].surface_id.clone();
        eprintln!("[obelisk-renderer] {surface_id}: dismissed by the compositor");
        self.hide_popup(index);
        self.latch_popup(index);

        let on_dismiss =
            self.client.scene().surface(&surface_id).and_then(|tree| match tree.properties.get("on_dismiss") {
                // This key is opaque too.
                Some(Value::Function(on_dismiss)) => Some(on_dismiss.clone()),
                _ => None,
            });
        let Some(on_dismiss) = on_dismiss else {
            // No handler is fine; the latch still prevents a livelock.
            return;
        };
        if let Err(e) = on_dismiss.call::<()>(()) {
            eprintln!("[obelisk-renderer] {surface_id}: on_dismiss raised, ignoring it: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_window() -> WindowSpec {
        WindowSpec {
            id: "settings".to_string(),
            title: "Obelisk settings".to_string(),
            app_id: "obelisk.settings".to_string(),
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
        // ordinary first configure on a floating compositor. `min_size` is the only thing a config
        // can say about a window's size, so it is what the client says back.
        let mut spec = settings_window();
        spec.min_size = Some(SizeHint { width: 320.0, height: 240.0 });
        assert_eq!(toplevel_size_for((None, None), &spec), (320, 240));
        // One axis each way, which is the shape a compositor constraining only width produces.
        assert_eq!(toplevel_size_for((nz(800), None), &spec), (800, 240));
    }

    #[test]
    fn an_unconfigured_axis_with_no_min_size_falls_back_to_the_named_constant() {
        // Its `ponytail:` states the ceiling: a config has nothing else to say here, and a
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
        rematched.app_id = "obelisk.prefs".to_string();
        assert_eq!(
            window_update(&applied, &rematched),
            WindowUpdate { app_id: Some("obelisk.prefs".to_string()), ..WindowUpdate::default() }
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
        // one is absent-versus-present, and dropping a `max_size` from a config has to send
        // the protocol's zero (meaning unset) rather than leaving the old maximum standing.
        let mut applied = settings_window();
        applied.max_size = Some(SizeHint { width: 1280.0, height: 800.0 });
        let fresh = settings_window();

        assert_eq!(window_update(&applied, &fresh), WindowUpdate { max_size: Some(None), ..WindowUpdate::default() });
        assert_eq!(size_hint_pair(None), None, "which `Window::set_max_size` sends as the protocol's zero");
        assert_eq!(size_hint_pair(Some(SizeHint { width: 320.0, height: 240.0 })), Some((320, 240)));
    }

    #[test]
    fn a_visible_popup_with_no_object_is_created_unless_the_latch_is_set() {
        assert_eq!(popup_visibility_action(true, false, None, 4), PopupAction::Create);
        // ADR-0051 decision 2, and the one row the whole latch exists for: a compositor
        // dismissal leaves the resolved tree still saying `visible = true`, so without this the
        // next re-resolve creates a second popup for the same click-outside to dismiss, forever.
        assert_eq!(popup_visibility_action(true, false, Some(4), 4), PopupAction::Nothing);
    }

    #[test]
    fn a_dismissal_with_no_pointer_input_since_holds_the_latch_for_the_generations_life() {
        // The livelock ADR-0051 decision 2 exists to stop, in the config that has no
        // `on_dismiss` at all. Nothing new arrives, so the counter never moves and no re-resolve
        // ever creates a replacement -- not for one turn, but forever.
        for _ in 0..1000 {
            assert_eq!(popup_visibility_action(true, false, Some(9), 9), PopupAction::Nothing);
        }
    }

    #[test]
    fn a_click_arriving_after_the_dismissal_clears_the_latch_in_the_same_turn() {
        // ADR-0051's first amendment. Under a grab niri delivers the closing click to the
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
        // ADR-0051 decision 1. Two monitors, one declared `bar`, and the click decides.
        let instances = ["bar@eDP-1", "bar@DP-1", "menu"];
        assert_eq!(parent_instance_index(instances.into_iter(), "bar", Some("bar@DP-1")), Some(1));
        assert_eq!(parent_instance_index(instances.into_iter(), "bar", Some("bar@eDP-1")), Some(0));
    }

    #[test]
    fn a_popup_with_nothing_armed_falls_back_to_the_first_instance_of_its_parent() {
        // The `grab = false` popup opened by a D-Bus notification: config has no way to say
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
        // A popup parents to either a `panel` or a `window`, and a window's instance carries
        // no `@output` because the compositor places it.
        let instances = ["bar@eDP-1", "settings"];
        assert_eq!(parent_instance_index(instances.into_iter(), "settings", None), Some(1));
    }

    #[test]
    fn a_popup_configure_is_taken_as_given_because_the_compositor_may_have_constrained_it() {
        // `constraint_adjustment` lets the compositor slide, flip or resize the popup to
        // keep it on screen, and the size it lands on is the one that has to be painted.
        assert_eq!(popup_size_for((180, 90), (200.0, 120.0)), (180, 90));
    }

    #[test]
    fn a_popup_configure_with_no_size_falls_back_to_what_the_positioner_asked_for() {
        // `PopupInner` seeds its pending dimensions at `-1` and reports whatever they hold when
        // `xdg_surface.configure` arrives; a `-1` reaching `WlEglSurface::new` is a crash and the
        // requested size is right there.
        assert_eq!(popup_size_for((-1, -1), (200.0, 120.0)), (200, 120));
        assert_eq!(popup_size_for((180, 0), (200.0, 120.0)), (180, 120), "per axis, not all or nothing");
    }

    #[test]
    fn a_popup_never_takes_a_zero_sized_buffer() {
        assert_eq!(popup_size_for((0, 0), (0.0, 0.0)), (1, 1), "a wl_egl_window of 0 is invalid");
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
    fn center_is_the_protocols_none_on_both_requests() {
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
            "the config default is dropdown behaviour, not the protocol's"
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
