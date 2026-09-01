//! Core surface bookkeeping shared by every role: the `TrackedRole`/`TrackedSurface`/`MapState`
//! data model, EGL binding, and the create/destroy/paint/(un)map lifecycle every configure handler
//! calls into. Role-specific behavior lives in `layer`, `xdg_shell` and `lock`; this module is
//! what they all share.

use super::*;
use crate::wayland::layer::anchor_for;
use crate::wayland::layer::keyboard_interactivity_for;
use crate::wayland::layer::layer_extent_for;

/// A window surface bound to the shared EGL context after its first configure.
/// Field order matters: wayland-egl requires `WlEglSurface` to outlive the EGL surface built
/// from it, and Rust drops fields top to bottom, so `egl_surface` is declared first.
/// `khronos_egl::Surface` has no `Drop`, so dropping this struct never calls `eglDestroySurface` --
/// only [`App::destroy_surface_by_id`] does that.
pub(super) struct BoundSurface {
    egl_surface: EglSurface,
    #[allow(dead_code)]
    native_window: WlEglSurface,
}
/// Logs an EGL/Wayland bind-time failure for `bind_and_clear`'s fallible steps. `surface_id`
/// is `"{id}@{output}"` (ADR-0038), naming both the config's surface and the monitor it
/// failed on.
pub(super) fn log_bind_failure(surface_id: &str, stage: &str, err: impl std::fmt::Display) {
    eprintln!("[oblisk-renderer] {surface_id}: {stage} failed: {err}");
}
/// § 6.1's `visible`, as the compositor currently sees it (ADR-0038 decision 2: within a
/// live generation, `visible` maps and unmaps a surface without destroying it).
///
/// Three states, not two: a re-map must commit with no buffer, then wait for a configure before
/// attaching one, and a surface's first map obeys the same rule. Both share `AwaitingConfigure`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MapState {
    /// `visible` is false: no buffer was ever attached, or a null buffer unmapped one. Nothing
    /// may be painted or committed here, since an empty commit is the re-map (see
    /// [`App::unmap`]).
    Unmapped,
    /// The map (or re-map) commit is out and the compositor has not configured the surface yet.
    AwaitingConfigure,
    /// Configured: [`App::paint_surface`] may attach a buffer, and its `swap_buffers` is the
    /// commit every other staged request rides on.
    Mapped,
}
impl MapState {
    /// Whether this surface will put a frame on screen: the one predicate PBA's expected set
    /// and its drawn set must agree on. See [`presenting_surface_ids`].
    fn presents(self) -> bool {
        self != MapState::Unmapped
    }
}
/// The protocol object a tracked surface's `wl_surface` has been given a role by, plus the spec
/// its state was last set from (§ 6, ADR-0040 decision 1). One enum, not a `Vec` per role: EGL
/// binding, paint, input routing and PBA staging are identical across roles and all index one
/// `App::surfaces`.
///
/// Variants differ in how long the Wayland object lives (ADR-0049 decision 1): a `panel`'s
/// lives for the generation, a `window`'s or `popup`'s only while shown, hence `Option`.
pub(super) enum TrackedRole {
    Panel {
        layer: LayerSurface,
        /// The diff baseline `layer::spec_update` compares a fresh resolve against, so only fields
        /// that actually moved are pushed (ADR-0038 decision 2). Also the standing anchor/exclusive
        /// answer [`App::apply_exclusive_zone`] needs once `configure` reports a size.
        spec: PanelSpec,
        /// This surface's output's logical size, the basis `SizeMode::Percent` resolves against.
        /// Kept per surface rather than read from `SurfaceInstance::available`: `set_instance_size`
        /// overwrites `available` with the compositor-granted size after the first configure, so
        /// resolving a percent against it later would shrink the surface on every push. Panel-only:
        /// § 6.2 gives a `window` no `width`/`height` to resolve.
        output_size: layout::LogicalSize,
    },
    Window {
        /// `None` when `visible` is false: the `xdg_toplevel`, `xdg_surface` and `wl_surface`
        /// do not exist at all (ADR-0049 decision 1). A declared-but-never-shown window
        /// costs one retained node and zero Wayland objects.
        window: Option<Window>,
        /// This toplevel's state's diff baseline for `xdg_shell::window_update`. Maintained even
        /// while `window` is `None`, so [`App::show_window`] builds from the last re-resolve's
        /// spec.
        spec: WindowSpec,
    },
    Popup {
        /// `None` when not shown. `xdg_positioner` is consumed by `get_popup`, so a popup
        /// anchored once cannot be re-anchored (ADR-0049); every open builds a fresh
        /// positioner, `wl_surface` and `xdg_popup`.
        popup: Option<Popup>,
        /// The spec the next open builds from. Not a diff baseline like a panel's or window's:
        /// every field is an `xdg_positioner` request consumed at creation, so this is a plain
        /// store, re-read whole at the next [`App::show_popup`].
        spec: PopupSpec,
        /// ADR-0051 decision 2's latch: [`App::pointer_input_count`] when the compositor dismissed
        /// this popup, `None` if not. No replacement opens while the counter is unmoved, or a
        /// click-outside livelocks: `popup_done` destroys the object but leaves `visible = true`,
        /// reopening it on the next re-resolve, forever.
        ///
        /// A count, not a bool (ADR-0051's first amendment): the `visible = false` edge meant to
        /// clear it is unobservable here, so a bool would latch permanently. See
        /// `xdg_shell::popup_visibility_action`, which reads this.
        dismissed_at: Option<u64>,
        /// Which of [`App::show_popup`]'s refusals was last logged for this `visible = true` run,
        /// or `None` if none has (ADR-0049's amendment: log a refusal once, not per re-resolve).
        /// Not a second latch, it only suppresses repeated logging. Cleared on a successful
        /// create or `visible = false`.
        refusal_logged: Option<PopupRefusal>,
    },
    Lock {
        /// The output this lock surface covers, held from instance expansion rather than
        /// looked up when the lock is taken: § 6.4 gives a `lock` no `monitor` to name one with.
        output: wl_output::WlOutput,
        /// `None` until this process holds the lock (ADR-0052 decision 2). Dropping this
        /// handle is the teardown and nothing else is: `SessionLockSurfaceInner::Drop` sends
        /// `ext_session_lock_surface_v1.destroy`, which makes the compositor fall back to a
        /// solid color on outputs still present. Cleared only by an output removal
        /// ([`App::destroy_surface_by_id`]) or the lock ending ([`App::teardown_lock_surfaces`]).
        surface: Option<SessionLockSurface>,
    },
}
impl TrackedRole {
    /// This surface's `wl_surface`, or `None` for a `window`/`popup` not currently shown.
    pub(super) fn wl_surface(&self) -> Option<&wl_surface::WlSurface> {
        match self {
            TrackedRole::Panel { layer, .. } => Some(layer.wl_surface()),
            TrackedRole::Window { window, .. } => window.as_ref().map(WaylandSurface::wl_surface),
            TrackedRole::Popup { popup, .. } => popup.as_ref().map(WaylandSurface::wl_surface),
            TrackedRole::Lock { surface, .. } => surface.as_ref().map(SessionLockSurface::wl_surface),
        }
    }

    /// This surface as something an `xdg_popup` can be rooted under, or `None` if it cannot be one
    /// (§ 6.3's `parent`, ADR-0051 decision 1). A `window`/`popup` not currently shown answers
    /// `None`, so the popup asking is not created either.
    pub(super) fn as_popup_parent(&self) -> Option<PopupParent> {
        match self {
            TrackedRole::Panel { layer, .. } => Some(PopupParent::Layer(layer.clone())),
            TrackedRole::Window { window, .. } => window.as_ref().map(|w| PopupParent::Xdg(w.xdg_surface().clone())),
            TrackedRole::Popup { popup, .. } => popup.as_ref().map(|p| PopupParent::Xdg(p.xdg_surface().clone())),
            // `ext_session_lock_surface_v1` is neither an `xdg_surface` nor a
            // `zwlr_layer_surface_v1`, the only two `get_popup` accepts, so no request can root a
            // popup here. That matches the protocol: while locked the compositor shows lock
            // surfaces only (ADR-0042).
            TrackedRole::Lock { .. } => None,
        }
    }
}
/// Why [`App::show_popup`] declined to open a popup, remembered so the line is not repeated on
/// the next re-resolve while the same refusal still holds (ADR-0049's amendment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PopupRefusal {
    /// `grab = true` and no input event armed a serial this turn (ADR-0051 decision 3).
    Unarmed,
    /// `grab = true` and the compositor advertises no seat to take the grab on.
    Seatless,
    /// § 6.3's `parent` names no surface that is currently shown, which is the ordinary state of a
    /// popup parented to a `window` whose own `visible` is false.
    HiddenParent,
}
/// The two ways a popup gets rooted, which differ in when rather than what.
///
/// `Popup::from_surface` takes an `Option<&xdg_surface>` and roots at creation, covering a
/// `window` or nested `popup`. A `panel`'s surface has a layer-shell role and no `xdg_surface`:
/// `zwlr_layer_surface_v1.get_popup` sends the raw `xdg_popup` back after the popup object exists
/// and before the initial commit, so [`Popup::new`], which commits for you, cannot be used here.
pub(super) enum PopupParent {
    Layer(LayerSurface),
    Xdg(xdg_surface::XdgSurface),
}
pub(super) struct TrackedSurface {
    pub(super) role: TrackedRole,
    pub(super) bound: Option<BoundSurface>,
    /// § 15's "surface_id", the instance id: `"{id}@{output}"` for a panel, bare `id` for a
    /// window. The one id space Lua, the retained `Scene`, this `wl_surface` and the PBA
    /// handshake all share.
    pub(super) surface_id: String,
    pub(super) map_state: MapState,
    /// Set once this surface's null buffer is committed (PBA candidate mode only, § 15.2 points
    /// 2-3); always `false` outside candidate mode. Never set for a `window` not shown, since
    /// staging happens on a configure it will never get. See [`candidate_has_staged`].
    pub(super) null_buffered: bool,
    /// The most recent `configure` size, so [`App::activate_draw`] has a real size to bind EGL
    /// to: in candidate mode the first configure doesn't bind EGL (see [`App::bind_and_clear`]).
    pub(super) configured_size: (u32, u32),
    /// What this surface last actually painted, and at what size, so [`App::paint_surface`] can
    /// skip a repaint that would put down identical pixels.
    ///
    /// Carries the size because the same list at a new size is a different frame: the EGL surface
    /// behind it was resized and its buffer holds nothing. Cleared when the surface is (re)bound,
    /// since a fresh `EGLSurface`'s buffers are undefined.
    ///
    /// `None` means "must paint". Getting invalidation wrong the other way leaves a stale frame on
    /// screen with nothing to trigger a redraw, so every branch that cannot prove the buffer still
    /// matches clears it.
    pub(super) last_painted: Option<((u32, u32), layout::paint::DisplayList)>,
}
/// § 5.1's `visible` at the moment [`App::create_surfaces`] first builds one instance, given what
/// its resolved tree says (`None` when it has none) and which role was declared.
///
/// Role-aware fallback: an absent tree means the startup apply failed and rolled back
/// (`Scene::apply` restores its pre-call state on error). A `panel` (ADR-0038 decision 2) treats
/// it as visible ("keep the shell up"), painting nothing until the next re-resolve. `window`/
/// `popup` have create-and-destroy semantics (ADR-0049 decision 1), so an absent tree is not
/// `visible = true`. A `lock` has no `visible` property (§ 6.4, ADR-0042); `false` stays correct
/// if a future caller reads it.
fn starting_visible(resolved: Option<bool>, roster: &SurfaceSpec) -> bool {
    resolved.unwrap_or(match roster {
        SurfaceSpec::Panel(_) => true,
        SurfaceSpec::Window(_) | SurfaceSpec::Popup(_) | SurfaceSpec::Lock(_) => false,
    })
}
/// One surface's [`SurfaceSpec`] re-derived from its resolved properties, and the § 6 role word
/// for the log line if it fails (ADR-0049's second amendment).
///
/// `roster` contributes only the role; everything else comes from `properties`, already read once
/// this pass (ADR-0044 decision 1). The role cannot come from properties instead: `kind` is what
/// built the roster, and a disagreeing resolved tree would be a reconcile bug.
///
/// One caller, [`App::create_surfaces`]; `apply_resolved_state` does the same parses inline since
/// it dispatches on the tracked role, not a roster entry.
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
/// The surface ids a PBA Candidate both announces in its `ReadySignal` and then draws on
/// `ActivateDraw`. Called from [`App::maybe_send_ready_signal`], using the same
/// [`MapState::presents`] predicate [`App::activate_draw`] skips on.
///
/// The two sets must be identical; either mismatch is fatal in `supervisor/src/reload.rs`'s
/// `drive_handshake`. An announced surface that never draws leaves `collected.len() <
/// expected.len()` waiting past `evidence_timeout`. A drawn surface that was never announced
/// trips `!expected.contains(&surface_id)` and aborts as `PbaFailure::UnexpectedEvidence`.
///
/// A panel declared `visible = false` still stages (ADR-0038 decision 2 creates it regardless)
/// but never presents, so the filter must catch it. `window`/`popup` reach the same answer since
/// ADR-0049 decision 1 never creates their role object when hidden, and a Candidate additionally
/// freezes a `popup`'s [`App::apply_visibility`] with no armed serial to grab with.
///
/// An empty result is legal: `drive_handshake`'s collection loop exits immediately on an empty
/// expected set.
fn presenting_surface_ids<'a>(surfaces: impl Iterator<Item = (&'a str, MapState)>) -> Vec<String> {
    surfaces.filter(|(_, state)| state.presents()).map(|(id, _)| id.to_string()).collect()
}
/// Whether every tracked surface has staged everything a PBA Candidate owes it, which is
/// [`App::maybe_send_ready_signal`]'s gate (§ 15.2 points 2-3). Takes `(null_buffered, exists)`
/// per surface, where `exists` is whether it currently has a Wayland object at all.
///
/// The `exists` half is a gate, not a staging difference: xdg-shell's initial-commit discipline is
/// layer-shell's, so a shown `window` attaches a null buffer on its first configure too. What does
/// not generalize is the plain `all(null_buffered)` assumption that every tracked surface gets a
/// configure. A `panel` always does, created and committed at startup even when `visible` is
/// false. A `window` declared `visible = false` has no `xdg_toplevel` (ADR-0049 decision 1), so
/// `null_buffered` stays false forever and the Candidate never sends `ReadySignal`, a
/// `ready_timeout` hang. A `popup` widens that further: a Candidate freezes `visible`
/// ([`App::apply_visibility`]), so its `xdg_popup` never exists during a handshake.
///
/// A surface with no object has nothing to stage, so it is complete by construction.
/// [`presenting_surface_ids`] filters it out on the same `MapState::Unmapped` that makes it
/// objectless here, keeping the two in step.
fn candidate_has_staged(surfaces: impl Iterator<Item = (bool, bool)>) -> bool {
    surfaces.into_iter().all(|(null_buffered, exists)| null_buffered || !exists)
}

impl App {
    /// One tracked surface per surface instance, built from the evaluation that declared it
    /// (ADR-0038 decision 1, ADR-0049 decision 1).
    ///
    /// The roles diverge only in what "create" means. A `panel` gets its `zwlr_layer_surface_v1`
    /// here whatever `visible` says, since that object lives as long as the generation. A `window`
    /// or `popup` gets a `TrackedSurface` here and its Wayland object only if `visible` already
    /// resolves true, through the same [`App::show_window`] and [`App::show_popup`] a later flip
    /// uses: one creation path, not a startup special case.
    ///
    /// `instances` and `specs` come from the same evaluation, so an instance whose declared id has
    /// no spec cannot happen; it is skipped with a log rather than panicking, the same "keep the
    /// shell up" principle as every other failure in this file.
    ///
    /// `specs` contributes the roster, not the field values ([`resolved_surface_spec`],
    /// [`starting_visible`]): parsed from the evaluation's unresolved properties, so a signal-bound
    /// field is still at its parser placeholder. Right for which declarations exist and what role
    /// each is, neither of which a re-resolve can change (ADR-0049 decision 3).
    ///
    /// Called with the whole instance set at startup, and with only the added instances on a
    /// monitor hotplug (see [`App::handle_output_change`]); the same function either way.
    pub(super) fn create_surfaces(
        &mut self,
        qh: &QueueHandle<App>,
        specs: &[SurfaceSpec],
        instances: &[SurfaceInstance],
    ) {
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
                eprintln!(
                    "[oblisk-renderer] instance {:?} has no matching declaration; skipping",
                    instance.instance_id
                );
                continue;
            };
            // `Scene::surface` hands back an owned `ResolvedNode`, so nothing borrows `self` past
            // this line and the `&mut self` creates below are free to run.
            let tree = self.client.scene().surface(&instance.instance_id);
            let visible = starting_visible(tree.as_ref().map(|tree| tree.visible), roster);
            // Built from the resolved properties, as `apply_resolved_state` builds it on every
            // later pass (ADR-0049's second amendment). `run` parses `specs` from the raw,
            // pre-resolve properties, so a signal-bound field is still at its parser placeholder.
            // For a popup that is permanent damage, not one stale frame: every `PopupSpec` field
            // is an `xdg_positioner` request consumed at `get_popup`, and `xdg_popup.reposition`
            // is not built, so a popup shown from the roster spec keeps `DEFERRED_POPUP_EXTENT`'s
            // 1x1-at-(0,0) placeholder for its whole life. A `panel` goes through the same path:
            // `resolve_properties` copies structural properties through raw either way.
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
        // expectation (ADR-0042). A no-op unless a lock is held right now; on a monitor
        // hotplug it gives the freshly advertised output its lock surface instead of leaving the
        // compositor to paint a solid color there.
        self.ensure_lock_surfaces(qh);
    }

    /// Frees one surface's rendering side, its EGL surface and its `wl_egl_window`, and leaves it
    /// unbound, with its role object untouched. Steps 1 and 2 of the teardown order
    /// [`App::destroy_surface_by_id`] documents; whoever calls this owns step 3.
    ///
    /// Two callers differ in what they do with the role object, not in how they free this half:
    /// `destroy_surface_by_id` drops it, and [`App::hide_window`] drops only the `xdg_toplevel`
    /// and keeps the tracking entry (ADR-0049 decision 1).
    pub(super) fn release_bound(&mut self, index: usize) {
        let Some(bound) = self.surfaces[index].bound.take() else {
            return;
        };
        // `eglDestroySurface`, by hand: `khronos_egl::Surface` is a plain copyable handle with no
        // `Drop`, so without this every unplugged monitor and closed window leaks one EGL surface.
        // Must come before the `wl_egl_window` is destroyed, per [`BoundSurface`]'s contract that
        // the `WlEglSurface` outlives the EGL surface built from it. `self.egl` is `Some` for any
        // surface that reached `bound`, since `ensure_bound` creates both. Written as a guard
        // rather than an `expect`: the cost of being wrong is a leaked EGL surface on a process
        // already tearing one down.
        if let Some(egl) = self.egl.as_ref()
            && let Err(err) = egl.instance.destroy_surface(egl.display, bound.egl_surface)
        {
            log_bind_failure(&self.surfaces[index].surface_id, "eglDestroySurface", err);
        }
        // `BoundSurface`'s drop, which is `wl_egl_window_destroy`.
        drop(bound);
        self.surfaces[index].configured_size = (0, 0);
    }

    /// Destroys one surface instance: its role object, its `wl_surface`, its `wl_egl_window`, and
    /// its EGL surface (ADR-0038 decision 3's removal half). A no-op for an id this process has
    /// no surface for: the normal case for the second of the two events an unplugged monitor
    /// produces, `zwlr_layer_surface_v1::closed` and `OutputHandler::output_destroyed` both
    /// arrive, in either order, and whichever comes first does the work.
    ///
    /// Teardown runs outermost-first. The explicit steps below make that so rather than leaving it
    /// to field order (`TrackedSurface` declares `role` before `bound`, so a plain drop would
    /// destroy the `wl_surface` out from under the `wl_egl_window` still pointing at it):
    ///
    /// 0. Every popup rooted under this surface ([`App::drop_child_popups`]): xdg-shell refuses to
    ///    destroy an `xdg_surface` that still has one.
    /// 1. `eglDestroySurface`, by hand ([`App::release_bound`]).
    /// 2. `BoundSurface`'s drop, which is `wl_egl_window_destroy` (also `release_bound`).
    /// 3. The role object's drop, destroying the role (`zwlr_layer_surface_v1`, or an
    ///    `xdg_toplevel` preceded by its decoration object) and then the `wl_surface`: both
    ///    protocols require that order and SCTK implements it.
    pub(super) fn destroy_surface_by_id(&mut self, instance_id: &str) {
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
    /// PBA candidate mode (`self.is_pba_candidate`, § 15.2 points 2-3) stops after the null
    /// buffer instead: a first configure commits a null buffer directly on the raw `wl_surface`
    /// rather than binding EGL. The Candidate stays invisible, occupying zero on-screen
    /// coordinates, until [`App::activate_draw`] does the real EGL bind later.
    ///
    /// Role-agnostic: xdg-shell's initial-commit discipline is `zwlr_layer_surface_v1`'s
    /// (ADR-0040 decision 4), so an `xdg_toplevel` configure lands here through the same path
    /// with nothing branching on which protocol asked. The two callers differ only in where the
    /// size comes from: layer-shell hands one over, a toplevel's may be the client's to pick
    /// (see `xdg_shell::toplevel_size_for`).
    pub(super) fn bind_and_clear(&mut self, index: usize, width: u32, height: u32) {
        self.surfaces[index].configured_size = (width, height);
        // Only here does a real size for this instance exist: the startup resolve used the whole
        // output's size, and this replaces it with what the compositor actually granted, marking
        // the scene dirty so the next poll turn re-resolves against it.
        //
        // So `paint_surface` below draws the previous resolve, and `run`'s loop repaints with the
        // corrected one on the very next turn: sub-frame, not a visible lag. Re-resolving here
        // instead would run one whole `Scene::apply` per configure in a startup burst, rather than
        // one for the burst, which is what the coalescing flag ADR-0044 decision 2 built is for.
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
        // direction. Matters most on the first configure: no input region has ever been set at
        // that point, and a fullscreen transparent panel whose configured size equals its output's
        // marks the scene clean, so no later re-resolve would set one and the surface would
        // swallow every click meant for the window behind it.
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
                // Committed on every candidate-mode configure rather than only the first: a
                // Candidate has no `swap_buffers` to ride on until `ActivateDraw`, so this is the
                // only commit that can carry the state staged directly above.
                surface.commit();
            } else {
                // `visible = false`: no buffer was ever attached, so this surface is already in
                // the invisible state § 15.2 point 3 asks a Candidate to reach. Marked staged
                // without touching the wire, so `maybe_send_ready_signal`'s gate still completes.
                // The surface is filtered out of the announced set by `presenting_surface_ids`.
                self.surfaces[index].null_buffered = true;
            }
            self.maybe_send_ready_signal();
            return;
        }

        // `!= Mapped`, not "is unmapped": `apply_resolved_state` above may have just issued a
        // re-map commit, and the protocol's wait applies to that too. The configure answering it
        // has not arrived, so no buffer may be attached this pass. See [`App::unmap`].
        if self.surfaces[index].map_state != MapState::Mapped {
            return;
        }

        if !self.ensure_bound(index) {
            return;
        }
        // A repeat configure carrying a new size (a mode change, an exclusive zone shifting a
        // neighbour) has to move the `wl_egl_window` too, or the surface keeps rendering into a
        // buffer sized at its first configure. This is `wayland-egl`'s own resize request, not a
        // rebind: the `WlEglSurface` and the EGL surface built from it both stay valid.
        if let Some(bound) = self.surfaces[index].bound.as_ref() {
            bound.native_window.resize(width.max(1) as i32, height.max(1) as i32, 0, 0);
        }
        self.paint_surface(index);
    }

    /// [`App::apply_resolved_state`] for every tracked surface, which is what the poll loop calls
    /// after a re-resolve actually changed the retained scene. Every surface, not the changed
    /// ones, for exactly the reason [`App::repaint_mapped_surfaces`] gives: ADR-0044 decision 2's
    /// dirty flag is one flag for the whole scene.
    pub(super) fn apply_resolved_surface_state(&mut self) {
        for index in 0..self.surfaces.len() {
            self.apply_resolved_state(index);
        }
    }

    /// Pushes one surface's freshly resolved root back to the compositor: the protocol fields its
    /// role permits changing on a live object, the input region, and whether the surface is shown
    /// at all (ADR-0038 decision 2, ADR-0049 decisions 1-2).
    ///
    /// This is where a `window`'s authoritative [`WindowSpec`] is derived; "resolved" is the whole
    /// point (ADR-0049's second amendment). `crate::socket::surface_specs` parses the unresolved
    /// properties, right for a `panel`'s topology fields since they reject a `Signal` on purpose
    /// (`get_layer_surface` fixes them at creation). A `window`'s `title` is the opposite: § 6.2
    /// spells it `string`/`Signal` precisely so it can move, and parsing at evaluation time would
    /// freeze it at whatever the file last saw. `tree.properties` is a `resolve_properties` result,
    /// so every `Signal` in it has already been read once for this pass (ADR-0044 decision 1), the
    /// same read `visible` and the input region below use.
    ///
    /// All three pushes are double-buffered `wl_surface` state, so they are staged here, not
    /// committed: the caller's commit (`paint_surface`'s `swap_buffers`, or the candidate branch's
    /// own commit) carries the whole update at once. Map, unmap, create and destroy are the
    /// exceptions, being commits by definition.
    ///
    /// The spec push runs before `apply_visibility`, and for a `window` that ordering is
    /// load-bearing: a `visible` flip from false to true creates the `xdg_toplevel` from the
    /// stored spec, so the spec must be this pass's before the object is built from it.
    fn apply_resolved_state(&mut self, index: usize) {
        let surface_id = self.surfaces[index].surface_id.clone();
        // Owned, so the immutable borrow of `self.client` ends before the `&mut self` calls below.
        let Some(tree) = self.client.scene().surface(&surface_id) else {
            // No resolved tree for this instance: a startup whose apply failed, or a re-resolve
            // that rolled back (`Scene::apply` restores its pre-call state on error). Every field
            // stays at what was last applied, the same "keep the last good frame" principle
            // `re_resolve_if_dirty` already follows. Pushing protocol defaults here would resize
            // and un-anchor a working surface over a transient bad capability value.
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
                // A store, not a diff: every field on a `PopupSpec` is an `xdg_positioner` request
                // consumed by `get_popup`, and `xdg_popup.reposition` is not built. There is
                // nothing to send at a live popup. What this push buys is that the next
                // `show_popup` builds its positioner from this pass's `anchor_rect` (ADR-0049's
                // second amendment). The click that opens a dropdown writes the button's rect to a
                // `state` signal on the same turn this reads it.
                Ok(fresh) => {
                    if let TrackedRole::Popup { spec, .. } = &mut self.surfaces[index].role {
                        *spec = fresh;
                    }
                }
                Err(err) => eprintln!(
                    "[oblisk-renderer] {surface_id}: re-resolved popup properties are invalid, keeping the last applied ones: {err}"
                ),
            },
            // Nothing to push, and § 6.4 is why, not an omission: a lock surface has no protocol
            // field a config could set (`ext_session_lock_surface_v1` has exactly one request,
            // `ack_configure`, and the size arrives in the configure rather than being asked for).
            //
            // The create path does call `node::lock_spec`, through [`resolved_surface_spec`], but
            // not in disagreement: that call rebuilds a `SurfaceSpec` for `create_surfaces`'s
            // four-arm `match`, with no equivalent here to feed. Everything past this match still
            // runs for a lock: the input region, and `apply_visibility`, which deliberately does
            // nothing for this role.
            TrackedRole::Lock { .. } => {}
        }
        self.apply_input_region(index, &tree);
        self.apply_visibility(index, tree.visible);
    }

    /// `wl_surface::set_input_region` from this surface's own resolved tree (§ 5.1, ADR-0038
    /// decision 5).
    ///
    /// Per surface, not for one overlay. Three cases fall out of the same code, not three
    /// branches: a root with no visible children yields an empty region, so clicks pass through
    /// to whatever is behind it; a root whose child fills it yields a region covering the
    /// surface, the protocol default already; anything in between, a fullscreen transparent
    /// panel holding one small OSD, gets exactly its visible content.
    ///
    /// The scale is `1.0`, matching `paint_surface`'s: nothing calls `set_buffer_scale`, so
    /// surface-local coordinates and the framebuffer are both at scale 1.
    ///
    /// Not diffed against the last region, unlike the spec fields: this only runs on an actual
    /// re-resolve, and the GPU repaint that follows costs far more than one `wl_region` round
    /// trip.
    ///
    /// Skipped for a `window` not shown: there is no `wl_surface` to set a region on, and
    /// [`App::show_window`]'s first re-resolve after it opens sets one.
    fn apply_input_region(&mut self, index: usize, tree: &layout::ResolvedNode) {
        let Some(surface) = self.surfaces[index].role.wl_surface().cloned() else {
            return;
        };
        let region = match Region::new(&self.compositor_state) {
            Ok(region) => region,
            Err(e) => {
                // Not fatal: the only failure `Region::new` reports is a missing `wl_compositor`,
                // which cannot happen here since `CompositorState::bind` in `run` already
                // succeeded against it. Killing a working shell over an unreachable branch is the
                // worse trade.
                log_bind_failure(&self.surfaces[index].surface_id.clone(), "wl_compositor::create_region", e);
                return;
            }
        };
        for rect in layout::overlay_input_regions(tree, 1.0) {
            region.add(rect.x0, rect.y0, rect.x1 - rect.x0, rect.y1 - rect.y0);
        }
        surface.set_input_region(Some(region.wl_region()));
        // `region` drops here, destroying the `wl_region`. `wl_surface::set_input_region` copies
        // its contents, so the object has no reason to outlive the request.
    }

    /// Applies § 5.1's `visible` to a live surface, by whichever mechanism the role's lifetime
    /// rule calls for (ADR-0038 decision 2, ADR-0049 decisions 1-2).
    ///
    /// The same Lua-facing property, two different mechanics underneath, and this is the one
    /// function where that divergence lives. A `panel`'s Wayland object outlives every flip, so
    /// `visible` is a map or unmap commit. A `window`'s exists only while shown, so `visible` is a
    /// create or a destroy.
    ///
    /// Frozen for a PBA Candidate: this is the one line in this file where a mistake hangs the
    /// shell instead of failing a test. `maybe_send_ready_signal` announces the surfaces this
    /// process will present, and `activate_draw` draws exactly that set. If `visible` could move
    /// between those two points, and it can, since § 15.2 point 2 hydrates a Candidate with cached
    /// capability state precisely in that window, the announced and drawn sets would disagree: an
    /// `evidence_timeout` hang or a `PbaFailure::UnexpectedEvidence` abort (see
    /// [`presenting_surface_ids`]). Freezing makes them agree by construction. The deferred change
    /// applies on the first re-resolve after promotion clears `is_pba_candidate`.
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
            // Its own function, not a third `map_state` arm: a popup answers on two inputs, not
            // one. ADR-0051 decision 2's latch is the second, and a dismissed popup sits in
            // `MapState::Unmapped` with `visible` still true, a state the other roles never reach.
            TrackedRole::Popup { .. } => self.apply_popup_visibility(index, visible),
            // The one role where `visible` is not a property at all: `layout::node::lock_spec`
            // refuses the key, so the `true` this is called with is `parse_visible`'s default. A
            // lock surface's lifetime is the compositor's end to end, created once `locked`
            // arrives, destroyed at `unlock_and_destroy`, so acting on `visible` here could only
            // destroy a surface the compositor is still showing (ADR-0042, ADR-0052 decision 2).
            TrackedRole::Lock { .. } => {}
        }
    }

    /// `zwlr_layer_surface_v1`'s own unmap procedure, taken literally: "Attaching a null buffer to
    /// a layer surface unmaps it." One commit, no destroyed protocol objects, the whole point of
    /// ADR-0038 decision 2: toggling a launcher costs this instead of a process spawn.
    ///
    /// This is the only commit an unmapped surface ever gets. Nothing else in this file may commit
    /// one, because the same description says "the client can re-map the surface by performing a
    /// commit without any buffer attached"; a stray bookkeeping commit here would silently re-map
    /// it.
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
    /// layer_shell.get_layer_surface". `anchor` is included for that reason alone: it never
    /// changes on a live surface, but it can be reset out from under one. The exclusive zone is
    /// not re-sent, it is not a spec field, and the configure re-derives it from the granted size.
    ///
    /// `set_size` needs no `layer::ambiguous_zero_axis` guard: the applied spec's size already
    /// passed it, in [`App::create_panel`] or [`App::apply_spec_change`], which both refuse
    /// rather than store a size the protocol would reject.
    ///
    /// Two starting states share this one request sequence and end in different `MapState`s; the
    /// difference is the compositor's, not a choice made here (measured against niri with
    /// `WAYLAND_DEBUG=1`):
    ///
    /// - A panel declared `visible = false` at startup was never mapped: it performed the initial
    ///   commit, was configured and acked, but never attached a buffer. Its layer-surface state
    ///   was never reset, so this commit changes nothing and no configure comes back. It goes
    ///   straight to [`MapState::Mapped`] and the next [`App::repaint_mapped_surfaces`] draws it.
    /// - A surface that really was mapped and then null-buffered has been reset, so this commit
    ///   is a fresh initial commit and a configure does come back. Attaching a buffer before
    ///   acking it is exactly what the protocol forbids, so that case waits in
    ///   [`MapState::AwaitingConfigure`] and lets `bind_and_clear` handle the configure normally.
    ///
    /// `bound.is_some()` is the honest test for which of the two this is: an EGL surface exists
    /// only for a surface that went through bind-and-paint, and every trip through it ends in a
    /// `swap_buffers`, so the two questions are the same question.
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
        self.surfaces[index].map_state = if was_mapped { MapState::AwaitingConfigure } else { MapState::Mapped };
        eprintln!("[oblisk-renderer] {} mapping: visible = true", self.surfaces[index].surface_id);
    }

    /// Builds the process's one EGL display, config and context if nothing has yet, reporting
    /// whether [`App::egl`] is `Some` afterwards.
    ///
    /// Called from [`App::ensure_bound`] and nowhere else, which is what makes the whole Mesa load
    /// conditional on a surface existing to draw into (ADR-0071). `surface_id` only names the
    /// surface unlucky enough to be first in the log line; the state it builds is shared.
    ///
    /// A failure is fatal, matching every other bind failure in [`App::ensure_bound`]: a PBA
    /// Candidate now signals ready before it has proven it can build a context, so an EGL that
    /// breaks between two generations of one session takes the shell down rather than rolling
    /// back (ADR-0071 decision 3).
    fn ensure_egl(&mut self, surface_id: &str) -> bool {
        if self.egl.is_some() {
            return true;
        }
        // SAFETY (`egl::init`'s): the pointer comes from the `Connection` this `App` owns, so the
        // `wl_display` outlives every EGL object built from it.
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

    /// Creates this surface's `wl_egl_window` and EGL window surface against the shared context if
    /// it has none yet, building that context on the very first call (see [`App::ensure_egl`]) and
    /// initializing the process-wide `glow` context on the first one. Returns whether the
    /// surface is bound afterwards; a failure is fatal (`self.exit`).
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

        // The first surface to get this far is the one that pays for Mesa (ADR-0071). After
        // the two cheap bails above, so a `window` that went invisible mid-bind still costs
        // nothing.
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

        // SAFETY: `native_window.ptr()` is a live `wl_egl_window*` just constructed above by
        // `WlEglSurface::new`, matching `egl.display`/`egl.config`'s own platform -- exactly the
        // handle `eglCreateWindowSurface` requires.
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

        // SAFETY: `glow::Context::from_loader_function`'s contract is that a GL context is
        // current on this thread for the lifetime of the returned `Context` -- guaranteed here
        // by the `eglMakeCurrent` call directly above, on this same single-threaded dispatch
        // loop, with no other context switch between the two.
        self.gl.get_or_insert_with(|| unsafe {
            glow::Context::from_loader_function(|s| {
                egl.instance.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void)
            })
        });

        eprintln!("[oblisk-renderer] {surface_id} up: {width}x{height}, EGL context current");
        self.surfaces[index].bound = Some(BoundSurface { egl_surface, native_window });
        // A new `EGLSurface`'s buffers hold nothing, so whatever the old one had painted is gone
        // and the next paint must be unconditional.
        self.surfaces[index].last_painted = None;
        true
    }

    /// Draws one bound surface's whole retained tree: make its EGL surface current, resize the
    /// shared canvas to it, clear, draw the surface's [`layout::paint::DisplayList`], and swap.
    ///
    /// One `TextPainter` serves every surface. All surfaces share one EGL context; under EGL a
    /// context owns its GL objects while a surface is only the framebuffer being drawn into, so
    /// `eglMakeCurrent` with a different draw surface leaves the canvas's textures, shaders and
    /// glyph atlas valid. Only the canvas's viewport is genuinely per surface, which is what
    /// `TextPainter::resize` (and so `Canvas::set_size`) sets on every call here. If a live run
    /// ever shows otherwise, one canvas per surface is the fallback, not a redesign.
    ///
    /// A surface whose instance has no resolved tree (an evaluation that failed to apply, or an
    /// instance the scene has not resolved yet) is cleared and swapped, not skipped: the buffer
    /// still has to be attached or the compositor keeps showing the last frame.
    ///
    /// ponytail: the paint scale is hardcoded `1.0`, so a HiDPI output renders at scale 1 and the
    /// compositor upscales it. Upgrade path: `set_buffer_scale`, a `WlEglSurface::resize` to the
    /// scaled physical size, and this argument, applied together. Passing scale here alone would
    /// snap geometry to a physical grid the framebuffer lacks.
    fn paint_surface(&mut self, index: usize) {
        // An unmapped surface has no buffer, and one still waiting for the configure that follows
        // its (re-)map commit may not attach one yet (ADR-0038 decision 2; see [`MapState`]).
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

        // Built before anything touches the GL context, because the whole point is what this
        // skips. An unchanged surface costs one tree walk here instead of an `eglMakeCurrent`, a
        // full-surface clear, every draw call in the tree, and an `eglSwapBuffers` the compositor
        // then has to composite into the screen.
        //
        // This is what stops the 1920x1200 wallpaper being redrawn once a second because the
        // clock's seconds digit advanced. ADR-0044 decision 2's dirty flag is one flag for the
        // whole scene, so [`App::repaint_mapped_surfaces`] offers every mapped surface a repaint;
        // comparing the display list is how a surface declines one.
        //
        // An absent tree gives an empty list, not an early return: a surface whose tree went away
        // should paint nothing over its old contents, and it has to reach the clear and swap
        // below to do that.
        let tree = self.client.scene().surface(&surface_id);
        // Scoped so the immutable borrow of `self` that `secure_field_for` holds ends before the
        // painter is borrowed mutably below. Nothing in the list borrows it: `Draw::Text` owns its
        // string.
        let list = {
            let focus = self.secure_field_for(&surface_id);
            tree.as_ref().map(|tree| layout::paint::build(tree, 1.0, focus.as_ref())).unwrap_or_default()
        };
        if self.surfaces[index]
            .last_painted
            .as_ref()
            .is_some_and(|(painted_size, painted)| *painted_size == (width, height) && *painted == list)
        {
            return;
        }

        // Another surface's own paint may have made a different EGL surface current on this
        // thread since this one last drew. The context is shared across every surface, so it is
        // re-established here rather than assumed still current.
        // `Some` for any surface holding an `egl_surface`: `ensure_bound` built both. A surface
        // that never bound never reaches here, `paint_surface`'s caller checks `bound` first.
        let Some(egl) = self.egl.as_ref() else {
            return;
        };
        if let Err(e) = egl.instance.make_current(egl.display, Some(egl_surface), Some(egl_surface), Some(egl.context))
        {
            log_bind_failure(&surface_id, "eglMakeCurrent", e);
            self.exit = true;
            return;
        }

        // SAFETY: every `glow::HasContext` method call requires a current GL context matching
        // `gl`'s own loader -- the `eglMakeCurrent` above is that context, and it's the only one
        // live on this thread.
        if let Some(gl) = self.gl.as_ref() {
            // SAFETY: as above -- the `eglMakeCurrent` earlier in this function bound the context
            // these entry points belong to, on this thread, with nothing switching it since.
            unsafe {
                use glow::HasContext;
                gl.clear_color(0.0, 0.0, 0.0, 0.0);
                gl.clear(glow::COLOR_BUFFER_BIT);
            }
        }

        if self.text_painter.is_none() {
            let font_chain = self.shaping.font_chain_data();
            match TextPainter::new(
                |s| egl.instance.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void),
                width,
                height,
                &font_chain,
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
            layout::paint::execute(painter, &mut self.image_cache, &list, 1.0);
        }

        if let Err(e) = egl.instance.swap_buffers(egl.display, egl_surface) {
            log_bind_failure(&surface_id, "eglSwapBuffers", e);
            self.exit = true;
            return;
        }
        // Only after the swap actually committed: recording a frame that never reached the
        // compositor would let the next identical list skip a paint the screen never got.
        self.surfaces[index].last_painted = Some(((width, height), list));
    }

    /// Repaints every mapped surface, after a re-resolve actually changed the scene. Every surface,
    /// not the changed ones: ADR-0044 decision 2's dirty flag is one flag for the whole scene, so
    /// which surfaces changed is not information this process has.
    ///
    /// This is also the commit that carries everything [`App::apply_resolved_surface_state`] staged
    /// for each surface on the same poll turn: `paint_surface` ends in `swap_buffers`, a
    /// `wl_surface` commit.
    pub(super) fn repaint_mapped_surfaces(&mut self) {
        for index in 0..self.surfaces.len() {
            if self.surfaces[index].map_state != MapState::Mapped {
                continue;
            }
            if self.surfaces[index].bound.is_none() {
                // A panel that started `visible = false` and has just been mapped by a `visible`
                // flip has no EGL surface yet: it was created and configured, but the configure
                // path returned before `ensure_bound` since there was nothing to draw into it.
                // This is the one place that bind can happen, since no further configure is
                // coming (see [`App::remap`]).
                //
                // Never for a Candidate: § 15.2 point 3 keeps it invisible until `ActivateDraw`,
                // and `activate_draw_one` is its only bind.
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
    /// no-op if already sent, or if some surface hasn't staged yet, called on every candidate-mode
    /// configure since any of them might complete the set.
    ///
    /// Two different sets, deliberately: the gate is every surface, since a Candidate is not ready
    /// until each has been dealt with, but the payload is only the surfaces that will present.
    /// See [`presenting_surface_ids`] for what each direction of a mismatch costs.
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

    /// § 15.3: draws the first real frame in response to `ActivateDraw`, requesting
    /// `wp_presentation_feedback` for each surface drawn. `nonce` is remembered as `active_nonce`
    /// so the later `presented` callback knows which handshake attempt to tag its evidence with.
    ///
    /// The surfaces that present, not every tracked surface: exactly the set
    /// `maybe_send_ready_signal` announced, filtered by the same [`MapState::presents`] predicate
    /// over a `map_state` [`App::apply_visibility`] holds still for a Candidate's whole life. That
    /// makes the announced and drawn sets identical, not merely similar. See
    /// [`presenting_surface_ids`] for why "similar" is a hang.
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
        // Promotion completes this process's PBA handshake. From now on it behaves like an
        // ordinary (non-candidate) generation, so a later `configure` (resize, output change, a
        // duplicate ack) must fall through to `bind_and_clear`'s ordinary EGL-bind/resize path,
        // not re-take the null-buffer-staging branch forever: that branch no-ops once
        // `null_buffered` is already `true`, permanently disabling resize. `activate_draw_one`
        // already populated `tracked.bound` in the shape the non-candidate path expects, so
        // flipping this alone is enough.
        self.is_pba_candidate = false;
    }

    /// One tracked surface's `ActivateDraw` response: the same EGL bind
    /// [`App::bind_and_clear`]'s non-candidate path does, plus a `wp_presentation_feedback`
    /// request placed before the paint so it associates with the commit `swap_buffers` performs.
    /// Indexes into `self.surfaces` rather than holding a `&mut TrackedSurface` across the whole
    /// body, since this needs `&mut self` for EGL/GL state and `self.text_painter` at several
    /// points, which a held borrow of one surface would conflict with.
    fn activate_draw_one(&mut self, index: usize, nonce: u64) {
        if !self.ensure_bound(index) {
            return;
        }

        // § 15.3 point 2: request presentation feedback before the commit `paint_surface`'s
        // `swap_buffers` performs, so the request associates with it. Verify with
        // `WAYLAND_DEBUG=1` that `feedback` appears on the wire before the corresponding `commit`.
        if let Some(surface) = self.surfaces[index].role.wl_surface().cloned()
            && let Err(e) = self.presentation_time.feedback(&surface, &self.queue_handle)
        {
            // Not fatal to the whole candidate: the Supervisor's evidence_timeout catches a
            // surface that never presents (ADR-0025). Don't invent a second failure-reporting
            // path here.
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

    /// Resolves a raw `wl_surface` (as handed back by a `wp_presentation_feedback` callback, a
    /// pointer event, or a keyboard focus event) to the tracked surface that owns it.
    ///
    /// `None` is routine, not exceptional, on every one of those paths: a `wl_pointer`, a
    /// `wl_keyboard` and a feedback object are all per seat or per commit, not per surface, so
    /// any of them can name a surface this process has since destroyed: an output change, or a
    /// `visible` flip that took a `window`'s toplevel away (ADR-0049 decision 1).
    pub(super) fn index_of_surface(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.surfaces.iter().position(|s| s.role.wl_surface() == Some(surface))
    }

    /// [`App::index_of_surface`]'s answer as the surface id, shared by `presented`/`discarded`.
    pub(super) fn surface_id_for(&self, surface: &wl_surface::WlSurface) -> Option<&str> {
        self.index_of_surface(surface).map(|index| self.surfaces[index].surface_id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wayland::input::rect_table;

    #[test]
    fn a_declared_but_unlocked_lock_instance_neither_hangs_nor_joins_the_pba_ready_set() {
        // A `lock` instance owns zero Wayland objects until `locked` arrives, so it reaches both PBA
        // gates as `(null_buffered: false, exists: false)` and `MapState::Unmapped` -- complete by
        // construction for the staging gate, absent from the announced set. Getting either wrong is
        // a `ready_timeout` hang or an `UnexpectedEvidence` abort.
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
