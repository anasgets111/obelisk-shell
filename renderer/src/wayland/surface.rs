//! Shared `TrackedRole`/`TrackedSurface`/`MapState` bookkeeping, EGL binding, and
//! create/destroy/paint/(un)map lifecycle. Role-specific behavior is in `layer`, `xdg_shell`, and
//! `lock`.

use super::*;

/// A surface bound to shared EGL after its first configure. Field order is load-bearing:
/// wayland-egl requires `WlEglSurface` to outlive the EGL surface, and Rust drops top to bottom;
/// `khronos_egl::Surface` has no `Drop`, so [`App::destroy_surface_by_id`] destroys it explicitly.
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1;

pub(super) struct BoundSurface {
    egl_surface: EglSurface,
    #[allow(dead_code)]
    native_window: WlEglSurface,
}
/// Logs a bind-time failure; `surface_id` is `"{id}@{output}"` (ADR-0038).
pub(super) fn log_bind_failure(surface_id: &str, stage: &str, err: impl std::fmt::Display) {
    eprintln!("[oblisk-renderer] {surface_id}: {stage} failed: {err}");
}
/// § 6's `visible` state. Three states are required because showing commits without a buffer and
/// waits for configure before drawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MapState {
    /// `visible` is false: either no buffer was attached yet, or the role object was destroyed
    /// (ADR-0088, ADR-0049 decision 1). Nothing may be painted here; a panel may have no
    /// `wl_surface` left to paint onto (see [`App::unmap`]).
    Unmapped,
    /// The map commit is out; configure has not arrived.
    AwaitingConfigure,
    /// Configured; [`App::paint_surface`] may attach a buffer, and `swap_buffers` commits state.
    Mapped,
}
impl MapState {
    /// Whether this surface will put a frame on screen: the one predicate PBA's expected set
    /// and its drawn set must agree on. See [`presenting_surface_ids`].
    fn presents(self) -> bool {
        self != MapState::Unmapped
    }
}
/// A tracked `wl_surface`'s role and last-applied spec (§ 6, ADR-0040 decision 1). One enum keeps
/// EGL, paint, input, and PBA paths on the shared `App::surfaces` index. Role objects exist only
/// while shown (ADR-0049 decision 1, ADR-0088), hence the `Option`s.
pub(super) enum TrackedRole {
    Panel {
        /// `None` after `visible = false`; ADR-0038's null-buffer remap was retained originally,
        /// but niri does not honor it. Wire trace: remap commit, configure, ack, and buffer attach
        /// all follow the spec, yet the surface never returns. A notification's first display and
        /// every later one were invisible; reopening a hidden bar stayed blank (ADR-0088).
        layer: Option<LayerSurface>,
        /// Instance output, reused by [`App::show_panel`] (ADR-0038 decision 3: the compositor
        /// never picks it).
        output: wl_output::WlOutput,
        /// `layer::spec_update`'s diff baseline and the spec used by
        /// [`App::apply_exclusive_zone`] after configure (ADR-0038 decision 2).
        spec: PanelSpec,
        /// Output logical size for `SizeMode::Percent`. Do not use `SurfaceInstance::available`:
        /// `set_instance_size` replaces it with the compositor size, which would shrink a panel on
        /// every push. Panel-only; a window has no `width`/`height` (§ 6).
        output_size: layout::LogicalSize,
    },
    Window {
        /// `None` when hidden: no `xdg_toplevel`, `xdg_surface`, or `wl_surface` exists
        /// (ADR-0049 decision 1).
        window: Option<Window>,
        /// `xdg_shell::window_update` baseline, retained while hidden so [`App::show_window`] uses
        /// the last re-resolved spec.
        spec: WindowSpec,
    },
    Popup {
        /// `None` when hidden. `get_popup` consumes `xdg_positioner`, so every open builds a fresh
        /// positioner, `wl_surface`, and `xdg_popup` (ADR-0049).
        popup: Option<Popup>,
        /// Whole spec for the next open; `xdg_positioner` fields are consumed at creation, so no
        /// live diff exists.
        spec: PopupSpec,
        /// The size the next open asks the positioner for, in logical pixels: the spec's declared
        /// numbers, with every `SizeMode::Content` axis replaced by what the resolved tree measured
        /// on the pass this was written. The spec cannot hold it -- a `Content` axis has no number
        /// until the tree is solved -- and the positioner cannot wait for it, since `get_popup`
        /// consumes the whole positioner at creation.
        ///
        /// Zero on an axis means nothing has been measured yet, which is what a hidden popup's
        /// frozen 0x0 tree reports (ADR-0124). [`App::show_popup`] declines to open on that rather
        /// than substituting a stale number: the pass that reveals the popup is the pass that
        /// measures it, so the size is there by the time it is needed, and if it somehow is not,
        /// waiting one more pass is better than opening at the wrong size.
        requested: (f32, f32),
        /// What the *live* popup's positioner was given, or `None` while nothing is open. A pass
        /// whose [`Placement`] differs from this is one the open popup is the wrong size or in the
        /// wrong place for, and [`App::reposition_popup`] is what closes that gap: `get_popup`
        /// consumed the positioner at creation, so the only way to change any of it afterwards is
        /// `xdg_popup.reposition`.
        ///
        /// Without it a popup keeps whatever it opened at. That is invisible for a menu, whose
        /// contents are fixed, and wrong for anything measured: a battery tooltip opened on
        /// "69% charging" cannot grow when the estimate arrives and becomes
        /// "69% charging, 1h 20m to full", so the hover that was already up cuts the words off.
        positioned: Option<Placement>,
        /// ADR-0051 decision 2 latch: pointer count at compositor dismissal. It blocks replacement
        /// while unchanged, preventing a `popup_done`/`visible = true` click-outside livelock. A
        /// count, not a bool (first amendment), because the `visible = false` clear edge is not
        /// observable here.
        dismissed_at: Option<u64>,
        /// Last [`App::show_popup`] refusal logged for this visible run; throttle repeats
        /// (ADR-0049 amendment). Cleared on create or `visible = false`.
        refusal_logged: Option<PopupRefusal>,
    },
    Lock {
        /// Output covered by this lock instance; § 6 gives `lock` no `monitor` property.
        output: wl_output::WlOutput,
        /// `None` until the lock is held (ADR-0052 decision 2). Dropping sends
        /// `ext_session_lock_surface_v1.destroy` and exposes a solid color; cleared on output
        /// removal or lock end.
        surface: Option<SessionLockSurface>,
    },
}
impl TrackedRole {
    /// This surface's `wl_surface`, or `None` for a hidden window/popup.
    pub(super) fn wl_surface(&self) -> Option<&wl_surface::WlSurface> {
        match self {
            TrackedRole::Panel { layer, .. } => layer.as_ref().map(LayerSurface::wl_surface),
            TrackedRole::Window { window, .. } => window.as_ref().map(WaylandSurface::wl_surface),
            TrackedRole::Popup { popup, .. } => popup.as_ref().map(WaylandSurface::wl_surface),
            TrackedRole::Lock { surface, .. } => surface.as_ref().map(SessionLockSurface::wl_surface),
        }
    }

    /// This surface as an `xdg_popup` parent, or `None` if hidden or unsupported (§ 6,
    /// ADR-0051 decision 1).
    pub(super) fn as_popup_parent(&self) -> Option<PopupParent> {
        match self {
            TrackedRole::Panel { layer, .. } => layer.as_ref().map(|layer| PopupParent::Layer(layer.clone())),
            TrackedRole::Window { window, .. } => window.as_ref().map(|w| PopupParent::Xdg(w.xdg_surface().clone())),
            TrackedRole::Popup { popup, .. } => popup.as_ref().map(|p| PopupParent::Xdg(p.xdg_surface().clone())),
            // Lock surfaces are neither accepted parent type (`xdg_surface` or
            // `zwlr_layer_surface_v1`); while locked, only lock surfaces show (ADR-0042).
            TrackedRole::Lock { .. } => None,
        }
    }
}
/// Why [`App::show_popup`] declined a popup; used to throttle repeated refusal logs
/// (ADR-0049 amendment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PopupRefusal {
    /// `grab = true` without a serial this turn (ADR-0051 decision 3).
    Unarmed,
    /// `grab = true` but no compositor seat exists.
    Seatless,
    /// § 6's `parent` names no shown surface, commonly a hidden parent window.
    HiddenParent,
    /// A `Content` axis with nothing measured on it yet, so there is no size to ask the positioner
    /// for. Ordinarily impossible -- the pass that makes a popup visible is the pass that measures
    /// it -- and a standing one means the tree resolves to nothing on that axis.
    Unmeasured,
    /// The compositor bound `xdg_popup` below version 3, which is where `reposition` was added, so
    /// an open popup cannot be resized or moved and keeps what it opened at until it closes.
    Unrepositionable,
}
/// Everything an `xdg_positioner` is told, as one value: the size to ask for and the five fields
/// that place it. It exists so that what is sent and what is remembered cannot drift apart -- a
/// popup repositions when this differs from what its live positioner was given, and a field added
/// here is compared by the same edit that starts sending it.
///
/// All `Copy`, so remembering one costs nothing. `PopupSpec`'s own `id` and `parent` are not here:
/// they are structural (ADR-0051 decision 1), and changing either is a different popup rather than
/// a repositioned one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Placement {
    pub(super) size: (f32, f32),
    pub(super) anchor_rect: crate::text::snap::LogicalRect,
    pub(super) anchor: node::PopupAnchor,
    pub(super) gravity: node::PopupAnchor,
    pub(super) constraint_adjustment: node::ConstraintAdjustment,
    pub(super) offset: node::PopupOffset,
}

impl Placement {
    /// What `spec` asks for, with its `Content` axes resolved against `root` -- the box the layout
    /// pass measured for this surface (see [`popup_requested_size`]).
    pub(super) fn of(spec: &PopupSpec, root: crate::text::snap::LogicalRect) -> Self {
        Self {
            size: popup_requested_size(spec, root),
            anchor_rect: spec.anchor_rect,
            anchor: spec.anchor,
            gravity: spec.gravity,
            constraint_adjustment: spec.constraint_adjustment,
            offset: spec.offset,
        }
    }

    /// Whether both axes have a size to ask for. A `Content` axis reads zero until the tree is
    /// measured, and `set_size` raises `invalid_input` on a zero.
    pub(super) fn is_measured(&self) -> bool {
        self.size.0 > 0.0 && self.size.1 > 0.0
    }
}

/// What the next `xdg_positioner::set_size` should ask for: the spec's own numbers, with each
/// `Content` axis taken from the box the layout pass measured for this surface's root.
///
/// `ceil`, not `round`: a card measuring 252.48 needs 253 or the half pixel it asked for is the
/// half pixel the surface cuts off, which is the whole failure this sizing exists to end. Rounding
/// is right for an anchor rect, which names a point, and wrong for an extent, which names room.
///
/// A `Content` axis the tree reports as zero stays zero, for [`App::show_popup`] to decline on.
pub(super) fn popup_requested_size(spec: &PopupSpec, root: crate::text::snap::LogicalRect) -> (f32, f32) {
    let axis = |mode: node::SizeMode, measured: f32| match mode {
        node::SizeMode::Pixels(px) => px,
        _ => measured.max(0.0).ceil(),
    };
    (axis(spec.width, root.width), axis(spec.height, root.height))
}

/// Popup roots: `Popup::from_surface` roots windows/nested popups at creation; a panel has no
/// `xdg_surface`, so layer-shell's `get_popup` must root the raw popup before its initial commit.
pub(super) enum PopupParent {
    Layer(LayerSurface),
    Xdg(xdg_surface::XdgSurface),
}
pub(super) struct TrackedSurface {
    pub(super) role: TrackedRole,
    pub(super) bound: Option<BoundSurface>,
    /// Supervisor § 14 `surface_id`: `"{id}@{output}"` for panels, bare `id` for windows; shared
    /// by Lua, the retained scene, Wayland, and PBA.
    pub(super) surface_id: String,
    pub(super) map_state: MapState,
    /// Null buffer committed in PBA mode (Supervisor § 14.2); hidden windows never set it because
    /// they receive no configure. Always false outside candidates.
    pub(super) null_buffered: bool,
    /// Latest configure size for [`App::activate_draw`]'s EGL bind; candidate mode records it
    /// before binding (see [`App::bind_and_clear`]).
    pub(super) configured_size: (u32, u32),
    /// Last display list and size. The size matters because a resized EGL surface has empty
    /// buffers; clear on rebind or any branch that cannot prove the pixels still match, or a stale
    /// frame can remain with no redraw trigger.
    pub(super) last_painted: Option<((u32, u32), layout::paint::DisplayList)>,
    /// The pixels on screen are stale although `last_painted` still describes them, so the next
    /// paint must run even against an identical list (ADR-0182). Set when a decode lands for a
    /// file this surface draws: the list is unchanged, the texture behind it is not.
    ///
    /// Kept apart from clearing `last_painted` because that list is also the pin set
    /// `ImageCache::trim` reads. Dropping it unpinned every image a mapped surface was showing for
    /// the width of one repaint, and a wallpaper mid-dissolve repaints every frame -- so `trim`
    /// kept landing in that window, evicting a whole picker's thumbnails, which then re-decoded,
    /// landed, and unpinned everything again.
    pub(super) stale: bool,
    /// This surface's `ext_background_effect_surface_v1` and the id of the `wl_surface` it names
    /// (ADR-0195). `None` on a compositor without the protocol, and on every surface whose tree
    /// never sets `blur`.
    ///
    /// The id is carried because the object goes inert when its `wl_surface` is destroyed, and
    /// calling `set_blur_region` on an inert one is a protocol error that takes the whole client
    /// down. A `TrackedSurface` outlives its `wl_surface`: a tooltip is destroyed and recreated on
    /// every hover, keeping its index and getting a fresh surface, and `unmap` -- which does drop
    /// this -- returns early for anything that is not a panel. Comparing ids at the push is what
    /// makes every teardown path safe rather than the ones that were remembered.
    pub(super) blur_effect: Option<(ExtBackgroundEffectSurfaceV1, wayland_client::backend::ObjectId)>,
    /// The region last sent, so an unchanged one is not resent. `apply_input_region` deliberately
    /// does not diff, and says why: one `wl_region` round trip is cheaper than the repaint that
    /// follows it. That reasoning was about a handful of rectangles. A rounded card is a couple of
    /// dozen, several cards more, and a card that only fades has the same region on every frame of
    /// the fade -- so this one compares.
    pub(super) last_blur_region: Vec<crate::text::snap::PhysicalRect>,
}
/// Which surfaces a narrowed repaint must cover: the ones a tick advanced, plus every surface
/// already marked `stale`.
///
/// Pure so the narrowing is testable -- `TrackedSurface` holds Wayland objects no test can build,
/// and this is the part that was wrong. `stale` means a surface differs for a reason its tree
/// cannot show, so a repaint chosen by tree identity alone passes it over: a decode refused for
/// pool capacity armed a frame callback, ticked nothing, and was narrowed straight back out
/// (ADR-0185).
fn narrowed_repaint_targets(ticked: &[String], stale: &[String]) -> Vec<String> {
    let mut targets = ticked.to_vec();
    for id in stale {
        if !targets.iter().any(|target| target == id) {
            targets.push(id.clone());
        }
    }
    targets
}

/// What changed on one turn of the main loop, as the repaint decision reads it.
pub(super) struct TurnChanges {
    /// A re-resolve ran, so any tree in the scene may differ.
    pub(super) passed: bool,
    /// A tween tick advanced at least one instance. Never true on the same turn as `passed`.
    pub(super) ticked: bool,
    /// Some mapped surface owes a repaint its tree cannot ask for (ADR-0185).
    pub(super) stale: bool,
    /// A keystroke reached a field, moving a caret `field_focus_for` draws.
    pub(super) typed: bool,
    /// A decode landed, invalidating by file across every list that draws it.
    pub(super) landed: bool,
}

/// How wide this turn's repaint has to be.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Repaint {
    Nothing,
    /// The ticked instances plus whatever is `stale`; see [`narrowed_repaint_targets`].
    Narrowed,
    Everything,
}

/// A turn that only ticked owes the screen exactly the surfaces it advanced, and `tick` just
/// named them. Every other reason to repaint is scene-wide: a pass can change any tree, a
/// keystroke moves a caret through `field_focus_for`, and a landed decode invalidates by file
/// across every list that draws it.
///
/// So `passed` rules the narrowing out on its own, and it has to be the pass flag rather than the
/// "did anything change" one the main loop also derives from the tick. Reading the derived flag
/// let a pass that changed a panel be narrowed down to an unrelated `stale` wallpaper, and the
/// panel's own repaint was simply dropped: the tick list it narrowed by was empty, because a turn
/// that re-resolves does not tick.
///
/// A surface left `stale` by a decode turned away for capacity owes a repaint that no tree and no
/// landing can ask for, so it is its own reason to reach one (ADR-0185).
pub(super) fn repaint_for_turn(changes: TurnChanges) -> Repaint {
    if changes.passed || changes.typed || changes.landed {
        Repaint::Everything
    } else if changes.ticked || changes.stale {
        Repaint::Narrowed
    } else {
        Repaint::Nothing
    }
}

/// Which surfaces owe a protocol-state push this turn.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum StateScope {
    /// Every tracked surface, because a pass can change any tree.
    Everything,
    /// The instances a tick named, and no others.
    Ticked,
    Nothing,
}

/// The protocol-state half of a turn: what [`App::apply_resolved_state`] is run over, and whether
/// ADR-0051's popup latch still has to be looked at for the popups that misses.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct SurfaceStateWork {
    pub(super) scope: StateScope,
    pub(super) popup_latch: bool,
}

/// `apply_resolved_state` is a role spec parse and an undiffed `wl_region` round trip per surface.
/// A pass earns that for all of them. A tick earns it for the instances it named and no others:
/// `Scene::tick` mutates only the trees it returns, so every other surface still has the tree its
/// last push came from. Eighteen mapped surfaces at 60 Hz is otherwise seventeen region round
/// trips a frame for surfaces the narrowed repaint will not even paint.
///
/// The latch is the exception, and it is an exception because it does not come from the scene at
/// all. A press or release arms `input_serial` and bumps `pointer_input_count`, and the loop
/// clears the serial at the end of that same turn (ADR-0049 amendment). A click whose handler
/// writes no signal -- `on_click` setting an already-true `visible` -- re-resolves nothing, so
/// nothing would look at whether the compositor has dismissed a popup the config still calls
/// visible. That reopen used to depend on some unrelated surface happening to be mid-tween.
pub(super) fn surface_state_for_turn(passed: bool, ticked: bool, armed_input: bool) -> SurfaceStateWork {
    let scope = match (passed, ticked) {
        (true, _) => StateScope::Everything,
        (false, true) => StateScope::Ticked,
        (false, false) => StateScope::Nothing,
    };
    // `Everything` has already visited every popup with this turn's serial in hand.
    let popup_latch = armed_input && scope != StateScope::Everything;
    SurfaceStateWork { scope, popup_latch }
}

/// Initial § 5.1 `visible`, with a role-aware fallback when startup apply has no tree
/// (`Scene::apply` rolled back): panels default visible to keep the shell up, painting nothing
/// until the next re-resolve; windows/popups default hidden (ADR-0049 decision 1), and locks have
/// no `visible` property (§ 6, ADR-0042).
fn starting_visible(resolved: Option<bool>, roster: &SurfaceSpec) -> bool {
    resolved.unwrap_or(match roster {
        SurfaceSpec::Panel(_) => true,
        SurfaceSpec::Window(_) | SurfaceSpec::Popup(_) | SurfaceSpec::Lock(_) => false,
    })
}
/// Re-derive a surface spec from resolved properties, using `roster` only for the § 6 role and log
/// label (ADR-0049 amendment). `kind` built the roster, so taking the role from properties could
/// hide a reconcile bug. [`App::create_surfaces`] is the caller; later passes parse inline by role.
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
/// Surface ids a PBA Candidate announces in `ReadySignal` and draws on `ActivateDraw`, using the
/// same [`MapState::presents`] predicate. They must match: `drive_handshake` otherwise waits past
/// `evidence_timeout` for an announced-but-undrawn surface or aborts with
/// `PbaFailure::UnexpectedEvidence` for an unannounced frame. Hidden panels stage but do not
/// present; hidden windows/popups have no role object, and Candidates freeze popup visibility.
/// Empty is legal; the evidence loop exits immediately.
fn presenting_surface_ids<'a>(surfaces: impl Iterator<Item = (&'a str, MapState)>) -> Vec<String> {
    surfaces.filter(|(_, state)| state.presents()).map(|(id, _)| id.to_string()).collect()
}
/// PBA § 14.2 staging gate. It takes `(null_buffered, exists)`: a hidden window has no
/// `xdg_toplevel`, so `null_buffered` stays false forever and a plain `all(null_buffered)` would
/// hang `ready_timeout`. A `panel` always gets a configure, created at startup even when `visible`
/// is false. A no-object surface is complete by construction; a shown window also attaches a
/// null buffer on its first configure. Popup visibility is frozen during the handshake.
/// [`presenting_surface_ids`] uses the matching `Unmapped` filter.
fn candidate_has_staged(surfaces: impl Iterator<Item = (bool, bool)>) -> bool {
    surfaces.into_iter().all(|(null_buffered, exists)| null_buffered || !exists)
}

impl App {
    /// Track each evaluated instance (ADR-0038 decision 1, ADR-0049 decision 1). Panels create
    /// their layer object regardless of `visible`; windows/popups create only when visible, through
    /// the same show paths used later. `specs` supplies roster/role, while resolved properties
    /// supply fields; signal-bound roster placeholders are unsafe for popup positioners, so later
    /// passes re-derive them. Neither declarations nor roles change on a re-resolve (ADR-0049
    /// decision 3). Missing declarations log and skip. Startup passes all instances;
    /// hotplug passes only additions.
    pub(super) fn create_surfaces(
        &mut self,
        qh: &QueueHandle<App>,
        specs: &[SurfaceSpec],
        instances: &[SurfaceInstance],
    ) {
        // Re-read outputs because this also runs after hotplug.
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
                eprintln!(
                    "[oblisk-renderer] instance {:?} has no matching declaration; skipping",
                    instance.instance_id
                );
                continue;
            };
            // `Scene::surface` returns an owned tree, ending the client borrow before creation.
            let tree = self.client.scene().surface(&instance.instance_id);
            let visible = starting_visible(tree.as_ref().map(|tree| tree.visible), roster);
            // Use resolved properties, not the raw roster. For popups this is permanent: each
            // `PopupSpec` field is consumed by `get_popup`, and no `xdg_popup.reposition` exists,
            // so a raw signal placeholder (`DEFERRED_POPUP_EXTENT`, 1x1 at 0,0) lasts its life.
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
        // On monitor hotplug, give the newly advertised output its lock surface (ADR-0042), or
        // the compositor paints a solid color there. No-op without a held lock.
        self.ensure_lock_surfaces(qh);
    }

    /// Frees rendering, EGL, and `wl_egl_window`, leaving the role object untouched. The caller
    /// owns its later drop; hidden windows keep their tracking entry (ADR-0049 decision 1).
    pub(super) fn release_bound(&mut self, index: usize) {
        let Some(bound) = self.surfaces[index].bound.take() else {
            return;
        };
        // `khronos_egl::Surface` has no `Drop`; destroy it before `wl_egl_window`, or each
        // unplugged monitor/closed window leaks an EGL surface. `ensure_bound` creates both, but
        // keep the guard so a mismatch leaks rather than panics.
        if let Some(egl) = self.egl.as_ref()
            && let Err(err) = egl.instance.destroy_surface(egl.display, bound.egl_surface)
        {
            log_bind_failure(&self.surfaces[index].surface_id, "eglDestroySurface", err);
        }
        // `BoundSurface`'s drop sends `wl_egl_window_destroy`.
        drop(bound);
        self.surfaces[index].configured_size = (0, 0);
    }

    /// Destroys one surface instance (ADR-0038 decision 3). An unplugged monitor produces both
    /// `zwlr_layer_surface_v1::closed` and `OutputHandler::output_destroyed`, in either order; the
    /// no-op handles whichever callback arrives second. Explicit order matters because
    /// `TrackedSurface` declares `role` before `bound`: drop child popups, `eglDestroySurface`,
    /// `wl_egl_window_destroy`, then role and `wl_surface`. Both protocols require that order;
    /// xdg-shell rejects a parent with live popups, and SCTK preserves role-before-surface.
    pub(super) fn destroy_surface_by_id(&mut self, instance_id: &str) {
        let Some(index) = self.surfaces.iter().position(|s| s.surface_id == instance_id) else {
            return;
        };
        // Drop children before `remove` invalidates indices and before the parent dies.
        self.drop_child_popups(index);
        self.release_bound(index);
        // Send the effect's `destroy` while its `wl_surface` is still alive. Dropping the proxy
        // does not send it, so an unplugged output would otherwise leave one inert object per
        // surface on the connection for the rest of the session.
        if let Some((effect, _)) = self.surfaces[index].blur_effect.take() {
            effect.destroy();
        }
        let TrackedSurface { role, surface_id, .. } = self.surfaces.remove(index);
        drop(role);
        // `App::surfaces` and `Scene::surfaces` are different maps; dropping the tracked surface
        // leaves the retained tree behind unless it is dropped here too.
        self.client.forget_surface(&surface_id);
        eprintln!("[oblisk-renderer] {surface_id} destroyed: its output is gone");
    }

    /// A configure records the compositor size, updates scene geometry and exclusive zone, binds
    /// EGL, and paints. PBA mode stops after a null-buffer commit, deferring the real bind to
    /// [`App::activate_draw`] (Supervisor § 14.2). Layer-shell and xdg-shell share this path
    /// because both require an initial unbuffered commit (ADR-0040 decision 4). The callers differ
    /// only in size source: layer-shell supplies it, while a toplevel's `None` axes may be chosen
    /// by the client (see `xdg_shell::toplevel_size_for`).
    pub(super) fn bind_and_clear(&mut self, index: usize, width: u32, height: u32) {
        self.surfaces[index].configured_size = (width, height);
        // The startup resolve used output size; replace it with the granted size and dirty the
        // scene. This paint uses the old resolve; the next poll turn applies the corrected one;
        // re-resolving here would run once per configure instead of once per startup burst
        // (ADR-0044 decision 2).
        self.client.set_instance_size(
            &self.surfaces[index].surface_id,
            layout::LogicalSize { width: width as f32, height: height as f32 },
        );
        // No buffer may attach before this first or remap configure; everything below may draw.
        if self.surfaces[index].map_state == MapState::AwaitingConfigure {
            self.surfaces[index].map_state = MapState::Mapped;
        }
        self.apply_exclusive_zone(index);
        // Apply resolved state on first configure too: a full transparent panel can otherwise mark
        // the scene clean before its input region is ever set and swallow clicks behind it.
        self.apply_resolved_state(index);

        if self.is_pba_candidate {
            // Cloned rather than borrowed: a `wl_surface` proxy is a refcounted handle, and holding
            // a borrow of `self.surfaces` across the `null_buffered` write below would not compile.
            let surface = self.surfaces[index].role.wl_surface().cloned();
            if let Some(surface) = surface.filter(|_| self.surfaces[index].map_state.presents()) {
                if !self.surfaces[index].null_buffered {
                    surface.attach(None, 0, 0);
                    self.surfaces[index].null_buffered = true;
                }
                // Every candidate configure needs a commit; no `swap_buffers` exists until
                // `ActivateDraw` to carry staged state.
                surface.commit();
            } else {
                // A hidden surface already has the invisible PBA state; mark staged without wire
                // traffic and let `presenting_surface_ids` omit it.
                self.surfaces[index].null_buffered = true;
            }
            self.maybe_send_ready_signal();
            return;
        }

        // A remap commit may be awaiting configure, so reject every state except `Mapped`.
        if self.surfaces[index].map_state != MapState::Mapped {
            return;
        }

        if !self.ensure_bound(index) {
            return;
        }
        // Resize the existing `wl_egl_window` on a mode or neighboring-zone change; rebinding is
        // unnecessary because both EGL objects remain valid.
        if let Some(bound) = self.surfaces[index].bound.as_ref() {
            bound.native_window.resize(width.max(1) as i32, height.max(1) as i32, 0, 0);
        }
        self.paint_surface(index);
    }

    /// Apply resolved state to every tracked surface after a changed scene; ADR-0044 decision 2
    /// has one dirty flag for the whole scene.
    pub(super) fn apply_resolved_surface_state(&mut self) {
        for index in 0..self.surfaces.len() {
            self.apply_resolved_state(index);
        }
    }

    /// The same, for the instances a tween tick advanced.
    ///
    /// A tick moves the trees it names and no others, so the rest hold the fields, region and
    /// visibility they were last pushed. Re-deriving those costs a role spec parse and a
    /// `wl_region` create/add/set/destroy per surface, and `apply_input_region` deliberately does
    /// not diff -- a reasonable trade when a GPU repaint follows, which for a narrowed tick is
    /// exactly what does not. Eighteen mapped surfaces at 60 Hz make that seventeen round trips a
    /// frame for surfaces nothing is going to paint.
    pub(super) fn apply_resolved_surface_state_for(&mut self, instance_ids: &[String]) {
        for index in 0..self.surfaces.len() {
            if instance_ids.iter().any(|id| *id == self.surfaces[index].surface_id) {
                self.apply_resolved_state(index);
            }
        }
    }

    /// The ADR-0051 latch, for the popups this turn's [`StateScope`] did not reach; see
    /// [`surface_state_for_turn`] for why a click owes this and the scene does not.
    ///
    /// Popups only, and visibility only. The role fields and the input region are what a tick has
    /// no business re-deriving; whether the compositor dismissed a popup the config still calls
    /// visible is not something the scene can say.
    pub(super) fn apply_popup_visibility_for_armed_input(&mut self) {
        for index in 0..self.surfaces.len() {
            if !matches!(self.surfaces[index].role, TrackedRole::Popup { .. }) {
                continue;
            }
            let surface_id = self.surfaces[index].surface_id.clone();
            let Some(visible) = self.client.scene().surface(&surface_id).map(|tree| tree.visible) else {
                continue;
            };
            self.apply_popup_visibility(index, visible);
        }
    }

    /// Push a resolved root's live protocol fields, input region, and visibility
    /// (ADR-0038 decision 2, ADR-0049 decisions 1-2). Window fields must come from the resolved
    /// `WindowSpec`: raw evaluation values would freeze signal-bound `title`s. The socket parser
    /// [`crate::socket::surface_specs`] still reads unresolved properties, which is right for a
    /// panel's topology fields but wrong for a window's live fields. Push before
    /// `apply_visibility`,
    /// so a newly shown window uses this pass's spec; callers commit all staged state together,
    /// while create/destroy/map/unmap commit by definition.
    fn apply_resolved_state(&mut self, index: usize) {
        let surface_id = self.surfaces[index].surface_id.clone();
        // `Scene::surface` lends its tree, so what the tree is read for is taken here and the
        // borrow ends with this block; the role updates below write through `&mut self`. Exactly
        // one spec is parsed -- the one this surface's role calls for.
        let (panel, window, popup, tree_rect, regions, visible, blur) = {
            let Some(tree) = self.client.scene().surface(&surface_id) else {
                // Startup/apply failure or rollback (`Scene::apply` restores its prior state): keep
                // the last applied fields rather than pushing defaults over a working surface.
                return;
            };
            let role = &self.surfaces[index].role;
            (
                matches!(role, TrackedRole::Panel { .. }).then(|| node::panel_spec(&tree.properties)),
                matches!(role, TrackedRole::Window { .. }).then(|| node::window_spec(&tree.properties)),
                matches!(role, TrackedRole::Popup { .. }).then(|| node::popup_spec(&tree.properties)),
                tree.rect,
                layout::overlay_input_regions(tree, 1.0),
                tree.visible,
                layout::blur_regions(tree, 1.0),
            )
        };

        match panel {
            Some(Ok(fresh)) => self.apply_spec_change(index, fresh),
            Some(Err(err)) => eprintln!(
                "[oblisk-renderer] {surface_id}: re-resolved panel properties are invalid, keeping the last applied ones: {err}"
            ),
            None => {}
        }
        match window {
            Some(Ok(fresh)) => self.apply_window_change(index, fresh),
            Some(Err(err)) => eprintln!(
                "[oblisk-renderer] {surface_id}: re-resolved window properties are invalid, keeping the last applied ones: {err}"
            ),
            None => {}
        }
        match popup {
            // Store, do not diff: `get_popup` consumes every positioner field and no
            // `xdg_popup.reposition` exists. The next open uses this pass's `anchor_rect`
            // (ADR-0049 amendment), including a click-written state signal.
            Some(Ok(fresh)) => {
                let size = popup_requested_size(&fresh, tree_rect);
                if let TrackedRole::Popup { spec, requested, .. } = &mut self.surfaces[index].role {
                    *spec = fresh;
                    *requested = size;
                }
            }
            Some(Err(err)) => eprintln!(
                "[oblisk-renderer] {surface_id}: re-resolved popup properties are invalid, keeping the last applied ones: {err}"
            ),
            None => {}
        }
        // Locks have no config-settable protocol field: only `ack_configure` exists and size
        // arrives in configure. The create path parses a spec only for the role match; input region
        // handling still runs.
        self.apply_input_region(index, regions);
        self.apply_blur_region(index, blur);
        self.apply_visibility(index, visible);
    }

    /// Set the per-surface input region from the resolved tree (§ 5.1, ADR-0038 decision 5): no
    /// visible children means pass-through, a full child covers the surface, and intermediate
    /// content gets its visible geometry. Scale is `1.0` because no buffer scale is set. Do not
    /// diff against the last region: the following GPU repaint costs more than one `wl_region`
    /// round trip. That holds because every caller is a surface about to paint -- see
    /// [`App::apply_resolved_surface_state_for`], which is what keeps a tick's frame from paying
    /// this for the surfaces it did not move. Skip a hidden window with no `wl_surface`; its first
    /// post-show re-resolve sets the region.
    fn apply_input_region(&mut self, index: usize, regions: Vec<crate::text::snap::PhysicalRect>) {
        let Some(surface) = self.surfaces[index].role.wl_surface().cloned() else {
            return;
        };
        let region = match Region::new(&self.compositor_state) {
            Ok(region) => region,
            Err(e) => {
                // `CompositorState::bind` already proved the compositor exists; keep the shell up
                // if region creation nevertheless fails.
                log_bind_failure(&self.surfaces[index].surface_id.clone(), "wl_compositor::create_region", e);
                return;
            }
        };
        for rect in regions {
            region.add(rect.x0, rect.y0, rect.x1 - rect.x0, rect.y1 - rect.y0);
        }
        surface.set_input_region(Some(region.wl_region()));
        // `set_input_region` copies the contents, so dropping the region here is sufficient.
    }

    /// Hand the compositor the region behind this surface it should blur (§ 5.1 `blur`,
    /// ADR-0195). The rects come from `layout::blur_regions`, which is where the policy lives; this
    /// is only the push.
    ///
    /// Lazily created and never created at all for the common surface, because most surfaces never
    /// set `blur` and an `ext_background_effect_surface_v1` per surface would be an object and a
    /// destroy for nothing. The object names a `wl_surface`, so it is dropped with one: `unmap`
    /// takes the layer object down and the next `create` makes a fresh pair.
    ///
    /// A compositor with no manager, or one whose `blur` capability is absent or withdrawn, gets
    /// nothing pushed and the config sees no error -- an unavailable compositor feature is not a
    /// config mistake.
    fn apply_blur_region(&mut self, index: usize, regions: Vec<crate::text::snap::PhysicalRect>) {
        let Some((manager, supported)) = self.background_effect.as_ref() else {
            return;
        };
        if !supported {
            return;
        }
        let Some(surface) = self.surfaces[index].role.wl_surface().cloned() else {
            return;
        };
        let manager = manager.clone();
        let qh = self.queue_handle.clone();
        // Identity first, and before the region compare below. An object whose `wl_surface` is
        // gone is inert, and a tooltip reopens at the same size constantly -- so the compare would
        // match, return, and leave the reopened surface holding a dead object and no blur. The
        // crash this replaces was the same fact read from the other side.
        let surface_id = surface.id();
        if let Some((stale, named)) = self.surfaces[index].blur_effect.take() {
            if named == surface_id {
                self.surfaces[index].blur_effect = Some((stale, named));
            } else {
                // The `wl_surface` is already gone, so `destroy` is the only request still legal
                // on it; the region it held died with the surface, so the compare below has
                // nothing to match against.
                stale.destroy();
                self.surfaces[index].last_blur_region.clear();
            }
        }
        if regions == self.surfaces[index].last_blur_region {
            return;
        }
        if self.surfaces[index].blur_effect.is_none() {
            // Nothing to ask for and nothing asked for before: do not create the object at all.
            if regions.is_empty() {
                return;
            }
            self.surfaces[index].blur_effect = Some((manager.get_background_effect(&surface, &qh, ()), surface_id));
        }
        let region = match Region::new(&self.compositor_state) {
            Ok(region) => region,
            Err(e) => {
                log_bind_failure(&self.surfaces[index].surface_id.clone(), "wl_compositor::create_region", e);
                return;
            }
        };
        for rect in &regions {
            region.add(rect.x0, rect.y0, rect.x1 - rect.x0, rect.y1 - rect.y0);
        }
        if let Some((effect, _)) = self.surfaces[index].blur_effect.as_ref() {
            // A null region would remove the effect; an empty one keeps the object and blurs
            // nothing, which is what a surface whose glass is currently hidden wants.
            effect.set_blur_region(Some(region.wl_region()));
            // The region is double-buffered and lands on the next `wl_surface.commit`, and
            // `paint_surface` skips both the draw and the commit when the display list is
            // unchanged. A `blur` that flips with nothing else moving produces exactly that list,
            // so without this the region would sit pending until some unrelated repaint. `stale`
            // is the existing word for "the committed state is behind what this surface should be
            // showing", and it costs one repaint of a surface whose glass just changed.
            self.surfaces[index].stale = true;
        }
        self.surfaces[index].last_blur_region = regions;
    }

    /// Apply § 5.1 `visible` as create/destroy for every role (ADR-0049 decision 1, ADR-0088).
    /// Freeze it for PBA Candidates: `ReadySignal` and `ActivateDraw` must announce and draw the
    /// same set, or § 14.2 yields `evidence_timeout` or `PbaFailure::UnexpectedEvidence`. Deferred
    /// changes apply on the first post-promotion re-resolve.
    fn apply_visibility(&mut self, index: usize, visible: bool) {
        if self.is_pba_candidate {
            return;
        }
        match &self.surfaces[index].role {
            TrackedRole::Panel { .. } => match (self.surfaces[index].map_state, visible) {
                (MapState::Unmapped, true) => {
                    // `QueueHandle` is a cheap refcounted handle; clone it across `&mut self`.
                    let qh = self.queue_handle.clone();
                    self.show_panel(&qh, index);
                }
                (MapState::AwaitingConfigure | MapState::Mapped, false) => self.unmap(index),
                _ => {}
            },
            TrackedRole::Window { .. } => match (self.surfaces[index].map_state, visible) {
                (MapState::Unmapped, true) => {
                    // Clone the cheap refcounted handle across `&mut self`.
                    let qh = self.queue_handle.clone();
                    self.show_window(&qh, index);
                }
                (MapState::AwaitingConfigure | MapState::Mapped, false) => self.hide_window(index),
                _ => {}
            },
            // Popup visibility also reads ADR-0051's latch: dismissal leaves it `Unmapped` while
            // `visible` remains true.
            TrackedRole::Popup { .. } => self.apply_popup_visibility(index, visible),
            // `lock_spec` rejects `visible`; the compositor owns lock-surface lifetime from
            // `locked` through `unlock_and_destroy` (ADR-0042, ADR-0052 decision 2).
            TrackedRole::Lock { .. } => {}
        }
    }

    /// The protocol's null-buffer unmap was one commit with no destroyed protocol objects, the
    /// point of ADR-0038 decision 2: toggling a launcher costs this instead of a process spawn.
    /// Hiding a panel destroys its layer object (ADR-0088): niri never revives it despite the
    /// correct wire sequence, so show rebuilds it; this matches windows/popups and mirrored Qt
    /// `PanelWindow`. Drop child popups, EGL/`wl_egl_window`, then the role and `wl_surface`.
    fn unmap(&mut self, index: usize) {
        if !matches!(self.surfaces[index].role, TrackedRole::Panel { .. }) {
            return;
        }
        self.drop_child_popups(index);
        self.release_bound(index);
        if let TrackedRole::Panel { layer, .. } = &mut self.surfaces[index].role {
            drop(layer.take());
        }
        // The effect object names the `wl_surface` that just went away; a stale one would be inert
        // at best. The next map creates a fresh pair, and the cleared region forces the push.
        if let Some((effect, _)) = self.surfaces[index].blur_effect.take() {
            effect.destroy();
        }
        self.surfaces[index].last_blur_region.clear();
        self.surfaces[index].map_state = MapState::Unmapped;
        self.surfaces[index].null_buffered = false;
        // The old object and its pixels are gone; force the next object to paint.
        self.surfaces[index].last_painted = None;
        // No `leave` follows client destruction; clear stale focus or a closed polkit prompt would
        // scrub and re-arm once per frame.
        if self.keyboard_focus.as_deref() == Some(self.surfaces[index].surface_id.as_str()) {
            self.keyboard_focus = None;
            self.focus_secure_submit(None);
        }
        eprintln!("[oblisk-renderer] {} destroyed: visible = false", self.surfaces[index].surface_id);
    }

    /// Lazily builds the process-wide EGL state on the first drawable surface (ADR-0071). Failure
    /// is fatal: a PBA Candidate signals ready before proving it can build a context, so a broken
    /// EGL between generations takes the shell down rather than rolling back (ADR-0071 decision 3).
    fn ensure_egl(&mut self, surface_id: &str) -> bool {
        if self.egl.is_some() {
            return true;
        }
        // SAFETY: the pointer comes from this `App`'s `Connection`, which outlives its EGL objects.
        match egl::init(self.conn.backend().display_ptr() as *mut c_void) {
            Ok(state) => {
                self.egl = Some(state);
                true
            }
            Err(err) => {
                log_bind_failure(surface_id, "egl::init", err);
                self.exit = true;
                false
            }
        }
    }

    /// Creates the surface's `wl_egl_window`/EGL surface against shared context, initializing EGL
    /// and `glow` on their first use. Failure is fatal (`self.exit`).
    fn ensure_bound(&mut self, index: usize) -> bool {
        if self.surfaces[index].bound.is_some() {
            return true;
        }
        let surface_id = self.surfaces[index].surface_id.clone();
        let (width, height) = self.surfaces[index].configured_size;
        let width = width.max(1) as i32;
        let height = height.max(1) as i32;
        let Some(surface_object_id) = self.surfaces[index].role.wl_surface().map(Proxy::id) else {
            // The window was hidden between requesting and performing the bind; there is nothing
            // to bind, and the map-state guard already stopped painting.
            return false;
        };

        // Only the first drawable surface pays for Mesa (ADR-0071); hidden windows took the cheap
        // bails above.
        if !self.ensure_egl(&surface_id) {
            return false;
        }
        let egl = self.egl.as_ref().expect("ensure_egl returned true, so the state is built");

        let native_window = match WlEglSurface::new(surface_object_id, width, height) {
            Ok(w) => w,
            Err(e) => {
                log_bind_failure(&surface_id, "WlEglSurface::new", e);
                self.exit = true;
                return false;
            }
        };

        // SAFETY: `native_window.ptr()` is the live `wl_egl_window*` just built for this EGL
        // display/config, exactly what `eglCreateWindowSurface` requires.
        let egl_surface = unsafe {
            egl.instance.create_window_surface(egl.display, egl.config, native_window.ptr() as *mut c_void, None)
        };
        let egl_surface = match egl_surface {
            Ok(s) => s,
            Err(e) => {
                log_bind_failure(&surface_id, "eglCreateWindowSurface", e);
                self.exit = true;
                return false;
            }
        };

        if let Err(e) = egl.instance.make_current(egl.display, Some(egl_surface), Some(egl_surface), Some(egl.context))
        {
            log_bind_failure(&surface_id, "eglMakeCurrent", e);
            self.exit = true;
            return false;
        }

        // Request non-blocking swaps on each newly current surface. EGL defaults to 1, which would
        // stall this same thread's Wayland dispatch, Supervisor reads, and input. Today the loop
        // paints only on dirty pushes, so pacing is unnecessary. Measured cost was 0.24-0.89 ms
        // per swap across five swaps in 25 s; it matters when ADR-0130 adds per-frame animation.
        // Failure is non-fatal and leaves EGL's current blocking default.
        if let Err(e) = egl.instance.swap_interval(egl.display, 0) {
            eprintln!(
                "[oblisk-renderer] {surface_id}: eglSwapInterval(0) failed ({e}); swaps on this surface keep EGL's blocking default"
            );
        }

        // SAFETY: `eglMakeCurrent` directly above binds the loader's context on this single
        // dispatch thread, with no intervening context switch.
        self.gl.get_or_insert_with(|| unsafe {
            glow::Context::from_loader_function(|s| {
                egl.instance.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void)
            })
        });

        eprintln!("[oblisk-renderer] {surface_id} up: {width}x{height}, EGL context current");
        self.surfaces[index].bound = Some(BoundSurface { egl_surface, native_window });
        // A new EGL surface has empty buffers, so the next paint is unconditional.
        self.surfaces[index].last_painted = None;
        true
    }

    /// Paint a bound surface's whole display list and swap. One `TextPainter` and EGL context serve
    /// all surfaces; GL objects stay valid across framebuffers, while viewport size is per surface.
    /// One canvas per surface is the fallback, not a redesign, if a live run shows this assumption
    /// wrong.
    /// An absent tree still clears/swaps, or the compositor keeps the last frame.
    /// ponytail: paint scale is hardcoded `1.0`, so HiDPI outputs are upscaled. Upgrade:
    /// `set_buffer_scale` and matching `WlEglSurface::resize` together.
    fn paint_surface(&mut self, index: usize) {
        // Unmapped or pre-configure surfaces cannot attach a buffer; `swap_buffers` is both attach
        // and commit, so this prevents `visible = false` from remapping (ADR-0038 decision 2).
        if self.surfaces[index].map_state != MapState::Mapped {
            return;
        }
        let Some(egl_surface) = self.surfaces[index].bound.as_ref().map(|b| b.egl_surface) else {
            return;
        };
        let surface_id = self.surfaces[index].surface_id.clone();
        let (width, height) = self.surfaces[index].configured_size;
        let (width, height) = (width.max(1), height.max(1));

        // Build before GL work so an unchanged surface costs one tree walk, not make-current,
        // clear, draw calls, and swap. This stops a 1920x1200 wallpaper redrawing every second
        // because the clock's seconds digit advanced (ADR-0044 decision 2's global dirty flag).
        // An absent tree becomes an empty list and still reaches clear/swap to erase old contents.
        let tree = self.client.scene().surface(&surface_id);
        let mut animating = tree.is_some_and(layout::ResolvedNode::animating);
        // End the immutable field-focus borrow before mutably borrowing the painter; `Draw::Text`
        // owns its string.
        let list = {
            let focus = self.field_focus_for(&surface_id);
            tree.as_ref().map(|tree| layout::paint::build(tree, 1.0, focus.as_ref())).unwrap_or_default()
        };
        let unchanged = !self.surfaces[index].stale
            && self.surfaces[index]
                .last_painted
                .as_ref()
                .is_some_and(|(painted_size, painted)| *painted_size == (width, height) && *painted == list);
        if unchanged {
            // A mid-tween surface still has to commit: a frame callback is only answered after
            // one, and a tween whose tick moved nothing visible would otherwise never get its
            // next (ADR-0145). What it does not have to do is draw the same pixels again. A
            // commit with no new buffer re-commits the state the surface already has, which is
            // what makes the frame request below effective -- so a hold, a lead-in `delay`, or
            // a step easing sitting on one value costs a commit instead of make-current, clear,
            // every draw call, and a swap.
            if animating && let Some(surface) = self.surfaces[index].role.wl_surface() {
                surface.frame(&self.queue_handle, FrameCallbackData(surface.clone()));
                surface.commit();
            }
            return;
        }

        // Another surface may have changed the current framebuffer, so re-establish it; bound
        // surfaces always have shared EGL state.
        let Some(egl) = self.egl.as_ref() else {
            return;
        };
        if let Err(e) = egl.instance.make_current(egl.display, Some(egl_surface), Some(egl_surface), Some(egl.context))
        {
            log_bind_failure(&surface_id, "eglMakeCurrent", e);
            self.exit = true;
            return;
        }

        // SAFETY: the `eglMakeCurrent` above is the only live context on this thread and matches
        // `gl`'s loader.
        if let Some(gl) = self.gl.as_ref() {
            // SAFETY: the context was made current above and has not switched since.
            unsafe {
                use glow::HasContext;
                gl.clear_color(0.0, 0.0, 0.0, 0.0);
                gl.clear(glow::COLOR_BUFFER_BIT);
            }
        }

        if self.text_painter.is_none() {
            let font_chain = self.shaping.font_chain_data();
            let generation = self.shaping.font_generation();
            match TextPainter::new(
                |s| egl.instance.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void),
                width,
                height,
                &font_chain,
                generation,
            ) {
                Ok(painter) => self.text_painter = Some(painter),
                Err(e) => {
                    log_bind_failure(&surface_id, "FemtoVG init", e);
                    self.exit = true;
                    return;
                }
            }
        }

        if let Some(painter) = self.text_painter.as_mut() {
            painter.resize(width, height);
            // A family a node named for the first time was resolved on the shaping worker while
            // this list was being measured; femtovg has to be given those faces before the list
            // that names them is drawn (ADR-0144). One atomic load on the frames where nothing
            // changed, which is all of them after startup.
            let generation = self.shaping.font_generation();
            if generation != painter.font_generation() {
                painter.sync(&self.shaping.font_chain_data(), generation);
            }
            // The context is current from `make_current` above, so a config shader can take a
            // cross this frame; without one every cross falls back to the dissolve (ADR-0184).
            let shaders = self.gl.as_ref().map(|gl| layout::paint::Shaders { gl, stage: &mut self.shader_stage });
            let drawn = layout::paint::execute(
                painter,
                &mut self.image_cache,
                &list,
                1.0,
                (width as f32, height as f32),
                shaders,
            );
            // After the draws that answered it, before the swap: the tree this reads is the one
            // the next build walks, so a `retain` cover ends and a `transition` starts on the
            // frame paint proved the texture exists (ADR-0183).
            if !drawn.is_empty() {
                self.client.note_drawn_images(&surface_id, &drawn, std::time::Instant::now());
                // Re-read: a dissolve that started in this very paint was not running when
                // `animating` was taken above, and the frame callback below is the only thing that
                // will ever advance it. Missing this is the tween-gate mistake again -- motion
                // begun where nothing was looking for it (ADR-0183).
                animating |= self.client.scene().surface(&surface_id).is_some_and(layout::ResolvedNode::animating);
            }
        }
        // Straight after this surface's `execute` and before any other paint runs, which is what
        // scopes a cache-wide flag to the surface that earned it (ADR-0185).
        let deferred = self.image_cache.take_deferred();

        // Before the swap, which is the commit it has to precede. Requested only while a tween is
        // running, so an idle shell arms nothing and the loop's timeout-free poll stays that way
        // (ADR-0124, ADR-0130 decision 3).
        if animating && let Some(surface) = self.surfaces[index].role.wl_surface() {
            surface.frame(&self.queue_handle, FrameCallbackData(surface.clone()));
        }
        if let Err(e) = egl.instance.swap_buffers(egl.display, egl_surface) {
            log_bind_failure(&surface_id, "eglSwapBuffers", e);
            self.exit = true;
            return;
        }
        // Record only after swap; otherwise an unpresented frame could make the next identical list
        // skip the paint the screen never received.
        self.surfaces[index].last_painted = Some(((width, height), list));
        // A request turned away for pool capacity recorded no slot, so asking again is the whole
        // retry -- and only a repaint asks. Staying `stale` is what stops the next turn skipping
        // this surface on an unchanged list, and `repaint_mapped_surfaces_where` is what stops a
        // narrowed repaint passing it over (ADR-0185).
        self.surfaces[index].stale = deferred;
        self.surfaces_drawn += 1;
        // Images absent from every current list are idle (ADR-0123); queue eviction for the next
        // paint.
        let surfaces = &self.surfaces;
        self.image_cache.trim(|| {
            let mut pinned = Vec::new();
            for surface in surfaces {
                if let Some((_, list)) = &surface.last_painted {
                    list.drawn_images(&mut pinned);
                }
            }
            pinned
        });
    }

    /// Repaint every mapped surface after a changed scene because ADR-0044 decision 2 has one
    /// global dirty flag. `paint_surface` skips unchanged lists, so protocol-only updates need
    /// their own commit in [`App::apply_spec_change`]. A decoded image invalidates any list that
    /// draws its file (ADR-0122), making the next repaint upload the new texture.
    pub(super) fn forget_painted_lists_drawing(&mut self, files: &[std::path::PathBuf]) {
        for surface in &mut self.surfaces {
            if surface.last_painted.as_ref().is_some_and(|(_, list)| list.draws_any_of(files)) {
                // Marked, not cleared: this surface still shows those images until it repaints, so
                // its list has to keep pinning them (ADR-0182).
                surface.stale = true;
            }
        }
    }

    pub(super) fn repaint_mapped_surfaces(&mut self) {
        self.repaint_mapped_surfaces_where(|_| true);
    }

    /// The surfaces a tween tick just advanced, by the instance ids `Scene::tick` returned, plus
    /// any surface already marked `stale`.
    ///
    /// A tick changes only the trees it names, so the others would each build a display list and
    /// have it rejected as equal to the one they last painted. That build is not free: a text draw
    /// copies its content and style runs, an image or icon its name. This is the same repaint,
    /// asked of the surfaces that can actually differ.
    ///
    /// `stale` is exactly the flag that says a surface differs for a reason its tree cannot show
    /// -- a decode turned away for capacity, whose retry *is* the next paint. Narrowing it out is
    /// what made arming a frame callback insufficient (ADR-0185).
    pub(super) fn repaint_surfaces_with_instance_ids(&mut self, instance_ids: &[String]) {
        let stale: Vec<String> =
            self.surfaces.iter().filter(|surface| surface.stale).map(|surface| surface.surface_id.clone()).collect();
        let targets = narrowed_repaint_targets(instance_ids, &stale);
        self.repaint_mapped_surfaces_where(|surface_id| targets.iter().any(|id| id == surface_id));
    }

    /// Whether any mapped surface owes a repaint its tree cannot ask for. The main loop's repaint
    /// selection needs this: with nothing ticked, nothing typed and nothing landed, it would
    /// otherwise reach no repaint at all and a deferred decode would never be asked for again.
    pub(super) fn has_stale_surfaces(&self) -> bool {
        self.surfaces.iter().any(|surface| surface.stale && surface.map_state == MapState::Mapped)
    }

    fn repaint_mapped_surfaces_where(&mut self, wanted: impl Fn(&str) -> bool) {
        for index in 0..self.surfaces.len() {
            if self.surfaces[index].map_state != MapState::Mapped {
                continue;
            }
            if !wanted(&self.surfaces[index].surface_id) {
                continue;
            }
            if self.surfaces[index].bound.is_none() {
                // A panel shown after starting hidden was configured without EGL; bind here because
                // no further configure is coming. Candidates wait for `ActivateDraw` (§ 14.2).
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

    /// Once every candidate surface stages, queue one § 14.2 `ReadySignal`. The gate covers every
    /// tracked surface; the payload covers only presenting surfaces. Called after each candidate
    /// configure because any one may complete the set.
    pub(super) fn maybe_send_ready_signal(&mut self) {
        let staged =
            candidate_has_staged(self.surfaces.iter().map(|s| (s.null_buffered, s.role.wl_surface().is_some())));
        if self.ready_signal_sent || !staged {
            return;
        }
        self.ready_signal_sent = true;
        let surfaces = presenting_surface_ids(self.surfaces.iter().map(|s| (s.surface_id.as_str(), s.map_state)));
        if let Err(e) = self.outbound_tx.send(RendererFrame::ReadySignal(ReadySignal { surfaces })) {
            eprintln!("[oblisk-renderer] failed to queue ReadySignal for the socket thread: {e}");
        }
    }

    /// Draw the § 14.2 first frame for `ActivateDraw`, requesting presentation feedback and tagging
    /// it with `nonce`. Draw exactly the `ReadySignal` presenting set; Candidates freeze map state,
    /// so any mismatch would hang or abort the handshake.
    pub(super) fn activate_draw(&mut self, nonce: u64) {
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
        // Promotion must return later configure events to the ordinary resize path; the candidate
        // branch stops after `null_buffered` and would otherwise disable resize permanently.
        self.is_pba_candidate = false;
    }

    /// One `ActivateDraw`: bind like the ordinary path, request presentation feedback before
    /// `swap_buffers`, then paint. Indexing avoids holding a surface borrow across EGL and painter
    /// calls.
    fn activate_draw_one(&mut self, index: usize, nonce: u64) {
        if !self.ensure_bound(index) {
            return;
        }

        // Request feedback before `swap_buffers` so it associates with that commit; verify ordering
        // with `WAYLAND_DEBUG=1` if needed (§ 14.2).
        if let Some(surface) = self.surfaces[index].role.wl_surface().cloned()
            && let Err(e) = self.presentation_time.feedback(&surface, &self.queue_handle)
        {
            // `evidence_timeout` catches a surface that never presents (ADR-0025); keep the
            // candidate alive rather than adding a second failure path.
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

    /// Resolve a raw `wl_surface` from feedback, pointer, or keyboard events. `None` is routine:
    /// per-seat/per-commit objects can name a surface destroyed by output change or `visible`
    /// flip (ADR-0049 decision 1).
    pub(super) fn index_of_surface(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.surfaces.iter().position(|s| s.role.wl_surface() == Some(surface))
    }

    /// [`App::index_of_surface`] as a surface id for `presented`/`discarded`.
    pub(super) fn surface_id_for(&self, surface: &wl_surface::WlSurface) -> Option<&str> {
        self.index_of_surface(surface).map(|index| self.surfaces[index].surface_id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wayland::input::rect_table;

    /// ADR-0185. A decode refused for pool capacity records no slot, so asking again is the whole
    /// retry -- and only a paint asks. The surface is marked `stale` and arms a frame callback,
    /// but the callback advances no tween, so `Scene::tick` names nothing and a repaint narrowed
    /// to the ticked ids covers no surface at all. That is the stall, and this is the narrowing
    /// that has to stop causing it.
    #[test]
    fn a_narrowed_repaint_still_covers_a_stale_surface_no_tick_named() {
        let id = |s: &str| s.to_string();

        // The bug: nothing ticked, one surface stale. Narrowing by tick alone repaints nothing.
        assert_eq!(narrowed_repaint_targets(&[], &[id("wallpaper@eDP-1")]), vec![id("wallpaper@eDP-1")]);

        // A tick elsewhere must not narrow the stale surface out, which is the case that actually
        // happens: a clock ticks every second while a wallpaper waits on a refused decode.
        assert_eq!(
            narrowed_repaint_targets(&[id("bar@eDP-1")], &[id("wallpaper@eDP-1")]),
            vec![id("bar@eDP-1"), id("wallpaper@eDP-1")]
        );

        // Both at once is one repaint, not two.
        assert_eq!(narrowed_repaint_targets(&[id("bar@eDP-1")], &[id("bar@eDP-1")]), vec![id("bar@eDP-1")]);

        // Nothing owed, nothing painted: the idle turn stays idle (ADR-0124).
        assert!(narrowed_repaint_targets(&[], &[]).is_empty());
    }

    /// The narrowing above is only ever right for a turn that did not re-resolve. A pass can
    /// change any tree, and it does not tick, so its `ticked` list is empty: narrowing by it
    /// repaints the stale surface and drops the surface the pass actually changed.
    #[test]
    fn a_pass_repaints_everything_even_when_something_else_is_stale() {
        let turn = |passed, ticked, stale, typed, landed| {
            repaint_for_turn(TurnChanges { passed, ticked, stale, typed, landed })
        };

        // The bug: a pass changed a panel while a wallpaper waited on a refused decode. The
        // narrowed repaint covers the wallpaper and the panel never reaches the screen.
        assert_eq!(turn(true, false, true, false, false), Repaint::Everything);
        assert_eq!(turn(true, false, false, false, false), Repaint::Everything);

        // A tween frame is what narrowing exists for, stale surface or not (ADR-0178, ADR-0185).
        assert_eq!(turn(false, true, false, false, false), Repaint::Narrowed);
        assert_eq!(turn(false, false, true, false, false), Repaint::Narrowed);

        // A caret and a landed decode are both scene-wide, and outrank a tick on the same turn.
        assert_eq!(turn(false, true, false, true, false), Repaint::Everything);
        assert_eq!(turn(false, true, false, false, true), Repaint::Everything);

        // An idle turn paints nothing and stays timeout-free (ADR-0124).
        assert_eq!(turn(false, false, false, false, false), Repaint::Nothing);
    }

    /// The protocol-state half of the same turn. Narrowing it to the ticked instances is the
    /// point, and ADR-0051's latch is what that narrowing must not take with it: a click whose
    /// handler writes no signal re-resolves nothing, so a popup the compositor dismissed while
    /// `visible` stayed true would be reopened only when some unrelated surface was mid-tween.
    #[test]
    fn a_click_gets_the_popup_latch_looked_at_whatever_else_the_turn_did() {
        let turn = surface_state_for_turn;

        // A pass visits every popup with this turn's serial in hand, so the latch is not owed
        // twice.
        assert_eq!(turn(true, false, true), SurfaceStateWork { scope: StateScope::Everything, popup_latch: false });
        assert_eq!(turn(true, false, false), SurfaceStateWork { scope: StateScope::Everything, popup_latch: false });

        // A tween frame re-derives state for what it advanced. The click on top of it is still
        // owed the latch, because the surfaces the tick named are not the popup's.
        assert_eq!(turn(false, true, false), SurfaceStateWork { scope: StateScope::Ticked, popup_latch: false });
        assert_eq!(turn(false, true, true), SurfaceStateWork { scope: StateScope::Ticked, popup_latch: true });

        // The case that was never handled at all: a click, no signal written, nothing animating.
        assert_eq!(turn(false, false, true), SurfaceStateWork { scope: StateScope::Nothing, popup_latch: true });

        // An idle turn pushes nothing (ADR-0124).
        assert_eq!(turn(false, false, false), SurfaceStateWork { scope: StateScope::Nothing, popup_latch: false });
    }

    #[test]
    fn a_declared_but_unlocked_lock_instance_neither_hangs_nor_joins_the_pba_ready_set() {
        // A `lock` instance owns zero Wayland objects until `locked` arrives, so it reaches both
        // PBA gates as `(null_buffered: false, exists: false)` and `MapState::Unmapped` -- complete
        // by construction for the staging gate, absent from the announced set. Getting either wrong
        // is a `ready_timeout` hang or an `UnexpectedEvidence` abort.
        assert!(candidate_has_staged([(false, false)].into_iter()));
        assert!(presenting_surface_ids([("screen-lock@eDP-1", MapState::Unmapped)].into_iter()).is_empty());
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
        // A plain `all(null_buffered)` gate is correct only while every tracked surface is a panel:
        // a panel always gets a configure, since it is committed at startup even when hidden.
        assert!(candidate_has_staged([(true, true), (true, true)].into_iter()));
        assert!(!candidate_has_staged([(true, true), (false, true)].into_iter()));
    }

    #[test]
    fn a_window_declared_invisible_has_nothing_to_stage_and_must_not_hold_the_ready_signal() {
        // ADR-0049 decision 1 creates no `xdg_toplevel` for it, so no configure is coming and
        // `null_buffered` would stay false forever -- a `ready_timeout` hang under a plain gate, on
        // any config declaring a hidden window (the dev config does).
        assert!(candidate_has_staged(
            [("bar", true, true), ("settings", false, false)].into_iter().map(|(_, n, e)| (n, e))
        ));
        assert!(
            candidate_has_staged([(false, false)].into_iter()),
            "a surface with no object at all is complete by construction"
        );
    }

    #[test]
    fn a_generation_whose_every_panel_starts_hidden_announces_nothing_at_all() {
        // Legal, not degenerate: `drive_handshake`'s `while collected.len() < expected.len()` loop
        // exits immediately on an empty expected set, so this Candidate completes its handshake.
        let surfaces = [("launcher@eDP-1", MapState::Unmapped)];
        assert!(presenting_surface_ids(surfaces.into_iter()).is_empty());
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
            exclusive: node::Exclusive::Reserve,
            margin: node::EdgeInsets::default(),
            width: SizeMode::Fill,
            height: SizeMode::Pixels(32.0),
        }
    }

    #[test]
    fn an_omitted_popup_axis_takes_the_measured_box_and_a_declared_one_ignores_it() {
        // The whole point of the `Content` axis: the number the positioner is given comes from
        // what the tree measured, not from a guess in the config. A declared axis is untouched by
        // the measurement, so one axis can be fixed and the other fitted.
        let measured = LogicalRect { x: 0.0, y: 0.0, width: 252.48, height: 36.0 };
        let mut spec = popup_spec_fixture();
        assert_eq!(popup_requested_size(&spec, measured), (200.0, 120.0), "declared numbers win outright");

        spec.width = node::SizeMode::Content;
        assert_eq!(
            popup_requested_size(&spec, measured),
            (253.0, 120.0),
            "252.48 rounds *up*: 252 would cut the half pixel the card asked for, which is the clipping this ends"
        );

        spec.height = node::SizeMode::Content;
        assert_eq!(popup_requested_size(&spec, measured), (253.0, 36.0));
    }

    #[test]
    fn a_content_axis_measuring_nothing_stays_zero_for_show_popup_to_decline_on() {
        // A hidden popup's tree is frozen at 0x0 (ADR-0124). Reporting that honestly is what lets
        // `show_popup` wait a pass instead of opening at a size nothing measured.
        let mut spec = popup_spec_fixture();
        spec.width = node::SizeMode::Content;
        spec.height = node::SizeMode::Content;
        let nothing = LogicalRect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 };
        assert_eq!(popup_requested_size(&spec, nothing), (0.0, 0.0));
    }

    #[test]
    fn a_placement_moves_when_the_measurement_does_and_holds_when_nothing_does() {
        // The comparison that decides whether an open popup is repositioned. It must be false for
        // an unchanged pass: `apply_popup_visibility` runs for every surface on every capability
        // push, so a placement that compared unequal to itself would send a `reposition` several
        // times a second for the life of every open popup.
        let card = LogicalRect { x: 0.0, y: 0.0, width: 169.0, height: 36.0 };
        let mut spec = popup_spec_fixture();
        spec.width = node::SizeMode::Content;
        spec.height = node::SizeMode::Content;

        let opened = Placement::of(&spec, card);
        assert_eq!(opened, Placement::of(&spec, card), "an unchanged pass is not a reposition");

        let grown = LogicalRect { width: 253.0, ..card };
        assert_ne!(opened, Placement::of(&spec, grown), "the words grew, so the surface must follow");

        // Placement, not just size: an indicator that moves takes its tooltip with it.
        let mut slid = spec.clone();
        slid.anchor_rect = LogicalRect { x: 400.0, ..spec.anchor_rect };
        assert_ne!(opened, Placement::of(&slid, card));

        // And a declared axis is deaf to the measurement, so a fixed popup never repositions for it.
        let fixed = popup_spec_fixture();
        assert_eq!(Placement::of(&fixed, card), Placement::of(&fixed, grown));
    }

    #[test]
    fn an_unmeasured_placement_is_not_something_to_reposition_to() {
        // A popup open on real content whose tree momentarily resolves to nothing must keep what it
        // has: `set_size` raises `invalid_input` on a zero, and a popup that vanished mid-hover is
        // worse than one a frame stale.
        let mut spec = popup_spec_fixture();
        spec.width = node::SizeMode::Content;
        spec.height = node::SizeMode::Content;
        let nothing = LogicalRect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 };
        assert!(!Placement::of(&spec, nothing).is_measured());
        assert!(Placement::of(&spec, LogicalRect { x: 0.0, y: 0.0, width: 169.0, height: 36.0 }).is_measured());

        // One axis measured is not enough; `set_size` takes both.
        let half = LogicalRect { x: 0.0, y: 0.0, width: 169.0, height: 0.0 };
        assert!(!Placement::of(&spec, half).is_measured());
    }

    fn popup_spec_fixture() -> PopupSpec {
        PopupSpec {
            id: "menu".to_string(),
            parent: "bar".to_string(),
            anchor_rect: LogicalRect { x: 997.0, y: 4.0, width: 86.0, height: 24.0 },
            width: SizeMode::Pixels(200.0),
            height: SizeMode::Pixels(120.0),
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
        // at once. A panel comes up painting nothing (the "keep the shell up" fallback); a window
        // or popup created here would be a Wayland object the config never asked for.
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
            assert!(
                !starting_visible(Some(false), &roster),
                "a declared-closed surface stays closed whatever its role"
            );
        }
    }

    #[test]
    fn a_new_surfaces_spec_comes_from_the_resolved_tree_not_the_evaluations_roster() {
        // ADR-0049's second amendment: the roster's `anchor_rect` is `DEFERRED_POPUP_EXTENT`'s
        // 1x1 placeholder whenever the config signal-bound it, and a popup shown from that keeps it
        // for its whole life since the positioner is consumed by `get_popup`.
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
        let SurfaceSpec::Popup(spec) = spec.unwrap() else {
            panic!("the role comes from the roster, not from the properties")
        };
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
}
