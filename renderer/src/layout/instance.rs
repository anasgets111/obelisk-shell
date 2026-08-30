//! Expanding declared surfaces into the instances the compositor actually maps (`CONTEXT.md`,
//! Surface instance; docs/adr/0038 decision 3; build-steps.md Phase 20 item 2 and Phase 22).
//!
//! One declared surface is not one Wayland surface. A `panel` with `monitor = "All"` targets every
//! connected output, each with its own `zwlr_layer_surface_v1` and configured size -- why the
//! retained scene keys by instance id rather than declared id (see `Scene`'s doc comment). Pure: no
//! Wayland types, which makes it the testable seam in a file whose neighbours have no headless
//! harness.
//!
//! Per-output resolves to three different answers per role: § 6.2 gives a `window` no `monitor`
//! (the compositor places a toplevel, so one declaration is one instance regardless of monitor
//! count); § 6.4 gives a `lock` no `monitor` for the opposite reason (the protocol requires a
//! surface on every output, so one declaration is always every monitor); only a `panel` expands per
//! output, because a property asked it to.

use crate::layout::node::SurfaceSpec;
use crate::layout::scene::LogicalSize;

/// One `(panel, output)` pair (`CONTEXT.md`, Surface instance).
///
/// `instance_id` is the one id space Lua, the retained scene, the Wayland surface, and the PBA
/// handshake all share (docs/adr/0038): the `"{id}@{output}"` convention `supervisor/src/reload.rs`
/// already used for wallpaper, generalized to every surface.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceInstance {
    /// `"bar@DP-1"` for a `panel`; the bare declared id (`"settings"`) for a `window`, which has no
    /// output to qualify it with. Keys `Scene`'s surface map and names this surface in every
    /// `ReadySignal`/`PresentationEvidence` frame.
    pub instance_id: String,
    /// `"bar"`. What `node::parse_surface_id` reads off the declared node; pairs this instance back
    /// to its `VirtualNode` when the scene resolves.
    pub declared_id: String,
    /// The output this instance lives on, or empty for a `window` (§ 6.2 gives a toplevel no
    /// `monitor`; the compositor places it).
    pub output: String,
    /// The size this instance resolves its tree against. Seeded from the output's logical size at
    /// startup, replaced per instance by `RendererClient::set_instance_size` once the compositor
    /// configures that surface -- differs from the output for any surface smaller than it, which is
    /// every bar.
    pub available: LogicalSize,
}

/// One connected output, as far as instance expansion cares: a name to match `monitor` against and
/// a size to seed `available` with. Not `smithay_client_toolkit::output::OutputInfo` -- `layout`
/// holds no Wayland types; `crate::wayland::App::output_geometries` derives these from a real
/// `wl_output`.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputGeometry {
    pub name: String,
    pub size: LogicalSize,
}

/// Every instance `specs` declares against the currently connected `outputs`, per role
/// (docs/adr/0038 decision 3, docs/adr/0049 decision 1).
///
/// **`panel`**: expands per output. `monitor = "All"` (§ 6.1 default) produces one instance per
/// output, in `outputs` order; any other value matches the one output with that name, or produces
/// no instance at all when nothing matches -- a config naming an unplugged display gets no surface
/// rather than one placed elsewhere. Logging the miss is the caller's job; this stays pure. The
/// `"{id}@{output}"` id form is uniform even for a single-output match (`monitor = "DP-1"` still
/// yields `"bar@DP-1"`, not `"bar"`), so every consumer reads one shape.
///
/// **`window`**: always exactly one instance, whatever `outputs` holds, including none -- the
/// compositor places a toplevel, so there is no output to qualify the id with (docs/adr/0049
/// decision 2). Its instance exists from startup even though its `xdg_toplevel` does not; that
/// instance is what lets the scene resolve the window's tree, and `visible` reads off that tree.
/// `available` seeds from the first output's logical size as a bound for measuring § 5.1's
/// `Content` sizing against, not a size the window will have -- `RendererClient::set_instance_size`
/// replaces it at the first `xdg_toplevel` configure. Zero when nothing is connected.
///
/// **`popup`**: one instance on its bare declared id, for a `window`'s reason and one more
/// (docs/adr/0051 decision 1): expanding per parent instance would mean one `visible` signal drove
/// `menu@eDP-1` and `menu@DP-1` both, opening a dropdown on every monitor from one click. Its
/// parent is chosen at creation from the click that armed it, not here. `available` seeds from the
/// popup's own § 6.3 `width`/`height` (both required, no `"Fill"`), since that size is
/// `xdg_positioner::set_size`'s argument and so the budget its child is measured against.
///
/// **`lock`**: expands per output like a `panel`, with no filter (docs/adr/0052 decision 2,
/// build-steps.md Phase 23): `ext-session-lock-v1` requires "lock surfaces for all outputs
/// currently present" and rejects a second surface on one output with `duplicate_output`, so there
/// is exactly one legal answer per output and no choice for § 6.4 to offer. Zero outputs produce
/// zero lock instances; the surfaces appear when the outputs do, through a re-expansion of this
/// same function (ADR-0042).
pub fn expand_instances(specs: &[SurfaceSpec], outputs: &[OutputGeometry]) -> Vec<SurfaceInstance> {
    let mut instances = Vec::new();
    for spec in specs {
        match spec {
            SurfaceSpec::Panel(panel) => {
                for output in outputs {
                    if panel.topology.monitor != "All" && panel.topology.monitor != output.name {
                        continue;
                    }
                    instances.push(SurfaceInstance {
                        instance_id: format!("{}@{}", panel.topology.id, output.name),
                        declared_id: panel.topology.id.clone(),
                        output: output.name.clone(),
                        available: output.size,
                    });
                }
            }
            SurfaceSpec::Window(window) => instances.push(SurfaceInstance {
                instance_id: window.id.clone(),
                declared_id: window.id.clone(),
                output: String::new(),
                available: outputs.first().map_or(LogicalSize::default(), |output| output.size),
            }),
            SurfaceSpec::Popup(popup) => instances.push(SurfaceInstance {
                instance_id: popup.id.clone(),
                declared_id: popup.id.clone(),
                output: String::new(),
                available: LogicalSize { width: popup.width, height: popup.height },
            }),
            SurfaceSpec::Lock(lock) => {
                for output in outputs {
                    instances.push(SurfaceInstance {
                        instance_id: format!("{}@{}", lock.id, output.name),
                        declared_id: lock.id.clone(),
                        output: output.name.clone(),
                        available: output.size,
                    });
                }
            }
        }
    }
    instances
}

/// Whether `instance_id` names an instance of the surface declared as `declared_id` -- the inverse
/// of the `"{id}@{output}"` rule [`expand_instances`] applies.
///
/// One caller: `crate::wayland::App`'s popup parent lookup (docs/adr/0051 decision 1), which pairs
/// a declared `parent` (§ 6.3) against a set of *instances*, across both spellings -- `"bar@eDP-1"`
/// for a panel, bare `"settings"` for a window.
///
/// A prefix test rather than a split on `'@'`: § 6.1 puts no character restriction on `id`, so
/// `"a@b"` on output `"DP-1"` must match both `"a@b"` and `"a@b@DP-1"`, which splitting on the
/// first `'@'` gets wrong.
///
/// ponytail: the id space is ambiguous at the edges and this cannot fix that, only avoid making it
/// worse. `"a@b@DP-1"` is what `panel { id = "a@b" }` produces on output `"DP-1"` *and* what
/// `panel { id = "a" }` would produce on an output named `"b@DP-1"`, so both answers are "yes" and
/// only one can be right. It bites nothing today -- a `wl_output` name is a connector like
/// `"eDP-1"` and holds no `'@'` -- and the fix is to stop encoding a pair in a string, by carrying
/// `SurfaceInstance`'s own `declared_id` on `crate::wayland::TrackedSurface` instead of re-deriving
/// it here. That is a wider change than this one caller justifies.
pub fn is_instance_of(instance_id: &str, declared_id: &str) -> bool {
    instance_id == declared_id
        || instance_id.strip_prefix(declared_id).is_some_and(|rest| rest.starts_with('@'))
}

/// What one output change does to a live generation's surface instances (docs/adr/0038 decision 3:
/// "monitor hotplug adds and removes instances in place, with no generation swap"), computed by
/// [`reconcile_instances`].
///
/// Three fields because the caller does three things with them: `added` needs a
/// `zwlr_layer_surface_v1` built, `removed` needs one destroyed, `instances` is the whole new set
/// for `RendererClient::set_instances` to resolve against. An instance in neither list is
/// deliberately untouched -- its surface keeps its EGL binding, configure history, and place on
/// screen.
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
/// A retained instance is carried over from `current` **unchanged** -- the one thing a plain
/// re-expansion cannot do. `expand_instances` seeds `available` from the output's logical size;
/// `RendererClient::set_instance_size` has since replaced it with the size the compositor actually
/// configured (a bar's 1920x32, not its output's 1920x1080). Handing back the re-expanded size
/// would resolve a surviving surface against its whole output until the next `configure`, which
/// never comes for a surface whose size did not change -- it would simply stay wrong.
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
    use crate::layout::node::{Anchor, KeyboardInteractivity, LayerKind, PanelSpec, SizeMode, SurfaceTopology, WindowSpec};

    fn spec(id: &str, monitor: &str) -> SurfaceSpec {
        SurfaceSpec::Panel(PanelSpec {
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
        })
    }

    fn window(id: &str) -> SurfaceSpec {
        SurfaceSpec::Window(WindowSpec {
            id: id.to_string(),
            title: "Settings".to_string(),
            app_id: format!("oblisk-{id}"),
            min_size: None,
            max_size: None,
        })
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
        assert_eq!(instances[0].instance_id, "dock@DP-1");
        assert_eq!(instances[0].available, LogicalSize { width: 3840.0, height: 2160.0 });
    }

    #[test]
    fn a_monitor_that_matches_no_connected_output_produces_no_instance_at_all() {
        let outputs = [output("eDP-1", 1920.0, 1080.0)];
        assert!(expand_instances(&[spec("bar", "HDMI-A-9")], &outputs).is_empty());
    }

    #[test]
    fn no_outputs_at_all_produces_no_instances_even_for_monitor_all() {
        assert!(expand_instances(&[spec("bar", "All")], &[]).is_empty());
    }

    #[test]
    fn a_window_is_one_instance_addressed_by_its_bare_id_however_many_monitors_are_connected() {
        let outputs = [output("eDP-1", 1920.0, 1080.0), output("DP-1", 3840.0, 2160.0)];
        let instances = expand_instances(&[window("settings")], &outputs);

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].instance_id, "settings", "no `@output` suffix: there is no output in the declaration to name");
        assert_eq!(instances[0].declared_id, "settings");
        assert!(instances[0].output.is_empty());
        assert_eq!(instances[0].available, LogicalSize { width: 1920.0, height: 1080.0 });
    }

    fn popup(id: &str, parent: &str) -> SurfaceSpec {
        SurfaceSpec::Popup(crate::layout::node::PopupSpec {
            id: id.to_string(),
            parent: parent.to_string(),
            anchor_rect: crate::text::snap::LogicalRect { x: 0.0, y: 0.0, width: 86.0, height: 24.0 },
            width: 200.0,
            height: 120.0,
            anchor: crate::layout::node::PopupAnchor::BottomLeft,
            gravity: crate::layout::node::PopupAnchor::BottomRight,
            constraint_adjustment: crate::layout::node::ConstraintAdjustment::default(),
            offset: crate::layout::node::PopupOffset::default(),
            grab: true,
        })
    }

    fn lock(id: &str) -> SurfaceSpec {
        SurfaceSpec::Lock(crate::layout::node::LockSpec { id: id.to_string() })
    }

    #[test]
    fn a_lock_expands_to_one_instance_per_output_with_no_filter_to_pass_first() {
        let outputs = [output("eDP-1", 1920.0, 1080.0), output("DP-1", 3840.0, 2160.0)];
        let instances = expand_instances(&[lock("screen-lock")], &outputs);

        assert_eq!(
            instances.iter().map(|i| i.instance_id.as_str()).collect::<Vec<_>>(),
            ["screen-lock@eDP-1", "screen-lock@DP-1"]
        );
        assert_eq!(instances[0].declared_id, "screen-lock");
        assert_eq!(instances[1].output, "DP-1");
        assert_eq!(instances[0].available, LogicalSize { width: 1920.0, height: 1080.0 });
        assert_eq!(instances[1].available, LogicalSize { width: 3840.0, height: 2160.0 });
    }

    #[test]
    fn a_lock_with_no_outputs_connected_expands_to_nothing_rather_than_to_one_unplaced_instance() {
        assert!(expand_instances(&[lock("screen-lock")], &[]).is_empty());
    }

    #[test]
    fn a_popup_gets_one_instance_on_its_bare_id_however_many_monitors_are_connected() {
        let instances = expand_instances(&[popup("menu", "bar")], &[output("eDP-1", 1920.0, 1080.0), output("DP-1", 3840.0, 2160.0)]);

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].instance_id, "menu");
        assert_eq!(instances[0].declared_id, "menu");
        assert!(instances[0].output.is_empty(), "the parent instance is chosen at creation, not here");
    }

    #[test]
    fn a_popups_available_is_its_own_declared_size_not_an_outputs() {
        let instances = expand_instances(&[popup("menu", "bar")], &[output("eDP-1", 1920.0, 1080.0)]);

        assert_eq!(instances[0].available, LogicalSize { width: 200.0, height: 120.0 });
    }

    #[test]
    fn a_popup_still_expands_with_no_outputs_connected_at_all() {
        let instances = expand_instances(&[popup("menu", "bar")], &[]);

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].available, LogicalSize { width: 200.0, height: 120.0 });
    }

    #[test]
    fn is_instance_of_matches_both_spellings_and_nothing_else() {
        assert!(is_instance_of("bar@eDP-1", "bar"), "a panel instance carries its output");
        assert!(is_instance_of("settings", "settings"), "a window or popup instance is the bare id");
        assert!(!is_instance_of("bar", "barn"));
        assert!(!is_instance_of("barn@eDP-1", "bar"), "a prefix that is not followed by `@` is a different surface");
        assert!(!is_instance_of("sidebar@eDP-1", "bar"));
    }

    #[test]
    fn is_instance_of_survives_a_declared_id_that_itself_contains_an_at_sign() {
        assert!(is_instance_of("a@b@DP-1", "a@b"));
        assert!(is_instance_of("a@b", "a@b"));
        assert!(is_instance_of("a@b@DP-1", "a"));
    }

    #[test]
    fn a_window_still_expands_with_no_outputs_connected_at_all() {
        let instances = expand_instances(&[window("settings"), spec("bar", "All")], &[]);

        assert_eq!(instances.iter().map(|i| i.instance_id.as_str()).collect::<Vec<_>>(), ["settings"]);
        assert_eq!(instances[0].available, LogicalSize::default());
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
