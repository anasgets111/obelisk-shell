//! The `panel` role: `zwlr_layer_surface_v1` (§ 6.1), including the protocol-value mappings, the
//! per-field diff `apply_spec_change` sends only what moved, and the exclusive-zone computation.
//!
//! Creation, in-place spec updates and the layer-shell configure/closed callbacks all live here;
//! the generic bind/paint/(un)map machinery a `panel` shares with every other role stays in
//! `surface`.

use super::*;
use crate::wayland::surface::MapState;
use crate::wayland::surface::TrackedRole;

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
pub(super) fn anchor_for(anchor: node::Anchor) -> Anchor {
    let mut flags = Anchor::empty();
    flags.set(Anchor::TOP, anchor.top);
    flags.set(Anchor::BOTTOM, anchor.bottom);
    flags.set(Anchor::LEFT, anchor.left);
    flags.set(Anchor::RIGHT, anchor.right);
    flags
}
/// § 6.1's `keyboard_interactivity` to the protocol's own field. Before docs/adr/0038 every
/// surface took a hardcoded mode per Rust-owned role, so a launcher wanting `Exclusive` and an
/// OSD wanting `None` could not coexist.
pub(super) fn keyboard_interactivity_for(mode: node::KeyboardInteractivity) -> KeyboardInteractivity {
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
/// case, not an edge one: it is `parse_size_mode`'s answer for an omitted `width`/`height`, and
/// a surface has no content size at creation time anyway. A percent is the one form that needs
/// the output, which is why this takes it.
pub(super) fn layer_extent_for(mode: SizeMode, output_extent: f32) -> u32 {
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
/// protocol error." A protocol error kills the whole Wayland connection, so a config writing
/// `panel { anchor = { top = true }, height = "Fill" }` would take the Renderer down with nothing
/// on screen to say why.
///
/// Now that the config picks sizes, this is a trust boundary: refuse the surface with a log
/// naming the axis, not invent a size for it -- guessing would silently give a config author a
/// full-screen bar where they asked for an auto-sized one.
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
/// compositor actually configured, not guessed at creation time: a `"Fill"`-sized bar has no
/// height at `get_layer_surface` time, so any zone set there would be a guess the compositor
/// then contradicts.
///
/// One rule: anchored top or bottom but not both reserves its configured height; left or right
/// but not both reserves its width. Everything else is `0` -- all four edges, no edges, or a
/// single corner all leave the edge to reserve against ambiguous, and the protocol's own
/// exclusive-zone wording only defines the strip cases. A bar (`top`, `left`, `right`) lands on
/// the height branch: pinned vertically to one edge, spanning horizontally.
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
/// The layer-shell requests one live surface needs after a re-resolve changed its `panel`
/// properties -- `margin`, `keyboard_interactivity`, size, and the `exclusive` flag the zone is
/// derived from (docs/adr/0038 decision 2, § 6.1). `None` per field means "unchanged, send
/// nothing": these are all double-buffered, so resending an unchanged value is just wire noise.
///
/// [`SurfaceTopology`](node::SurfaceTopology)'s five fields -- `id`, `layer`, `anchor`, `monitor`,
/// `namespace` -- are deliberately absent. The protocol cannot change a surface's namespace or
/// output at all (`get_layer_surface` consumes both), and an edit to any of the five is a topology
/// change `crate::socket::handle_reevaluate` routes to a generation swap instead. So they cannot
/// legitimately differ between `applied` and `fresh` here: each is `is_structural_property`,
/// copied through raw and refused a `Signal` (`layout::node::reject_signal_in_structural_field`).
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

impl App {
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

    /// [`App::create_surfaces`]'s `panel` arm: one `zwlr_layer_surface_v1` on this instance's own
    /// output, initially committed and tracked.
    pub(super) fn create_panel(
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
        // the initial commit above, required by `get_layer_surface` before any configure arrives;
        // what makes it invisible is that no buffer is ever attached, and `MapState::Unmapped` is
        // what keeps `paint_surface` from attaching one. No unmap commit is needed here: on an
        // already-bufferless surface that would be the protocol's re-map procedure, not an unmap --
        // see [`App::remap`], which measured this against a real compositor.
        self.surfaces.push(TrackedSurface {
            role: TrackedRole::Panel { layer, spec: spec.clone(), output_size: instance.available },
            bound: None,
            surface_id: instance.instance_id.clone(),
            map_state: if visible { MapState::AwaitingConfigure } else { MapState::Unmapped },
            null_buffered: false,
            configured_size: (0, 0),
        });
    }

    /// `set_exclusive_zone`, computed from the size the compositor configured (see
    /// [`exclusive_zone_for`]) for a surface the config marked `exclusive`, and an explicit `0`
    /// for one it did not.
    ///
    /// The explicit `0` matters: this used to leave a non-exclusive surface alone entirely, on the
    /// reasoning that the protocol's default zone is already 0 -- true only while `exclusive` could
    /// never change. It is a `Signal`-bindable property (docs/adr/0038 decision 2), so a dock
    /// turning `exclusive = false` has to take back the zone it previously reserved, and the
    /// default is no help once a real value has been sent.
    ///
    /// Stages only; the caller's commit carries it. Committing here would split one surface update
    /// across several commits, and on an unmapped surface a commit with no buffer attached is the
    /// protocol's own re-map procedure (see [`App::unmap`]).
    ///
    /// A no-op on a `window`: an exclusive zone is `zwlr_layer_surface_v1`'s own request, and a
    /// toplevel reserves no screen area -- reserving space is what makes a surface a shell component
    /// instead of a window (§ 6.1, § 6.2).
    pub(super) fn apply_exclusive_zone(&mut self, index: usize) {
        let tracked = &self.surfaces[index];
        let TrackedRole::Panel { layer, spec, .. } = &tracked.role else {
            return;
        };
        let zone = if spec.exclusive { exclusive_zone_for(spec.topology.anchor, tracked.configured_size) } else { 0 };
        layer.set_exclusive_zone(zone);
    }

    /// Diffs one surface's freshly resolved `panel` spec against the one its layer-shell state was
    /// last set from and sends only what moved (see [`spec_update`] for which fields, and for why
    /// the topology ones are not among them).
    pub(super) fn apply_spec_change(&mut self, index: usize, mut fresh: PanelSpec) {
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
            // The same guard `create_panel` runs, and it must run again here, not only at
            // creation: `width`/`height` are resolvable properties, so a `Signal` can turn a fixed
            // height into `"Fill"` at runtime, and a `set_size` of 0 on a singly anchored axis is a
            // protocol error that kills the connection and the whole shell (see
            // [`ambiguous_zero_axis`]).
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
}

impl LayerShellHandler for App {
    /// `zwlr_layer_surface_v1::closed` means this surface is gone and must be destroyed -- the
    /// compositor sends it when the output the surface was on is destroyed, docs/adr/0038
    /// decision 3's removal half arriving by the layer-shell route instead of the `wl_output` one.
    /// It is not a shutdown signal.
    ///
    /// This used to set `self.exit`, defensible while one hardcoded bar was the only surface but
    /// not once a config declares N of them across M monitors: unplugging one external display
    /// would have killed a shell still painting on the laptop panel.
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

    fn output_1080p() -> layout::LogicalSize {
        layout::LogicalSize { width: 1920.0, height: 1080.0 }
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
        // A `Signal` can turn a fixed 32 into `"Fill"` at runtime, and `set_size(_, 0)` on a surface
        // anchored to one vertical edge is a protocol error that kills the shell. The guard has to
        // run on the update path, not only at creation.
        let applied = panel("bar");
        let mut filled = applied.clone();
        filled.height = SizeMode::Fill;

        let size = spec_update(&applied, &filled, output_1080p()).size.expect("the height moved from 32 to 0");
        assert_eq!(ambiguous_zero_axis(size, filled.topology.anchor), Some("height"));
    }
}
