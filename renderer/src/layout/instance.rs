//! Expanding declared surfaces into the instances the compositor actually maps (`CONTEXT.md`,
//! Surface instance; docs/adr/0038 decision 3; build-steps.md Phase 20 item 2 and Phase 22).
//!
//! One declared surface is not one Wayland surface. A `panel` with `monitor = "All"` targets every
//! connected output, and each of those gets its own `zwlr_layer_surface_v1` with its own
//! configured size -- which is exactly why the retained scene keys by the instance id rather than
//! the declared one (see `layout::scene::Scene`'s doc comment). Everything here is pure: it takes
//! the parsed specs and a snapshot of the outputs and returns the instances, with no Wayland types
//! anywhere, which is what makes it the testable seam in a file whose neighbours have no headless
//! harness at all.
//!
//! Per-output is not one rule but three answers, and Phase 22 and Phase 23 are where that stops
//! being the same statement. § 6.2 gives a `window` no `monitor` because the compositor places a
//! toplevel, so one declaration is one instance no matter how many monitors are connected. § 6.4
//! gives a `lock` no `monitor` for the opposite reason: the protocol requires a surface on every
//! output, so one declaration is always every monitor. Only a `panel` expands per output because a
//! property asked it to.

use crate::layout::node::SurfaceSpec;
use crate::layout::scene::LogicalSize;

/// One `(panel, output)` pair (`CONTEXT.md`, Surface instance).
///
/// `instance_id` is the one id space that Lua, the retained scene, the Wayland surface, and the
/// PBA handshake all share since docs/adr/0038 -- `supervisor/src/reload.rs` already carried this
/// `"{id}@{output}"` convention through PBA for wallpaper, and this generalizes it to every
/// surface rather than inventing a second scheme next to it.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceInstance {
    /// `"bar@DP-1"` for a `panel`, and the bare declared id (`"settings"`) for a `window`, which
    /// has no output to qualify it with. Keys `layout::scene::Scene`'s surface map and names this
    /// surface in every `ReadySignal`/`PresentationEvidence` frame.
    pub instance_id: String,
    /// `"bar"`. What `layout::node::parse_surface_id` reads off the declared node, and what pairs
    /// this instance back to its `VirtualNode` when the scene resolves.
    pub declared_id: String,
    /// The output this instance lives on, or empty for a `window`: § 6.2 gives a toplevel no
    /// `monitor` because the compositor is what places it, so there is no output to name.
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

/// Every instance `specs` declares against the currently connected `outputs`, per role
/// (docs/adr/0038 decision 3, docs/adr/0049 decision 1).
///
/// A **`panel`** expands per output. `monitor = "All"` (§ 6.1's default) produces one instance per
/// output, in `outputs` order. Any other value produces one instance for the output whose name
/// matches it, and **none at all** when no output matches. Returning nothing for a miss is the
/// correct answer rather than a silent fallback to some other monitor: a config naming an unplugged
/// display asked for a surface on that display, and putting it somewhere else would be the engine
/// inventing placement policy. Logging the miss is the caller's job, not this function's -- it is
/// pure so that it stays testable, and a log line here would fire once per re-expansion rather than
/// once per config.
///
/// The `"{id}@{output}"` form is uniform across panels, with no special case for a single-output
/// target. A config naming `monitor = "DP-1"` still gets `"bar@DP-1"`, not `"bar"`, so every
/// consumer -- the scene, the Wayland surface map, the PBA handshake -- reads one shape rather than
/// branching on how many outputs a surface happened to match.
///
/// A **`window`** expands to exactly one instance, whatever `outputs` holds, including none: the
/// compositor places a toplevel, so there is no output for the id to be qualified by and no output
/// list for the count to depend on. Its instance exists from startup even though its `xdg_toplevel`
/// does not, and that is the point -- the instance is what makes the scene resolve the window's
/// tree at all, and `visible` is read off that resolved tree (docs/adr/0049 decision 2).
///
/// `available` seeds a window from the first output's logical size, which is a **bound for
/// measuring content against, not a size the window will have**. A `window` root takes § 5.1's
/// `Content` sizing (§ 6.2 gives it no `width`/`height`), so `available` only caps text wrapping
/// until the first `xdg_toplevel` configure, at which point
/// `crate::socket::RendererClient::set_instance_size` replaces it with the size the compositor
/// granted -- exactly a panel's lifecycle. Zero when nothing is connected, which is honest: with no
/// output there is no screen to measure against and nothing is being painted.
///
/// A **`popup`** expands to exactly one instance, for the same reason a `window` does and one more
/// of its own (docs/adr/0051 decision 1). A popup is opened by one click, on one monitor, and it
/// belongs there; expanding it per parent instance would give `click_menu@eDP-1` and
/// `click_menu@DP-1` driven by *one* `visible` signal, so a single click would open a dropdown on
/// every monitor. Its instance id is therefore the bare declared `id`, like a `window`'s, and the
/// parent it roots under is chosen at creation from the click that armed it, not here.
///
/// Its `available` is seeded from the popup's own § 6.3 `width`/`height`, not from an output's, and
/// that is the one place this differs from the other two roles. § 6.3 requires both and gives a
/// popup no `"Fill"`, because a popup has nothing to fill: its size is `xdg_positioner::set_size`'s
/// argument, so the declared size *is* the budget its child is measured against. The first
/// `xdg_popup` configure replaces it through
/// `crate::socket::RendererClient::set_instance_size`, exactly as it does for the other two.
///
/// A **`lock`** expands per output like a `panel`, on the same `"{id}@{output}"` id, and with no
/// filter of any kind (docs/adr/0052 decision 2, build-steps.md Phase 23). The absence of the
/// filter is the whole difference between the two, and it is the protocol's doing rather than a
/// default: `ext-session-lock-v1` says the client "is expected to create lock surfaces for all
/// outputs currently present", and a second surface on one output is a `duplicate_output` error, so
/// there is exactly one legal answer per output. § 6.4 therefore gives a `lock` no `monitor` at
/// all, and offering a choice with one legal value would be worse than not offering it. The
/// per-output `available` matters here for the same reason it does for a `panel` and for more: a
/// lock surface is sized by its own output's configure, and a laptop panel beside a 4K external
/// cannot share one resolved tree.
///
/// Zero outputs produce zero lock instances, and that is not a hole to plug. There is no screen to
/// lock, nothing is being painted, and the surfaces appear when the outputs do -- ADR-0042's
/// "any new outputs as they are advertised" is a re-expansion through this same function, not a
/// separate path.
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
/// of the `"{id}@{output}"` rule [`expand_instances`] applies, and the only place that rule is read
/// back rather than written.
///
/// One caller: `crate::wayland::App`'s popup parent lookup (docs/adr/0051 decision 1), which is
/// handed a declared `parent` by § 6.3 and a set of *instances* by this module, and has to pair
/// them across both spellings -- `"bar@eDP-1"` for a panel and the bare `"settings"` for a window.
///
/// A prefix test rather than a split on `'@'`, because a declared id may itself contain one: § 6.1
/// puts no character restriction on `id`, so `"a@b"` on output `"DP-1"` has to match `"a@b"` and
/// `"a@b@DP-1"`, which a split on the *first* `'@'` gets wrong.
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
    fn a_window_is_one_instance_addressed_by_its_bare_id_however_many_monitors_are_connected() {
        // § 6.2 gives a `window` no `monitor` because the compositor places a toplevel, so the
        // per-output expansion above is a `panel` rule rather than a surface rule.
        let outputs = [output("eDP-1", 1920.0, 1080.0), output("DP-1", 3840.0, 2160.0)];
        let instances = expand_instances(&[window("settings")], &outputs);

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].instance_id, "settings", "no `@output` suffix: there is no output in the declaration to name");
        assert_eq!(instances[0].declared_id, "settings");
        assert!(instances[0].output.is_empty());
        // A bound for measuring the `Content`-sized root against, replaced by `set_instance_size`
        // at the first configure.
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
        // The protocol leaves one legal answer -- "lock surfaces for all outputs currently
        // present", with a `duplicate_output` error for a second on one output -- so § 6.4 gives a
        // `lock` no `monitor` to filter on (docs/adr/0042, docs/adr/0052 decision 2). Same
        // `"{id}@{output}"` shape a `panel` gets, because every consumer downstream reads one id
        // form and a lock instance is no more special to them than a bar on a second monitor is.
        let outputs = [output("eDP-1", 1920.0, 1080.0), output("DP-1", 3840.0, 2160.0)];
        let instances = expand_instances(&[lock("screen-lock")], &outputs);

        assert_eq!(
            instances.iter().map(|i| i.instance_id.as_str()).collect::<Vec<_>>(),
            ["screen-lock@eDP-1", "screen-lock@DP-1"]
        );
        assert_eq!(instances[0].declared_id, "screen-lock");
        assert_eq!(instances[1].output, "DP-1");
        // Per-output sizing, for the reason a panel needs it and one more: a lock surface is sized
        // by its own output's configure, so one resolved tree cannot serve both screens.
        assert_eq!(instances[0].available, LogicalSize { width: 1920.0, height: 1080.0 });
        assert_eq!(instances[1].available, LogicalSize { width: 3840.0, height: 2160.0 });
    }

    #[test]
    fn a_lock_with_no_outputs_connected_expands_to_nothing_rather_than_to_one_unplaced_instance() {
        // Unlike a `window`, which is one instance whatever `outputs` holds. A lock surface is
        // always attached to an output, so with none connected there is nothing to attach to and
        // nothing being painted; the surfaces arrive when the outputs do, through a re-expansion
        // of this same function rather than a second path.
        assert!(expand_instances(&[lock("screen-lock")], &[]).is_empty());
    }

    #[test]
    fn a_popup_gets_one_instance_on_its_bare_id_however_many_monitors_are_connected() {
        // docs/adr/0051 decision 1. The tempting alternative -- one instance per parent instance,
        // `menu@eDP-1` and `menu@DP-1` -- is wrong for the reason that makes it tempting: one
        // `visible` signal drives both, so a single click would open a dropdown on every monitor.
        let instances = expand_instances(&[popup("menu", "bar")], &[output("eDP-1", 1920.0, 1080.0), output("DP-1", 3840.0, 2160.0)]);

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].instance_id, "menu");
        assert_eq!(instances[0].declared_id, "menu");
        assert!(instances[0].output.is_empty(), "the parent instance is chosen at creation, not here");
    }

    #[test]
    fn a_popups_available_is_its_own_declared_size_not_an_outputs() {
        // The one seeding difference between the three roles, and § 6.3 is why: a popup has no
        // `"Fill"` and both axes are required, because the declared size *is*
        // `xdg_positioner::set_size`'s argument and so the budget its child is measured against.
        let instances = expand_instances(&[popup("menu", "bar")], &[output("eDP-1", 1920.0, 1080.0)]);

        assert_eq!(instances[0].available, LogicalSize { width: 200.0, height: 120.0 });
    }

    #[test]
    fn a_popup_still_expands_with_no_outputs_connected_at_all() {
        // For a `window`'s reason plus one of its own: a popup's size does not come from an output
        // in the first place, so there is nothing an empty output list could take away.
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
        // § 6.1 puts no character restriction on `id`, so splitting on the first `'@'` would fail
        // to pair `"a@b@DP-1"` with the surface declared `"a@b"` that actually produced it.
        assert!(is_instance_of("a@b@DP-1", "a@b"));
        assert!(is_instance_of("a@b", "a@b"));
        // And the ambiguity the ponytail names, asserted rather than left to be discovered: this is
        // also what `id = "a"` on an output named `"b@DP-1"` would produce, and no rule reading one
        // string can tell the two apart.
        assert!(is_instance_of("a@b@DP-1", "a"));
    }

    #[test]
    fn a_window_still_expands_with_no_outputs_connected_at_all() {
        // A panel with nothing connected has no surface to be; a window still does, because the
        // compositor owns its placement. Its instance is what makes the scene resolve the tree
        // `visible` is then read off (docs/adr/0049 decision 2).
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
