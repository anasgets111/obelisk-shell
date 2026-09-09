//! The `panel` role: `zwlr_layer_surface_v1` (§ 6), including protocol mappings, per-field diffs,
//! exclusive zones, creation, updates, and callbacks. Shared bind/paint/(un)map logic is in
//! `surface`.

use super::*;
use crate::wayland::surface::MapState;
use crate::wayland::surface::TrackedRole;

/// `layout`'s `LayerKind` to the protocol stacking level. Pure and unit-testable without a live
/// compositor, unlike most of `wayland/mod.rs`, which is why the mapping lives here.
fn layer_for(kind: LayerKind) -> Layer {
    match kind {
        LayerKind::Background => Layer::Background,
        LayerKind::Bottom => Layer::Bottom,
        LayerKind::Top => Layer::Top,
        LayerKind::Overlay => Layer::Overlay,
    }
}
/// § 6's `anchor` edge booleans to protocol bitflags.
pub(super) fn anchor_for(anchor: node::Anchor) -> Anchor {
    let mut flags = Anchor::empty();
    flags.set(Anchor::TOP, anchor.top);
    flags.set(Anchor::BOTTOM, anchor.bottom);
    flags.set(Anchor::LEFT, anchor.left);
    flags.set(Anchor::RIGHT, anchor.right);
    flags
}
/// § 6's `keyboard_interactivity`. Before ADR-0038 each role hardcoded a mode, preventing
/// `Exclusive` and `None` surfaces from coexisting.
pub(super) fn keyboard_interactivity_for(mode: node::KeyboardInteractivity) -> KeyboardInteractivity {
    match mode {
        node::KeyboardInteractivity::None => KeyboardInteractivity::None,
        node::KeyboardInteractivity::OnDemand => KeyboardInteractivity::OnDemand,
        node::KeyboardInteractivity::Exclusive => KeyboardInteractivity::Exclusive,
    }
}
/// What a `Content` axis asks for before the layout pass has measured one. An invisible root
/// resolves to no geometry at all, so a panel created hidden has nothing to measure; one pixel
/// rather than zero because zero on an axis this surface does not span is the protocol error
/// [`ambiguous_zero_axis`] names. Nothing is ever shown at this extent: [`App::create_panel`]
/// leaves a measured panel's layer object unbuilt until the pass that gives it a tree.
const UNMEASURED_EXTENT: u32 = 1;

/// One `set_size` axis. `0` is "you decide", which is `Fill`, and also what a `Content` axis whose
/// opposite edges are both anchored asks for. The protocol does not require that -- it demands
/// opposite anchors only for an omitted *zero* -- but smithay, and so niri, spans such an axis and
/// drops the size outright (`LayerMap::arrange`: `if anchor.anchored_horizontally() { size.w =
/// source.size.w }`), while wlroots keeps and centres it. Asking for `0` is what makes the two
/// agree, and it keeps `available` honest: on a spanned axis the configure is a real allocation,
/// and a measured request the compositor ignored would leave the tree solved to a width the
/// surface does not have. `Percent` is the only mode that reads the output.
///
/// `spanned` is that both-edges test for this axis, not the whole anchor: a bar anchored left and
/// right spans its width and still measures its height.
fn layer_extent_for(mode: SizeMode, output_extent: f32, measured_extent: f32, spanned: bool) -> u32 {
    match mode {
        SizeMode::Fill => 0,
        SizeMode::Content if spanned => 0,
        // `ceil`, and the same reasoning as `wayland::surface::popup_requested_size`: a card
        // measuring 252.48 given 252 loses the half pixel to the surface's own edge, which is the
        // clipping content sizing exists to end.
        SizeMode::Content => (measured_extent.max(0.0).ceil() as u32).max(UNMEASURED_EXTENT),
        SizeMode::Pixels(px) => px.max(0.0) as u32,
        SizeMode::Percent(fraction) => (output_extent * fraction).max(0.0) as u32,
    }
}
/// Both `set_size` axes for a spec: `output` resolves its percentages, `measured` is the box the
/// layout pass solved for its root.
fn layer_size_for(spec: &PanelSpec, output: layout::LogicalSize, measured: layout::LogicalSize) -> (u32, u32) {
    let anchor = spec.topology.anchor;
    (
        layer_extent_for(spec.width, output.width, measured.width, anchor.left && anchor.right),
        layer_extent_for(spec.height, output.height, measured.height, anchor.top && anchor.bottom),
    )
}
/// Which axes this panel measures from its own tree rather than receives from the compositor, in
/// `SurfaceInstance::measured_axes`' sense: `available` stays the ceiling there and
/// `RendererClient::set_instance_size` leaves it alone, or the size the surface asked for would
/// become the cap on what it may ask for next.
///
/// A spanned `Content` axis is not measured. Its `set_size` is `0` and the compositor's answer is
/// an allocation like `Fill`'s, which is exactly what `available` wants.
///
/// The resolved spec, never `crate::socket::surface_specs`': a signal-bound `width` reads as
/// `Content` on the evaluation pass by design (`parse_size_mode` defers it for
/// [`App::apply_spec_change`] to re-derive), and pinning `available` for one of those would strand
/// a panel sized by a signal at its output's width.
/// The most room a measured axis can actually take, and so what `available` is on one: the output,
/// less the margins on the edges this surface is anchored to. That is the same subtraction the
/// compositor makes before it clamps -- smithay's `LayerMap::arrange` does `source.size.w -=
/// data.margin.left` under `Anchor::LEFT` and then `size.w = size.w.min(source.size.w)`.
///
/// Deliberately not derived from the configure. On a measured axis the granted size is either the
/// size this surface asked for, and feeding that back caps the content at whatever it opened at,
/// or the compositor's clamp of that size, and a ceiling built from clamps only ever ratchets
/// down: it would never recover when the room grew back. Computing it instead means it is right
/// again the moment a margin or an anchor changes.
///
/// What it leaves out is the exclusive zones other clients reserved, which this surface is never
/// told. Content that wants more room than those leave is cut by the compositor's clamp rather
/// than wrapped into what is left.
fn measured_ceiling(spec: &PanelSpec, output: layout::LogicalSize) -> layout::LogicalSize {
    let (anchor, margin) = (spec.topology.anchor, spec.margin);
    let axis = |extent: f32, near: bool, near_margin: f32, far: bool, far_margin: f32| {
        (extent - if near { near_margin } else { 0.0 } - if far { far_margin } else { 0.0 }).max(0.0)
    };
    layout::LogicalSize {
        width: axis(output.width, anchor.left, margin.left, anchor.right, margin.right),
        height: axis(output.height, anchor.top, margin.top, anchor.bottom, margin.bottom),
    }
}
/// What `RendererClient::set_measured_axes` is given for this panel: which axes it measures, and
/// the ceiling those axes are solved against.
pub(super) fn measurement(spec: &PanelSpec, output: layout::LogicalSize) -> ((bool, bool), layout::LogicalSize) {
    (measured_axes(spec), measured_ceiling(spec, output))
}
fn measured_axes(spec: &PanelSpec) -> (bool, bool) {
    let anchor = spec.topology.anchor;
    (
        spec.width == SizeMode::Content && !(anchor.left && anchor.right),
        spec.height == SizeMode::Content && !(anchor.top && anchor.bottom),
    )
}
/// The axis on which `set_size(0, ...)` would be a protocol error. Layer-shell requires both
/// opposite edges for an omitted axis; the error kills the connection, so name and refuse it
/// instead of silently turning an auto-sized request into a full-screen bar.
fn ambiguous_zero_axis(size: (u32, u32), anchor: node::Anchor) -> Option<&'static str> {
    if size.0 == 0 && !(anchor.left && anchor.right) {
        return Some("width");
    }
    if size.1 == 0 && !(anchor.top && anchor.bottom) {
        return Some("height");
    }
    None
}
/// State for a kept panel when `visible` turns true, based on whether its create configure was
/// acked. [`App::bind_and_clear`] records the size; `App::unbind` resets it to `(0, 0)`. No
/// configure carries zero on both axes.
///
/// Startup-created hidden panels are usually already configured. An `osd` shown in the same
/// dispatch turn can still have no ack; attaching then triggers niri's "must ack the initial
/// configure before attaching buffer" error. Measured boot crash rate: roughly one run in four,
/// each costing a generation restart. Waiting costs nothing because [`App::bind_and_clear`] will
/// finish the show when the configure arrives.
fn map_state_for_kept_layer(configured_size: (u32, u32)) -> MapState {
    if configured_size == (0, 0) { MapState::AwaitingConfigure } else { MapState::Mapped }
}
/// An `exclusive` zone from the compositor-configured size, not creation-time guesses (`Fill` has
/// no size yet). One top/bottom edge reserves height; one left/right edge reserves width; all
/// other anchor shapes reserve `0` because the protocol defines only those strips.
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
/// Double-buffered layer-shell changes: `margin`, `keyboard_interactivity`, size, and `exclusive`
/// (ADR-0038 decision 2, § 6). `None` means unchanged. Topology (`id`, `layer`, `anchor`,
/// `monitor`, `namespace`) is absent because `get_layer_surface` consumes namespace/output and
/// edits route to a generation swap instead; `crate::socket::handle_reevaluate` owns that handoff.
/// `is_structural_property` rejects signals there. `output` is the logical output size, not the
/// configured surface size.
#[derive(Debug, Default, PartialEq)]
struct SpecUpdate {
    margin: Option<node::EdgeInsets>,
    keyboard_interactivity: Option<node::KeyboardInteractivity>,
    size: Option<(u32, u32)>,
    /// § 6's mode, not the zone: `Reserve` derives that from [`exclusive_zone_for`].
    exclusive: Option<node::Exclusive>,
}
impl SpecUpdate {
    /// Whether any double-buffered layer-shell request changed and needs a commit.
    fn moved_anything(&self) -> bool {
        self.margin.is_some()
            || self.keyboard_interactivity.is_some()
            || self.size.is_some()
            || self.exclusive.is_some()
    }
}
fn spec_update(
    applied: &PanelSpec,
    fresh: &PanelSpec,
    requested: (u32, u32),
    output: layout::LogicalSize,
    measured: layout::LogicalSize,
) -> SpecUpdate {
    // Against the pair last put on the wire, not against a re-derivation of `applied`: equivalent
    // percent and pixel sizes still match, and so do a spanned `Content` and a `Fill`, but content
    // that grew under a spec that did not change is a size change too -- the one this whole
    // measurement exists for -- and comparing two specs to one measurement cannot see it.
    let wanted = layer_size_for(fresh, output, measured);
    SpecUpdate {
        margin: (fresh.margin != applied.margin).then_some(fresh.margin),
        keyboard_interactivity: (fresh.keyboard_interactivity != applied.keyboard_interactivity)
            .then_some(fresh.keyboard_interactivity),
        size: (wanted != requested).then_some(wanted),
        exclusive: (fresh.exclusive != applied.exclusive).then_some(fresh.exclusive),
    }
}
/// Parameters for [`App::spawn_layer`], bundled for clippy's argument-count limit.
pub(super) struct LayerSpec<'a> {
    layer_type: Layer,
    /// Compositor-visible namespace (§ 6, default `"oblisk-{id}"`), matched by `layerrule`.
    namespace: &'a str,
    /// Always `Some` (ADR-0038 decision 3): one surface per `(surface, output)` pair.
    output: &'a wl_output::WlOutput,
    anchor: Anchor,
    size: (u32, u32),
    margin: node::EdgeInsets,
    keyboard_interactivity: KeyboardInteractivity,
}

impl App {
    /// Creates and configures (but does not commit) a layer-shell surface.
    pub(super) fn spawn_layer(&mut self, qh: &QueueHandle<App>, spec: LayerSpec) -> LayerSurface {
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
        // The exclusive zone uses the configure-time size.
        layer
    }

    /// [`App::create_surfaces`]'s `panel` arm: create, commit, and track one layer surface.
    /// `measured` is the box the layout pass solved for this instance's root, for the axes the
    /// spec measures; zero when there is no tree yet, or when the panel starts hidden.
    pub(super) fn create_panel(
        &mut self,
        qh: &QueueHandle<App>,
        spec: &PanelSpec,
        instance: &SurfaceInstance,
        outputs: &HashMap<String, wl_output::WlOutput>,
        visible: bool,
        measured: layout::LogicalSize,
    ) {
        let Some(output) = outputs.get(&instance.output) else {
            eprintln!(
                "[oblisk-renderer] instance {:?} names an output that has since gone; skipping",
                instance.instance_id
            );
            return;
        };
        // Before the first configure, and before `set_instance_size` has replaced it, `available`
        // is still the output's own size, which is what `Percent` wants.
        let size = layer_size_for(spec, instance.available, measured);
        // Tell the scene which axes this panel measures before its first configure can arrive:
        // that configure is the size this surface asked for, and writing it back into `available`
        // would make the measurement its own ceiling.
        let (axes, ceiling) = measurement(spec, instance.available);
        self.client.set_measured_axes(&instance.instance_id, axes, ceiling);
        // A hidden panel has no tree to measure -- an invisible root resolves to no geometry at all
        // -- so a measured one would be created at `UNMEASURED_EXTENT` and take its first buffer
        // there, a frame wide of what the pass that shows it will have solved. Leave the layer
        // object to [`App::show_panel`], which runs on that pass and is already the path a panel
        // hidden after being shown takes (ADR-0088). PBA staging loses nothing: an `Unmapped`
        // surface presents no frame either way (`MapState::presents`).
        let deferred = !visible && measured_axes(spec) != (false, false);
        if !deferred && let Some(axis) = ambiguous_zero_axis(size, spec.topology.anchor) {
            eprintln!(
                "[oblisk-renderer] surface {:?} leaves its {axis} to the compositor without anchoring both {axis} edges, \
                 which layer-shell rejects as a protocol error; no surface created. Give it an explicit {axis}, or anchor both edges.",
                instance.instance_id
            );
            return;
        }
        let layer = (!deferred).then(|| {
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
            layer
        });

        // A declared `visible = false` panel that is not deferred still needs `get_layer_surface`'s
        // initial commit, but no buffer is attached. Unlike an already-shown hidden panel
        // (ADR-0088), the object has never mapped and can safely remain for PBA staging to see
        // every declared surface. A deferred one has no object at all, which PBA already treats as
        // complete by construction (`surface::candidate_has_staged`).
        self.surfaces.push(TrackedSurface {
            role: TrackedRole::Panel {
                layer,
                output: output.clone(),
                spec: spec.clone(),
                output_size: instance.available,
                measured,
                requested: if deferred { (0, 0) } else { size },
            },
            bound: None,
            surface_id: instance.instance_id.clone(),
            map_state: if visible { MapState::AwaitingConfigure } else { MapState::Unmapped },
            null_buffered: false,
            configured_size: (0, 0),
            last_painted: None,
            stale: false,
            blur_effect: None,
            last_blur_region: Vec::new(),
        });
    }

    /// Shows a `panel` (ADR-0088). The role holding a `LayerSurface` distinguishes two states.
    ///
    /// **Never shown.** [`App::create_panel`] already built a bufferless surface. Painting attaches
    /// the first buffer and maps it only after its configure is acked.
    ///
    /// **Hidden after being shown.** [`App::unmap`] destroyed the object, so rebuild from the last
    /// spec and wait in [`MapState::AwaitingConfigure`]; attaching before the initial ack is the
    /// protocol error niri reports.
    ///
    /// Repeat [`App::create_panel`]'s guard because signal-bound `width`/`height` can turn a fixed
    /// axis into `Fill` after creation; `set_size(_, 0)` would kill the connection.
    pub(super) fn show_panel(&mut self, qh: &QueueHandle<App>, index: usize) {
        let TrackedRole::Panel { layer, spec, output, output_size, measured, .. } = &self.surfaces[index].role else {
            return;
        };
        if layer.is_some() {
            self.surfaces[index].map_state = map_state_for_kept_layer(self.surfaces[index].configured_size);
            eprintln!("[oblisk-renderer] {} mapping: visible = true", self.surfaces[index].surface_id);
            return;
        }
        // `measured` is this pass's: [`App::apply_resolved_state`] writes it from the solved root
        // before it gets here, so a panel measured from its content is created at the size the
        // tree that is about to be painted actually takes.
        let size = layer_size_for(spec, *output_size, *measured);
        if let Some(axis) = ambiguous_zero_axis(size, spec.topology.anchor) {
            eprintln!(
                "[oblisk-renderer] surface {:?} resolved to a {axis} of 0 without anchoring both {axis} edges, \
                 which layer-shell rejects as a protocol error; it stays hidden. Give it an explicit {axis}, or anchor both edges.",
                self.surfaces[index].surface_id
            );
            return;
        }
        // Owned copies, because `spawn_layer` takes `&mut self` and a `LayerSpec` borrows from the
        // role this index owns. A `wl_output` is a refcounted proxy, so its clone is a handle.
        let (layer_type, namespace, anchor, margin, interactivity, output) = (
            layer_for(spec.topology.layer),
            spec.topology.namespace.clone(),
            anchor_for(spec.topology.anchor),
            spec.margin,
            keyboard_interactivity_for(spec.keyboard_interactivity),
            output.clone(),
        );
        let fresh = self.spawn_layer(
            qh,
            LayerSpec {
                layer_type,
                namespace: &namespace,
                output: &output,
                anchor,
                size,
                margin,
                keyboard_interactivity: interactivity,
            },
        );
        fresh.commit();
        if let TrackedRole::Panel { layer, requested, .. } = &mut self.surfaces[index].role {
            *layer = Some(fresh);
            *requested = size;
        }
        self.surfaces[index].map_state = MapState::AwaitingConfigure;
        eprintln!("[oblisk-renderer] {} created: visible = true", self.surfaces[index].surface_id);
    }

    /// Stages `Reserve`'s configured-size zone, explicit `0` for `Respect`, or `-1` for `Ignore`.
    /// Explicit values matter because signal-bound `exclusive` must withdraw a prior reservation.
    /// The caller commits: committing here would split updates, and a bufferless commit on an
    /// unmapped surface is the re-map procedure. Windows have no zone (§ 6).
    pub(super) fn apply_exclusive_zone(&mut self, index: usize) {
        let tracked = &self.surfaces[index];
        let TrackedRole::Panel { layer: Some(layer), spec, .. } = &tracked.role else {
            return;
        };
        let zone = match spec.exclusive {
            node::Exclusive::Reserve => exclusive_zone_for(spec.topology.anchor, tracked.configured_size),
            node::Exclusive::Respect => 0,
            // `-1` is the protocol's "ignore every zone" sentinel.
            node::Exclusive::Ignore => -1,
        };
        layer.set_exclusive_zone(zone);
    }

    /// Diffs the fresh `panel` spec against layer-shell's baseline and sends only changed fields.
    ///
    /// Commits its own changes because `paint_surface` skips byte-identical display lists. Without
    /// this, double-buffered requests could sit pending until an unrelated repaint.
    ///
    /// Measured incident: `panel_host` raised `keyboard_interactivity` to `Exclusive` after its
    /// unchanged card was drawn, but niri never gave it the keyboard. An unrelated border-width
    /// edit changed the display list and `swap_buffers` delivered focus immediately. The bar hid
    /// the bug by redrawing its clock once a second.
    ///
    /// Only a `Mapped`, non-Candidate surface: a bufferless commit would be the protocol's re-map
    /// procedure, and Supervisor services § 14.2 keeps Candidates invisible until `ActivateDraw`.
    pub(super) fn apply_spec_change(&mut self, index: usize, mut fresh: PanelSpec) {
        let TrackedRole::Panel { layer: Some(layer), spec: applied, output_size, measured, requested, .. } =
            &self.surfaces[index].role
        else {
            // No layer object: hidden after being shown (ADR-0088), or deferred at creation because
            // it measures an axis and had no tree to measure. Nothing can be sent, but the spec must
            // still be kept -- [`App::show_panel`] rebuilds from it, and a spec dropped here is one
            // the panel never gets back. Dropping it stranded a panel whose creation-time `width`
            // was refused: every later pass carried a good one and every show retried the bad one.
            if let TrackedRole::Panel { spec, .. } = &mut self.surfaces[index].role {
                *spec = fresh;
            }
            return;
        };
        let update = spec_update(applied, &fresh, *requested, *output_size, *measured);

        if let Some(margin) = update.margin {
            layer.set_margin(margin.top as i32, margin.right as i32, margin.bottom as i32, margin.left as i32);
        }
        if let Some(mode) = update.keyboard_interactivity {
            // Log every change: it takes the keyboard from whatever the user was typing in, and a
            // dead password prompt otherwise cannot distinguish a missing request from compositor
            // inaction.
            eprintln!("[oblisk-renderer] {}: keyboard_interactivity -> {mode:?}", self.surfaces[index].surface_id);
            layer.set_keyboard_interactivity(keyboard_interactivity_for(mode));
        }
        let mut sent = None;
        if let Some(size) = update.size {
            // Signals can turn a fixed height into `Fill`; `set_size(_, 0)` on a singly anchored
            // axis kills the connection, so repeat [`ambiguous_zero_axis`].
            if let Some(axis) = ambiguous_zero_axis(size, fresh.topology.anchor) {
                eprintln!(
                    "[oblisk-renderer] surface {:?} resolved to a {axis} of 0 without anchoring both {axis} edges, \
                     which layer-shell rejects as a protocol error; keeping its previous size. Give it an explicit {axis}, or anchor both edges.",
                    self.surfaces[index].surface_id
                );
                // Keep the old baseline so the next re-resolve retries this size.
                fresh.width = applied.width;
                fresh.height = applied.height;
            } else {
                layer.set_size(size.0, size.1);
                sent = Some(size);
            }
        }

        // End the role borrow before `apply_exclusive_zone` reads the updated spec.
        if let TrackedRole::Panel { spec, requested, .. } = &mut self.surfaces[index].role {
            *spec = fresh;
            if let Some(size) = sent {
                *requested = size;
            }
        }
        if update.exclusive.is_some() || update.size.is_some() {
            self.apply_exclusive_zone(index);
        }
        // After the zone request, keeping the pass to one commit.
        if update.moved_anything()
            && self.surfaces[index].map_state == MapState::Mapped
            && !self.is_pba_candidate
            && let TrackedRole::Panel { layer: Some(layer), .. } = &self.surfaces[index].role
        {
            layer.commit();
        }
    }
}

impl LayerShellHandler for App {
    /// `closed` means this surface is gone, usually because its output was destroyed
    /// (ADR-0038 decision 3), the removal half arriving through layer-shell rather than
    /// `wl_output`. It does not mean the process should shut down. Exiting here would kill a shell
    /// still painting on another monitor.
    ///
    /// ponytail: closing every surface leaves the process alive with nothing on screen. Upgrade:
    /// exit on `closed` only when no output change explains it.
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
        assert_eq!(
            keyboard_interactivity_for(node::KeyboardInteractivity::Exclusive),
            KeyboardInteractivity::Exclusive
        );
    }

    #[test]
    fn a_panel_shown_before_its_first_configure_waits_for_the_ack_instead_of_attaching() {
        assert_eq!(
            map_state_for_kept_layer((0, 0)),
            MapState::AwaitingConfigure,
            "nothing acked yet, so painting here would be the protocol error that kills the connection"
        );
        assert_eq!(
            map_state_for_kept_layer((1920, 39)),
            MapState::Mapped,
            "the ordinary case: created hidden at startup, configured long before anything showed it"
        );
    }

    #[test]
    fn layer_extent_maps_fill_to_the_protocols_zero_and_resolves_a_percent() {
        assert_eq!(
            layer_extent_for(SizeMode::Fill, 1920.0, 300.0, false),
            0,
            "`Fill` means the anchors decide, which the protocol spells 0, whatever the tree measured"
        );
        assert_eq!(layer_extent_for(SizeMode::Pixels(32.0), 1920.0, 300.0, false), 32);
        assert_eq!(layer_extent_for(SizeMode::Percent(0.5), 1920.0, 300.0, false), 960);
    }

    #[test]
    fn a_content_axis_asks_for_what_the_tree_measured_unless_the_surface_spans_it() {
        assert_eq!(
            layer_extent_for(SizeMode::Content, 1920.0, 252.48, false),
            253,
            "`ceil`: 252 would leave the half pixel it asked for to be cut by the surface's own edge"
        );
        assert_eq!(
            layer_extent_for(SizeMode::Content, 1920.0, 252.48, true),
            0,
            "both edges anchored, so layer-shell spans the axis and drops any size given; asking is noise"
        );
        assert_eq!(
            layer_extent_for(SizeMode::Content, 1920.0, 0.0, false),
            UNMEASURED_EXTENT,
            "nothing measured yet -- a hidden root has no geometry -- and 0 here is the protocol error"
        );
    }

    #[test]
    fn a_measured_axiss_ceiling_drops_the_margins_on_the_edges_it_is_anchored_to() {
        // The same subtraction the compositor makes before it clamps. A margin on an edge this
        // surface is not anchored to costs it nothing, because the compositor never applies it.
        let mut stack = panel("stack");
        stack.topology.anchor = node::Anchor { top: true, right: true, bottom: false, left: false };
        stack.margin = node::EdgeInsets { top: 60.0, right: 20.0, bottom: 500.0, left: 500.0 };

        assert_eq!(
            measured_ceiling(&stack, output_1080p()),
            layout::LogicalSize { width: 1900.0, height: 1020.0 },
            "the anchored top and right margins come off; the unanchored bottom and left do not"
        );

        // Never negative: a margin wider than the output leaves no room, not room owed.
        let mut absurd = stack.clone();
        absurd.margin = node::EdgeInsets { top: 4000.0, right: 4000.0, bottom: 0.0, left: 0.0 };
        assert_eq!(measured_ceiling(&absurd, output_1080p()), layout::LogicalSize::default());
    }

    #[test]
    fn only_an_unspanned_content_axis_is_measured_rather_than_allocated() {
        // What `set_instance_size` reads. A spanned `Content` axis gets a real allocation from the
        // compositor, exactly like `Fill`, and `available` should take it.
        let mut bar = panel("bar");
        bar.height = SizeMode::Content;
        assert_eq!(
            measured_axes(&bar),
            (false, true),
            "anchored left and right, so the width is the compositor's; the height is the bar's own"
        );

        let mut covering = bar.clone();
        covering.topology.anchor.bottom = true;
        assert_eq!(measured_axes(&covering), (false, false), "anchoring the fourth edge hands the height back");

        assert_eq!(measured_axes(&panel("bar")), (false, false), "`Fill` and a number are never measured");
    }

    #[test]
    fn a_zero_axis_is_only_legal_when_both_of_that_axiss_edges_are_anchored() {
        // The `set_size` protocol-error rule. Getting this wrong kills the whole connection, so a
        // config that trips it must be refused per surface instead.
        let bar = node::Anchor { top: true, right: true, bottom: false, left: true };
        assert_eq!(ambiguous_zero_axis((0, 32), bar), None, "width 0 is fine: left and right are both anchored");
        assert_eq!(
            ambiguous_zero_axis((0, 0), bar),
            Some("height"),
            "height 0 with only the top edge anchored is the protocol error"
        );

        let corner = node::Anchor { top: true, right: true, bottom: false, left: false };
        assert_eq!(ambiguous_zero_axis((0, 40), corner), Some("width"));
        assert_eq!(ambiguous_zero_axis((380, 40), corner), None, "an explicit size on both axes is always legal");

        let full = node::Anchor { top: true, right: true, bottom: true, left: true };
        assert_eq!(
            ambiguous_zero_axis((0, 0), full),
            None,
            "a fullscreen surface may leave both axes to the compositor"
        );
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
            exclusive: node::Exclusive::Reserve,
            margin: node::EdgeInsets::default(),
            width: SizeMode::Fill,
            height: SizeMode::Pixels(32.0),
        }
    }

    fn output_1080p() -> layout::LogicalSize {
        layout::LogicalSize { width: 1920.0, height: 1080.0 }
    }

    /// For the specs that measure nothing: every axis of `panel` is a `Fill` or a number.
    fn nothing_measured() -> layout::LogicalSize {
        layout::LogicalSize::default()
    }

    /// `spec_update`'s baseline for a surface already carrying `applied`: the pair its last
    /// `set_size` put on the wire.
    fn on_the_wire(applied: &PanelSpec, measured: layout::LogicalSize) -> (u32, u32) {
        layer_size_for(applied, output_1080p(), measured)
    }

    #[test]
    fn a_re_resolve_that_changed_nothing_sends_no_requests_at_all() {
        let applied = panel("bar");
        assert_eq!(
            spec_update(
                &applied,
                &applied.clone(),
                on_the_wire(&applied, nothing_measured()),
                output_1080p(),
                nothing_measured()
            ),
            SpecUpdate::default()
        );
    }

    #[test]
    fn each_in_place_field_is_pushed_on_its_own_and_only_when_it_moved() {
        let applied = panel("bar");

        let mut moved_margin = applied.clone();
        moved_margin.margin = node::EdgeInsets { top: 12.0, right: 12.0, bottom: 0.0, left: 0.0 };
        assert_eq!(
            spec_update(
                &applied,
                &moved_margin,
                on_the_wire(&applied, nothing_measured()),
                output_1080p(),
                nothing_measured()
            ),
            SpecUpdate { margin: Some(moved_margin.margin), ..SpecUpdate::default() }
        );

        let mut takes_typing = applied.clone();
        takes_typing.keyboard_interactivity = node::KeyboardInteractivity::Exclusive;
        assert_eq!(
            spec_update(
                &applied,
                &takes_typing,
                on_the_wire(&applied, nothing_measured()),
                output_1080p(),
                nothing_measured()
            ),
            SpecUpdate {
                keyboard_interactivity: Some(node::KeyboardInteractivity::Exclusive),
                ..SpecUpdate::default()
            }
        );

        let mut stops_reserving = applied.clone();
        stops_reserving.exclusive = node::Exclusive::Respect;
        assert_eq!(
            spec_update(
                &applied,
                &stops_reserving,
                on_the_wire(&applied, nothing_measured()),
                output_1080p(),
                nothing_measured()
            ),
            SpecUpdate { exclusive: Some(node::Exclusive::Respect), ..SpecUpdate::default() }
        );

        // The third mode has to diff as its own value, not collapse into "not reserving": a surface
        // going `Reserve` -> `Ignore` and one going `Reserve` -> `Respect` send different zones
        // (-1 against 0), and a boolean could not tell them apart.
        let mut covers_everything = applied.clone();
        covers_everything.exclusive = node::Exclusive::Ignore;
        assert_eq!(
            spec_update(
                &applied,
                &covers_everything,
                on_the_wire(&applied, nothing_measured()),
                output_1080p(),
                nothing_measured()
            ),
            SpecUpdate { exclusive: Some(node::Exclusive::Ignore), ..SpecUpdate::default() }
        );
    }

    #[test]
    fn content_that_grew_under_an_unchanged_spec_is_still_a_size_change() {
        // The whole point of measuring, and the one case a spec-against-spec diff cannot see: the
        // declaration is identical pass to pass and only the tree under it moved. Caught live --
        // an open panel measured from its content sat at the height it was first shown at while
        // rows were added to it, one `set_size` short of following them.
        let mut applied = panel("stack");
        applied.height = SizeMode::Content;
        applied.topology.anchor = node::Anchor { top: true, right: true, bottom: false, left: false };

        let one_row = layout::LogicalSize { width: 300.0, height: 36.8 };
        let four_rows = layout::LogicalSize { width: 300.0, height: 104.8 };
        let opened = on_the_wire(&applied, one_row);
        assert_eq!(opened, (0, 37), "width unanchored on both sides is still `Fill`'s 0; the height is measured");

        assert_eq!(
            spec_update(&applied, &applied.clone(), opened, output_1080p(), four_rows),
            SpecUpdate { size: Some((0, 105)), ..SpecUpdate::default() },
            "same spec, taller tree: the surface must ask for the room the rows now need"
        );
        assert_eq!(
            spec_update(&applied, &applied.clone(), opened, output_1080p(), one_row),
            SpecUpdate::default(),
            "and a pass that measured the same thing again sends nothing"
        );
    }

    #[test]
    fn a_size_change_is_diffed_as_the_pixels_that_go_on_the_wire_not_as_the_size_mode() {
        let applied = panel("bar");

        let mut taller = applied.clone();
        taller.height = SizeMode::Pixels(48.0);
        assert_eq!(
            spec_update(
                &applied,
                &taller,
                on_the_wire(&applied, nothing_measured()),
                output_1080p(),
                nothing_measured()
            ),
            SpecUpdate { size: Some((0, 48)), ..SpecUpdate::default() },
            "`Fill` stays the protocol's 0 on the width axis; only the height moved"
        );

        // A percent resolves against the *output*, so half of a 1080p height is the same request
        // as an explicit 540, and neither is a change against the other.
        let mut half_by_percent = applied.clone();
        half_by_percent.height = SizeMode::Percent(0.5);
        let mut half_by_pixels = applied.clone();
        half_by_pixels.height = SizeMode::Pixels(540.0);
        assert_eq!(
            spec_update(
                &half_by_percent,
                &half_by_pixels,
                on_the_wire(&half_by_percent, nothing_measured()),
                output_1080p(),
                nothing_measured()
            ),
            SpecUpdate::default()
        );
    }

    #[test]
    fn a_size_change_a_signal_could_make_is_refused_by_the_same_guard_creation_uses() {
        // A `Signal` can turn a fixed 32 into `"Fill"` at runtime, and `set_size(_, 0)` on a
        // surface anchored to one vertical edge is a protocol error that kills the shell. The guard
        // has to run on the update path, not only at creation.
        let applied = panel("bar");
        let mut filled = applied.clone();
        filled.height = SizeMode::Fill;

        let size = spec_update(
            &applied,
            &filled,
            on_the_wire(&applied, nothing_measured()),
            output_1080p(),
            nothing_measured(),
        )
        .size
        .expect("the height moved from 32 to 0");
        assert_eq!(ambiguous_zero_axis(size, filled.topology.anchor), Some("height"));
    }
}
