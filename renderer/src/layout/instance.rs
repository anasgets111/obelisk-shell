//! Expanding declared `panel`s into the `(surface, output)` pairs the compositor actually maps
//! (`CONTEXT.md`, Surface instance; docs/adr/0038 decision 3; build-steps.md Phase 20 item 2).
//!
//! One declared surface is not one Wayland surface. A `panel` with `monitor = "All"` targets every
//! connected output, and each of those gets its own `zwlr_layer_surface_v1` with its own
//! configured size -- which is exactly why the retained scene keys by the instance id rather than
//! the declared one (see `layout::scene::Scene`'s doc comment). Everything here is pure: it takes
//! the parsed specs and a snapshot of the outputs and returns the pairs, with no Wayland types
//! anywhere, which is what makes it the testable seam in a file whose neighbours have no headless
//! harness at all.

use crate::layout::node::PanelSpec;
use crate::layout::scene::LogicalSize;

/// One `(panel, output)` pair (`CONTEXT.md`, Surface instance).
///
/// `instance_id` is the one id space that Lua, the retained scene, the Wayland surface, and the
/// PBA handshake all share since docs/adr/0038 -- `supervisor/src/reload.rs` already carried this
/// `"{id}@{output}"` convention through PBA for wallpaper, and this generalizes it to every
/// surface rather than inventing a second scheme next to it.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceInstance {
    /// `"bar@DP-1"`. Keys `layout::scene::Scene`'s surface map and names this surface in every
    /// `ReadySignal`/`PresentationEvidence` frame.
    pub instance_id: String,
    /// `"bar"`. What `layout::node::parse_surface_id` reads off the declared node, and what pairs
    /// this instance back to its `VirtualNode` when the scene resolves.
    pub declared_id: String,
    pub output: String,
    /// The size this instance resolves its tree against. Seeded from the output's own logical size
    /// at startup and replaced per instance by `crate::socket::RendererClient::set_instance_size`
    /// once the compositor configures that surface -- the two differ for any surface smaller than
    /// its output, which is every bar.
    pub available: LogicalSize,
}

/// One connected output, as far as instance expansion cares: a name to match `monitor` against and
/// a size to seed `available` with. Deliberately not `smithay_client_toolkit::output::OutputInfo`
/// -- `layout` holds no Wayland types, and `crate::wayland::App::output_geometries` is the one
/// place that knows how to derive these two fields from a real `wl_output`.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputGeometry {
    pub name: String,
    pub size: LogicalSize,
}

/// Every `(panel, output)` pair `specs` declares against the currently connected `outputs`
/// (docs/adr/0038 decision 3).
///
/// `monitor = "All"` (§ 6.1's default) produces one instance per output, in `outputs` order.
/// Any other value produces one instance for the output whose name matches it, and **none at all**
/// when no output matches. Returning nothing for a miss is the correct answer rather than a
/// silent fallback to some other monitor: a config naming an unplugged display asked for a surface
/// on that display, and putting it somewhere else would be the engine inventing placement policy.
/// Logging the miss is the caller's job, not this function's -- it is pure so that it stays
/// testable, and a log line here would fire once per re-expansion rather than once per config.
///
/// The `"{id}@{output}"` form is uniform, with no special case for a single-output target. A
/// config naming `monitor = "DP-1"` still gets `"bar@DP-1"`, not `"bar"`, so every consumer -- the
/// scene, the Wayland surface map, the PBA handshake -- reads one shape rather than branching on
/// how many outputs a surface happened to match.
pub fn expand_instances(specs: &[PanelSpec], outputs: &[OutputGeometry]) -> Vec<SurfaceInstance> {
    let mut instances = Vec::new();
    for spec in specs {
        for output in outputs {
            if spec.topology.monitor != "All" && spec.topology.monitor != output.name {
                continue;
            }
            instances.push(SurfaceInstance {
                instance_id: format!("{}@{}", spec.topology.id, output.name),
                declared_id: spec.topology.id.clone(),
                output: output.name.clone(),
                available: output.size,
            });
        }
    }
    instances
}

/// What one output change does to a live generation's surface instances (docs/adr/0038 decision 3:
/// "monitor hotplug adds and removes instances in place, with no generation swap"), computed by
/// [`reconcile_instances`].
///
/// Three fields rather than one merged list because the caller does three different things with
/// them: `added` needs a `zwlr_layer_surface_v1` built for it, `removed` needs one destroyed, and
/// `instances` is the whole new set for `crate::socket::RendererClient::set_instances` to resolve
/// against. An instance in neither `added` nor `removed` is deliberately untouched -- its surface
/// keeps its EGL binding, its configure history, and its place on screen.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceReconcile {
    /// Every instance that should exist after the change, in `fresh` order.
    pub instances: Vec<SurfaceInstance>,
    pub added: Vec<SurfaceInstance>,
    /// `instance_id`s whose surface must be destroyed.
    pub removed: Vec<String>,
}

/// Diffs the instance set a generation is currently resolving against the one
/// [`expand_instances`] produces from the outputs now connected.
///
/// A retained instance is carried over from `current` **unchanged**, and that is the one thing
/// this does that a plain re-expansion cannot. `expand_instances` seeds `available` from the
/// output's own logical size; `crate::socket::RendererClient::set_instance_size` has since
/// replaced it with the size the compositor actually configured that surface to (a bar's 1920x32,
/// not its output's 1920x1080). Handing the re-expanded size back would resolve every surviving
/// surface against its whole output until the next `configure`, and a surface whose size did not
/// change gets no further configure at all, so it would simply stay wrong.
///
/// Pure, and testable for exactly the reason the module doc gives: `crate::wayland` has no
/// headless harness, and this is where the add/remove/retain decision actually lives.
pub fn reconcile_instances(current: &[SurfaceInstance], fresh: &[SurfaceInstance]) -> InstanceReconcile {
    let mut instances = Vec::with_capacity(fresh.len());
    let mut added = Vec::new();
    for instance in fresh {
        match current.iter().find(|existing| existing.instance_id == instance.instance_id) {
            Some(existing) => instances.push(existing.clone()),
            None => {
                instances.push(instance.clone());
                added.push(instance.clone());
            }
        }
    }
    let removed = current
        .iter()
        .filter(|existing| !fresh.iter().any(|instance| instance.instance_id == existing.instance_id))
        .map(|existing| existing.instance_id.clone())
        .collect();
    InstanceReconcile { instances, added, removed }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::node::{Anchor, KeyboardInteractivity, LayerKind, SizeMode, SurfaceTopology};

    fn spec(id: &str, monitor: &str) -> PanelSpec {
        PanelSpec {
            topology: SurfaceTopology {
                id: id.to_string(),
                layer: LayerKind::Top,
                anchor: Anchor::default(),
                monitor: monitor.to_string(),
                namespace: format!("oblisk-{id}"),
            },
            keyboard_interactivity: KeyboardInteractivity::None,
            exclusive: false,
            margin: crate::layout::EdgeInsets::default(),
            width: SizeMode::Fill,
            height: SizeMode::Content,
        }
    }

    fn output(name: &str, width: f32, height: f32) -> OutputGeometry {
        OutputGeometry { name: name.to_string(), size: LogicalSize { width, height } }
    }

    #[test]
    fn monitor_all_produces_one_instance_per_output_in_output_order() {
        let outputs = [output("eDP-1", 1920.0, 1080.0), output("DP-1", 3840.0, 2160.0)];
        let instances = expand_instances(&[spec("bar", "All")], &outputs);

        assert_eq!(
            instances.iter().map(|i| i.instance_id.as_str()).collect::<Vec<_>>(),
            ["bar@eDP-1", "bar@DP-1"]
        );
        // The whole reason the scene keys by instance rather than by declared id: a laptop panel
        // and a 4K external resolve against genuinely different sizes, so one tree cannot serve
        // both.
        assert_eq!(instances[0].available, LogicalSize { width: 1920.0, height: 1080.0 });
        assert_eq!(instances[1].available, LogicalSize { width: 3840.0, height: 2160.0 });
        assert_eq!(instances[0].declared_id, "bar");
        assert_eq!(instances[0].output, "eDP-1");
    }

    #[test]
    fn a_named_monitor_produces_exactly_the_matching_output_still_addressed_by_instance_id() {
        let outputs = [output("eDP-1", 1920.0, 1080.0), output("DP-1", 3840.0, 2160.0)];
        let instances = expand_instances(&[spec("dock", "DP-1")], &outputs);

        assert_eq!(instances.len(), 1);
        // Uniform, with no single-output special case -- `"dock"` alone would make every consumer
        // branch on how many outputs a surface happened to match.
        assert_eq!(instances[0].instance_id, "dock@DP-1");
        assert_eq!(instances[0].available, LogicalSize { width: 3840.0, height: 2160.0 });
    }

    #[test]
    fn a_monitor_that_matches_no_connected_output_produces_no_instance_at_all() {
        // A config naming an unplugged monitor gets no surface, rather than one placed somewhere
        // it did not ask for. The caller logs the miss; this function stays pure.
        let outputs = [output("eDP-1", 1920.0, 1080.0)];
        assert!(expand_instances(&[spec("bar", "HDMI-A-9")], &outputs).is_empty());
    }

    #[test]
    fn no_outputs_at_all_produces_no_instances_even_for_monitor_all() {
        assert!(expand_instances(&[spec("bar", "All")], &[]).is_empty());
    }

    #[test]
    fn several_specs_expand_independently_and_keep_spec_order() {
        let outputs = [output("eDP-1", 1920.0, 1080.0), output("DP-1", 2560.0, 1440.0)];
        let instances = expand_instances(&[spec("bar", "All"), spec("dock", "DP-1")], &outputs);

        assert_eq!(
            instances.iter().map(|i| i.instance_id.as_str()).collect::<Vec<_>>(),
            ["bar@eDP-1", "bar@DP-1", "dock@DP-1"]
        );
    }

    fn configured(instance_id: &str, declared_id: &str, output_name: &str, width: f32, height: f32) -> SurfaceInstance {
        SurfaceInstance {
            instance_id: instance_id.to_string(),
            declared_id: declared_id.to_string(),
            output: output_name.to_string(),
            available: LogicalSize { width, height },
        }
    }

    #[test]
    fn a_plugged_in_monitor_adds_one_instance_and_leaves_the_existing_one_alone() {
        let current = [configured("bar@eDP-1", "bar", "eDP-1", 1920.0, 32.0)];
        let fresh = expand_instances(&[spec("bar", "All")], &[output("eDP-1", 1920.0, 1080.0), output("DP-1", 3840.0, 2160.0)]);

        let reconcile = reconcile_instances(&current, &fresh);

        assert_eq!(reconcile.added.iter().map(|i| i.instance_id.as_str()).collect::<Vec<_>>(), ["bar@DP-1"]);
        assert!(reconcile.removed.is_empty());
        assert_eq!(reconcile.instances.iter().map(|i| i.instance_id.as_str()).collect::<Vec<_>>(), ["bar@eDP-1", "bar@DP-1"]);
    }

    #[test]
    fn a_retained_instance_keeps_the_size_the_compositor_configured_not_the_outputs_logical_size() {
        // The whole reason this is not just `expand_instances`'s output. `expand_instances` seeds
        // `available` from the output's logical size, and `RendererClient::set_instance_size`
        // replaced it with the 1920x32 the compositor granted this bar. Handing the re-expanded
        // 1920x1080 back would resolve the bar at full screen height until its next `configure`,
        // and a surface whose size did not change gets no further configure at all.
        let current = [configured("bar@eDP-1", "bar", "eDP-1", 1920.0, 32.0)];
        let fresh = expand_instances(&[spec("bar", "All")], &[output("eDP-1", 1920.0, 1080.0)]);

        let reconcile = reconcile_instances(&current, &fresh);

        assert_eq!(reconcile.instances, current);
        assert!(reconcile.added.is_empty());
        assert!(reconcile.removed.is_empty());
    }

    #[test]
    fn an_unplugged_monitor_removes_only_its_own_instance() {
        let current = [
            configured("bar@eDP-1", "bar", "eDP-1", 1920.0, 32.0),
            configured("bar@DP-1", "bar", "DP-1", 3840.0, 48.0),
        ];
        let fresh = expand_instances(&[spec("bar", "All")], &[output("eDP-1", 1920.0, 1080.0)]);

        let reconcile = reconcile_instances(&current, &fresh);

        assert_eq!(reconcile.removed, ["bar@DP-1"]);
        assert!(reconcile.added.is_empty());
        assert_eq!(reconcile.instances, [current[0].clone()]);
    }

    #[test]
    fn every_output_going_away_removes_every_instance_and_leaves_none() {
        // A laptop lid closing with nothing else attached. The generation survives with no
        // surfaces rather than exiting -- docs/adr/0038 decision 3 handles hotplug in place, and
        // the outputs coming back is another output change, not a new generation.
        let current = [
            configured("bar@eDP-1", "bar", "eDP-1", 1920.0, 32.0),
            configured("dock@eDP-1", "dock", "eDP-1", 64.0, 1080.0),
        ];

        let reconcile = reconcile_instances(&current, &[]);

        assert_eq!(reconcile.removed, ["bar@eDP-1", "dock@eDP-1"]);
        assert!(reconcile.added.is_empty());
        assert!(reconcile.instances.is_empty());
    }

    #[test]
    fn a_swap_of_one_monitor_for_another_adds_and_removes_in_the_same_pass() {
        let current = [configured("bar@eDP-1", "bar", "eDP-1", 1920.0, 32.0)];
        let fresh = expand_instances(&[spec("bar", "All")], &[output("DP-1", 3840.0, 2160.0)]);

        let reconcile = reconcile_instances(&current, &fresh);

        assert_eq!(reconcile.added.iter().map(|i| i.instance_id.as_str()).collect::<Vec<_>>(), ["bar@DP-1"]);
        assert_eq!(reconcile.removed, ["bar@eDP-1"]);
        assert_eq!(reconcile.instances.iter().map(|i| i.instance_id.as_str()).collect::<Vec<_>>(), ["bar@DP-1"]);
    }

    #[test]
    fn a_first_expansion_against_an_empty_current_set_is_all_added() {
        let fresh = expand_instances(&[spec("bar", "All")], &[output("eDP-1", 1920.0, 1080.0)]);

        let reconcile = reconcile_instances(&[], &fresh);

        assert_eq!(reconcile.added, fresh);
        assert_eq!(reconcile.instances, fresh);
        assert!(reconcile.removed.is_empty());
    }
}
