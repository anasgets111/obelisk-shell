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
}
