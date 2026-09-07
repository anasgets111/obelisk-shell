//! Shared `TrackedRole`/`TrackedSurface`/`MapState` bookkeeping, EGL binding, and
//! create/destroy/paint/(un)map lifecycle. Role-specific behavior is in `layer`, `xdg_shell`, and
//! `lock`.

use super::*;

/// A surface bound to shared EGL after its first configure. Field order is load-bearing:
/// wayland-egl requires `WlEglSurface` to outlive the EGL surface, and Rust drops top to bottom;
/// `khronos_egl::Surface` has no `Drop`, so [`App::destroy_surface_by_id`] destroys it explicitly.
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
        let (panel, window, popup, regions, visible) = {
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
                layout::overlay_input_regions(tree, 1.0),
                tree.visible,
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
                if let TrackedRole::Popup { spec, .. } = &mut self.surfaces[index].role {
                    *spec = fresh;
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
        self.apply_visibility(index, visible);
    }

    /// Set the per-surface input region from the resolved tree (§ 5.1, ADR-0038 decision 5): no
    /// visible children means pass-through, a full child covers the surface, and intermediate
    /// content gets its visible geometry. Scale is `1.0` because no buffer scale is set. Do not
    /// diff against the last region: the following GPU repaint costs more than one `wl_region`
    /// round trip. Skip a hidden window with no `wl_surface`; its first post-show re-resolve sets
    /// the region.
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
        let animating = tree.is_some_and(layout::ResolvedNode::animating);
        // End the immutable field-focus borrow before mutably borrowing the painter; `Draw::Text`
        // owns its string.
        let list = {
            let focus = self.field_focus_for(&surface_id);
            tree.as_ref().map(|tree| layout::paint::build(tree, 1.0, focus.as_ref())).unwrap_or_default()
        };
        // A mid-tween surface always commits, even an unchanged list: the frame callback below
        // is only answered after a commit, and a tween whose first tick moved nothing visible
        // would otherwise never get its second (ADR-0145).
        if !animating
            && self.surfaces[index]
                .last_painted
                .as_ref()
                .is_some_and(|(painted_size, painted)| *painted_size == (width, height) && *painted == list)
        {
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
            layout::paint::execute(painter, &mut self.image_cache, &list, 1.0);
        }

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
                surface.last_painted = None;
            }
        }
    }

    pub(super) fn repaint_mapped_surfaces(&mut self) {
        for index in 0..self.surfaces.len() {
            if self.surfaces[index].map_state != MapState::Mapped {
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
