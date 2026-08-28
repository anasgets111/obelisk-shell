//! The one-pass resolve algorithm and the retained-scene transaction
//! (`docs/oblisk-layout-engine-geometry.md` § 3-5, `CONTEXT.md`'s "Retained scene" entries).
//!
//! `resolve_and_reconcile` is the single recursive walk doing all three of § 3's passes
//! (constraint down, size up, position down) per node in one recursion, matching the spec's
//! literal "single... pass" language, while simultaneously matching fresh nodes to retained ones
//! (`pair_children_by_id_then_position`, § 4 as amended by docs/adr/0045: children carrying an
//! `id` pair by that `id` alone, scoped to their parent, and the id-less ones pair by position
//! among themselves -- ADR-0023's original rule on a smaller set) and retiring removed subtrees
//! child-first (`CONTEXT.md`, Lease).

use std::collections::{HashMap, HashSet};

use mlua::{Lua, Value};

use crate::layout::instance::SurfaceInstance;
use crate::layout::node::{self, Align, EdgeInsets, LayoutError, SizeMode};
use crate::lua::nodes::VirtualNode;
use crate::text::shaping::{ShapeRequest, ShapingHandle};
use crate::text::snap::{LogicalRect, PhysicalRect, snap_to_physical};

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LogicalSize {
    pub width: f32,
    pub height: f32,
}

/// The recursion bound for `resolve_and_reconcile` (build-steps.md Phase 19 item 3): at most this
/// many node levels are admitted, and the level past it is refused, the same "at most N levels"
/// boundary `lua::signal`'s `MAX_SIGNAL_NESTING_DEPTH` uses. Applies to both a literal cyclic tree
/// (`r.children = { r }`) and a computed `children` signal that manufactures fresh depth on every
/// read (`docs/adr/0021`'s amendment, build-steps.md Phase 19 item 3 defect 2): both recurse
/// through this same function, so one counter catches both.
///
/// It exists to turn an abort into a `LayoutError`, not to express a design limit. Sizing it means
/// sizing it *together with* `MAX_SIGNAL_NESTING_DEPTH`, because the two recursions compound: a
/// node level resolves its `children` property, and that resolution can nest signals. Measured on
/// a 2 MiB debug-build test thread (the tightest stack this runs on; in production `wayland::run`
/// owns the Lua VM on the process main thread, 8 MiB by default), stack cost is additive and
/// linear in each:
///
/// - 5,216 B per `resolve_and_reconcile` level;
/// - 14,864 B per nested `Signal::get_value` level of the expensive kind (a `computed` body
///   calling `:get()`, so a full Rust-to-Lua-to-Rust round trip); a plain dependency chain
///   (`s:map(f):map(g)`) is 1,840 B per level, so the body-nesting figure is the worst case.
///
/// The signal nest is popped before descending to the next node level, so it is paid once rather
/// than per level: the compounded worst case is `MAX_TREE_DEPTH * 5216 +
/// MAX_SIGNAL_NESTING_DEPTH * 14864`. Verified against the model at (60, 24): predicted 669,696 B,
/// measured 669,808 B.
///
/// That is why this is 64 and not the 128 it started at. 128 with a 32-deep signal nest peaks at
/// 1,143,296 B, roughly 1.8x margin on a 2 MiB stack -- and 1.8x flatters itself, since the
/// measurement spans only the recursive frames and not the mlua/Lua frames below the deepest one
/// or the error-formatting frames the refusal itself runs at full depth. 64 peaks at 809,472 B,
/// roughly 2.6x, while still leaving 4x headroom above the 10 to 15 levels a real `shell.lua`
/// produces. `MAX_SIGNAL_NESTING_DEPTH` keeps its 32 because it is the cap that now also bounds
/// dependency chains, the shape a real config is likeliest to grow; the tree cap was the cheaper
/// lever, at 2.85x fewer bytes per level.
const MAX_TREE_DEPTH: u32 = 64;


#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(u64);

/// The public, ID-less output of one node's resolution: geometry plus a full passthrough of its
/// properties for a future paint stage. Reused by `Scene::surface` and by `overlay_input_regions`.
///
/// `properties` holds **resolved** values, never a `Signal` handle: `node::resolve_properties` ran
/// over this node's raw map exactly once, at the top of the pass that produced this node
/// (build-steps.md Phase 19 item 5). So this tree is a snapshot of one pass, which is what makes
/// it safe for a later paint stage to read a colour or a radius straight off it without resolving
/// anything itself -- and what guarantees the value it paints is the same one layout measured.
/// The structural keys are the deliberate exception: `node::is_structural_property` copies them
/// through raw on the kinds whose parsers read them, and those parsers reject a `Signal` outright,
/// so no handle reaches here by that route either.
///
/// ponytail: absent and nil are one state in this map. `node::resolve_properties` omits a key whose
/// signal resolved to `Value::Nil` (ADR-0044 decision 1's amendment), so a `background` bound to a
/// capability signal that currently reads `nil` is indistinguishable here from a `background` the
/// config never set. For every property a parser in `layout::node` reads that is exactly right --
/// each has a documented default and the two spellings of "no value here" must agree, which is the
/// argument that rule rests on. For the paint-only properties this comment tells a paint stage to
/// trust, it is a real difference collapsed: that stage will apply its own built-in default to a
/// property the config did bind, at precisely the moments a capability has not answered yet (every
/// `CAPABILITIES` global reads `nil` until the first `StateSnapshot` drains, so this is the state
/// at boot, not an edge case). Distinguishing them means a third state in the map, `Value::Nil`
/// retained as "bound but unresolved", and every parser here re-learning to treat it as absent --
/// not worth it before a paint stage exists to have the opinion.
#[derive(Debug, Clone)]
pub struct ResolvedNode {
    pub kind: String,
    pub rect: LogicalRect,
    pub visible: bool,
    pub properties: HashMap<String, Value>,
    pub children: Vec<ResolvedNode>,
}

/// One retained node: `ResolvedNode`'s geometry plus the `NodeId` identity that lets the next
/// `Scene::apply` decide whether to reuse it or retire it. `properties` is resolved, for the same
/// reason and with the same exception as [`ResolvedNode`]'s -- it is where that one comes from.
///
/// `Clone` exists solely for `Scene::apply`'s rollback snapshot (see its doc comment) -- nothing
/// else in this module needs to duplicate a retained subtree.
#[derive(Clone)]
struct RetainedNode {
    id: NodeId,
    kind: String,
    rect: LogicalRect,
    visible: bool,
    properties: HashMap<String, Value>,
    children: Vec<RetainedNode>,
}

impl RetainedNode {
    fn to_resolved(&self) -> ResolvedNode {
        ResolvedNode {
            kind: self.kind.clone(),
            rect: self.rect,
            visible: self.visible,
            properties: self.properties.clone(),
            children: self
                .children
                .iter()
                .map(RetainedNode::to_resolved)
                .collect(),
        }
    }
}

/// The persistent node tree for one generation (`CONTEXT.md`, Retained scene), keyed per
/// **surface instance** by that instance's `"{id}@{output}"` id (`CONTEXT.md`, Surface instance;
/// `layout::instance`).
///
/// Instance-keyed, not declared-id-keyed, and that is a real distinction rather than a naming
/// choice: a laptop panel and a 4K external genuinely need two resolved trees, because a surface
/// targeting `monitor = "All"` gets one `zwlr_layer_surface_v1` per output and each of those is
/// configured to a different size. One tree per declared surface cannot serve both -- whichever
/// output resolved last would decide the geometry the other one painted.
///
/// This *refines* docs/adr/0045 decision 5 rather than replacing it. The surface `id` is still the
/// reconcile identity: `apply` finds a fresh `VirtualNode` by `node::parse_surface_id` exactly as
/// before, and a surface's `id` is still both its topology identity (`node::SurfaceTopology`) and
/// its reconcile identity (`node::parse_surface_id`'s doc comment). What the key adds is the
/// output half, and that half is stable for an instance's whole life -- an instance is created for
/// one output and dies with it -- so the pair is as much an identity as the `id` alone was.
///
/// Everything below a surface's root reconciles through `pair_children_by_id_then_position`: an
/// optional, per-parent-scoped `id` pairs only against the same `id`, and the children carrying
/// none fall back to ADR-0023's original positional rule among themselves (§ 4 as amended by
/// docs/adr/0045).
#[derive(Default)]
pub struct Scene {
    surfaces: HashMap<String, RetainedNode>,
    /// Removed subtrees, held alive child-first order until [`Scene::release`] finalizes the
    /// drop (`CONTEXT.md`, Lease). Nothing calls `release` in production yet -- see docs/adr/0023
    /// item 7. `Vec`, not a `HashMap`, specifically so insertion order (child-first) is
    /// observable, both for a future consumer and for this module's own tests.
    retiring: Vec<(NodeId, RetainedNode)>,
    next_id: u64,
}

impl Scene {
    pub fn new() -> Self {
        Self::default()
    }

    fn alloc_id(&mut self) -> NodeId {
        let id = NodeId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Reconciles one retained tree per entry in `instances` into the retained scene, each keyed by
    /// its `instance_id` and resolved against that instance's own `available` size
    /// (build-steps.md Phase 20 items 2 and 4; this is what replaced `socket.rs`'s deleted
    /// hardcoded 1920x40 placeholder size, closing docs/adr/0023 item 6).
    ///
    /// The two arguments answer two different questions and neither implies the other.
    /// `fresh_surfaces` is what the config *declared*; `instances` is what the compositor is
    /// actually being asked to map (`layout::instance::expand_instances`). So a declared surface
    /// with no instance -- a `monitor` naming an unplugged display -- resolves not at all, which is
    /// correct: there is no output to resolve it against and nothing on screen for it to be. The
    /// reverse, an instance naming a surface `fresh_surfaces` does not contain, is a caller bug
    /// rather than a config one (the two come from the same evaluation), so it raises
    /// [`LayoutError::InvalidProperty`] rather than being skipped.
    ///
    /// An instance id present in the retained scene but absent from `instances` this cycle is left
    /// untouched: a `surface` disappearing entirely is a topology change (`CONTEXT.md`), handled
    /// by a generation swap, not this in-place apply.
    ///
    /// Rolls back to exactly its pre-call state on `Err` (`CONTEXT.md`, Rollback; `socket.rs`'s
    /// `handle_reevaluate` already assumes this -- it deliberately leaves `applied_topology`
    /// unchanged on a failed apply). Before property resolve could call back into Lua (ADR-0044
    /// decision 1), every property was an inert `mlua::Value`, so a config that applied once
    /// always applied again and a plain remove-then-insert-per-surface loop was safe. Now a
    /// Signal getter can fail at any depth inside `resolve_and_reconcile`, partway through
    /// mutating `self.next_id` (`alloc_id`) and `self.retiring` (`retire_child_first`), and
    /// partway through a multi-surface `fresh_surfaces` list. Snapshotting `surfaces`/`next_id`/
    /// `retiring` up front and restoring the snapshot wholesale on any error is the smallest
    /// change that actually holds the invariant for all three -- `resolve_and_reconcile` still
    /// takes retained state by value and still mutates `next_id`/`retiring` as it walks, so
    /// nothing short of restoring a pre-walk snapshot undoes that once an error has propagated
    /// up from an arbitrary depth.
    ///
    /// ponytail: the snapshot is taken unconditionally, so every apply deep-clones the retained
    /// tree whether or not anything fails, and item 2's dirty flag turns that into once per
    /// capability push rather than once per config edit. The clone is structural (a
    /// `RetainedNode`'s `mlua::Value` properties are refcount bumps, not deep copies), so it is
    /// O(nodes), not O(Lua heap). This comment used to name build-steps.md Phase 19 item 5 as the
    /// fix, on the theory that resolving every property up front would finish all the fallible
    /// work before any state was touched. Item 5 landed and that turned out to be wrong: a parent
    /// must know a child's `margin` before it can hand that child a budget, so resolution is
    /// interleaved with the walk rather than preceding it, and a Signal getter can still fail at
    /// arbitrary depth with `next_id` and `retiring` already mutated. Removing the snapshot needs
    /// the whole fresh tree resolved into a separate tree first, which is a second traversal and a
    /// second allocation to save a clone that is already O(nodes).
    pub fn apply(
        &mut self,
        fresh_surfaces: &[VirtualNode],
        instances: &[SurfaceInstance],
        shaping: &ShapingHandle,
        lua: &Lua,
    ) -> Result<(), LayoutError> {
        let next_id_snapshot = self.next_id;
        let retiring_snapshot_len = self.retiring.len();
        let surfaces_snapshot = self.surfaces.clone();

        for instance in instances {
            if let Err(err) = self.apply_one_instance(fresh_surfaces, instance, shaping, lua) {
                self.surfaces = surfaces_snapshot;
                self.next_id = next_id_snapshot;
                self.retiring.truncate(retiring_snapshot_len);
                return Err(err);
            }
        }
        Ok(())
    }

    /// One instance's worth of `apply`'s loop body, split out so `apply` can wrap it in a single
    /// early-return-on-error site instead of duplicating the rollback at every `?`.
    fn apply_one_instance(
        &mut self,
        fresh_surfaces: &[VirtualNode],
        instance: &SurfaceInstance,
        shaping: &ShapingHandle,
        lua: &Lua,
    ) -> Result<(), LayoutError> {
        // Matched by the declared `id`, keyed by the instance id: docs/adr/0045 decision 5's
        // reconcile identity, resolved per output (see this type's doc comment).
        let mut fresh = None;
        for candidate in fresh_surfaces {
            if node::parse_surface_id(&candidate.properties)? == instance.declared_id {
                fresh = Some(candidate);
                break;
            }
        }
        let Some(fresh) = fresh else {
            return Err(node::invalid(
                "id",
                format!(
                    "surface instance `{}` names a surface `{}` this evaluation did not declare -- \
                     instances and declarations come from the same evaluation, so this is a caller bug, not a config error",
                    instance.instance_id, instance.declared_id
                ),
            ));
        };
        let key = instance.instance_id.clone();
        let available = instance.available;
        let existing = self.surfaces.remove(&key);
        // The root's one resolve for this pass, gated by the same admissibility check its children
        // get in the loop below, for the same reason: resolution runs Lua, so a node the walk will
        // refuse must not run any first. Every node below this one is resolved by its own parent,
        // in the child loop that needs its `margin` before it can recurse (build-steps.md Phase 19
        // item 5).
        ensure_node_admissible(&fresh.kind, 0)?;
        let properties = node::resolve_properties(&fresh.properties, &fresh.kind, lua)?;
        // **An unsized `window` root is its surface** (build-steps.md Phase 22 item 1). A `panel`
        // root sizes itself from § 6.1's `width`/`height`, which are also its
        // `zwlr_layer_surface_v1::set_size` request; § 6.2 gives a `window` neither, because a
        // toplevel's size is the compositor's and arrives as an `xdg_toplevel` configure that
        // `set_instance_size` has already turned into this `available`.
        //
        // Without this the root fell to `parse_size_mode`'s `Content` default, and a
        // `Content`-sized parent hands its children a budget of zero (see `child_budget` in
        // `resolve_and_reconcile`), so `child = column { width = "Fill" }` -- the obvious way to
        // write a window -- resolved to nothing and the toplevel mapped a fully transparent buffer.
        // Measured against niri, which configured the window at its 1920x1168 tile and had a 0x0
        // tree painted into it.
        //
        // Only the `Content` default is overridden, per axis. A `window` that does write a `width`
        // is writing a property § 6.2 does not define, and the answer to that is to honour it like
        // any other node's rather than to silently discard it.
        let forced = if fresh.kind == "window" {
            (
                matches!(node::parse_size_mode(&properties, "width")?, SizeMode::Content).then_some(available.width),
                matches!(node::parse_size_mode(&properties, "height")?, SizeMode::Content).then_some(available.height),
            )
        } else {
            (None, None)
        };
        let reconciled =
            resolve_and_reconcile(self, existing, &fresh.kind, properties, available, shaping, forced.0, forced.1, lua, 0)?;
        self.surfaces.insert(key, reconciled);
        Ok(())
    }

    /// One surface instance's resolved tree, by its `"{id}@{output}"` instance id
    /// (`layout::instance::SurfaceInstance::instance_id`) -- not by the declared `id` a config
    /// writes. `crate::wayland::App::paint_surface` looks a tree up with exactly the id its
    /// `TrackedSurface` carries, which is what makes the two id spaces one (docs/adr/0038).
    pub fn surface(&self, instance_id: &str) -> Option<ResolvedNode> {
        self.surfaces.get(instance_id).map(RetainedNode::to_resolved)
    }

    /// Finalizes the drop of one retired subtree. Returns `false` if `id` isn't currently
    /// retiring (already released, or never retired).
    ///
    /// ponytail: no production caller yet -- nothing in this codebase owns a per-node GPU
    /// resource to guard, so nothing needs to signal "done consuming this retired subtree." The
    /// mechanism is built ahead of that consumer per build-steps.md Phase 12's own instruction
    /// (docs/adr/0023 item 7), matching `supervisor/src/socket.rs`'s `GenerationRegistry::send_to`
    /// precedent from Phase 9. Exercised by this module's own tests only.
    #[allow(dead_code)]
    pub fn release(&mut self, id: NodeId) -> bool {
        if let Some(pos) = self.retiring.iter().position(|(rid, _)| *rid == id) {
            self.retiring.remove(pos);
            true
        } else {
            false
        }
    }

    /// Releases every retired subtree at once, in the child-first order [`Self::retire_child_first`]
    /// established. `crate::socket`'s `RendererClient` calls this after each successful
    /// `Scene::apply`, which is what stops the lease bag growing forever now that an apply runs at
    /// capability-push cadence (ADR-0044 decision 2) rather than once per config edit: every
    /// re-resolve that shortens a `children` list retires the tail, and each retired
    /// `RetainedNode` holds a `HashMap<String, mlua::Value>`, so an undrained bag leaks Lua heap
    /// as well as Rust memory in a process meant to live for a whole session.
    ///
    /// Unconditional, and only correct because it is: nothing in this codebase holds a lease
    /// today, so nothing can be mid-teardown when this runs (docs/adr/0023 item 7, the same fact
    /// [`Self::release`]'s own ponytail records).
    ///
    /// ponytail: this is the wrong shape the moment a real lease holder exists. Once a paint stage
    /// owns per-node GPU resources, release has to be driven by that holder dropping its lease --
    /// per node, when it is actually done with it -- and dropping a node here while it still owns
    /// a texture would free a resource out from under it. This call is what has to change then:
    /// it becomes the holder's own `release` calls, and `apply` stops being the trigger at all.
    pub fn release_all_retired(&mut self) {
        self.retiring.clear();
    }

    /// Ids currently held in the lease bag, in child-first insertion order. A diagnostic/future
    /// consumer accessor, not needed by `apply`/`release` themselves.
    ///
    /// ponytail: no production caller yet, same reason as [`Self::release`]. Exercised by this
    /// module's own tests only.
    #[allow(dead_code)]
    pub fn retiring_ids(&self) -> Vec<NodeId> {
        self.retiring.iter().map(|(id, _)| *id).collect()
    }

    /// Moves `node` and all its descendants into `retiring`, children before their parent
    /// (`CONTEXT.md`, Lease: "tears down removed subtrees child-first so a parent never frees a
    /// resource a child still holds").
    fn retire_child_first(&mut self, node: RetainedNode) {
        let RetainedNode {
            id,
            kind,
            rect,
            visible,
            properties,
            children,
        } = node;
        for child in children {
            self.retire_child_first(child);
        }
        self.retiring.push((
            id,
            RetainedNode {
                id,
                kind,
                rect,
                visible,
                properties,
                children: Vec::new(),
            },
        ));
    }
}

/// `window` and `popup` join `panel` here per docs/adr/0040 decision 1 (build-steps.md Phase 22).
/// All three are surface containers: a role for a `wl_surface`, a `child` tree inside it, and no
/// layout model of their own beyond the stacking one docs/adr/0023 item 4 already gives `panel`.
/// `lock`, the fourth role, is not here -- its ownership is still open (docs/adr/0042).
fn ensure_supported_kind(kind: &str) -> Result<(), LayoutError> {
    match kind {
        "panel" | "window" | "popup" | "rect" | "row" | "column" | "text" | "icon" | "button" | "list" | "textfield" => Ok(()),
        other => Err(LayoutError::UnsupportedNodeKind(other.to_string())),
    }
}

/// Everything a node has to pass before *anything* reads its properties: a supported kind, and a
/// level within [`MAX_TREE_DEPTH`]. Called at each site about to run `node::resolve_properties`
/// over a node's raw map -- `Scene::apply_one_surface` for a surface root, `resolve_and_reconcile`'s
/// child loop for every node below one -- and again at the top of `resolve_and_reconcile`, which is
/// what keeps [`children_of`]'s `unreachable!` arm unreachable for any caller.
///
/// The ordering is the whole reason this is a function rather than two lines inline. Resolution
/// calls back into Lua (ADR-0044 decision 1), so running it before these checks executes a rejected
/// node's `Signal` getters on its behalf: measured, a self-generating `children` signal's body ran
/// 64 times against a 64-level cap, because the parent at the last admitted level resolved the
/// child's whole property map -- running its `children` closure -- before recursing into the check
/// that refused that child. The kind case is worse than one wasted level: every property getter of
/// an unsupported-kind node ran, arbitrary Lua side effects for a node that never entered the
/// accepted tree.
///
/// `depth` is the level the node being checked would occupy, so a parent at `depth` checks its
/// children at `depth + 1` -- the same number the recursive call is handed, and the same
/// `TreeTooDeep` this used to raise one frame later.
fn ensure_node_admissible(kind: &str, depth: u32) -> Result<(), LayoutError> {
    ensure_supported_kind(kind)?;
    // build-steps.md Phase 19 item 3: a cyclic or infinitely-generating tree returns a LayoutError
    // at MAX_TREE_DEPTH's own stack depth, not somewhere further down (see MAX_TREE_DEPTH's doc
    // comment for the measured stack cost this leaves margin against).
    //
    // `>=`, not `>`: `depth` counts levels already entered, root at 0, so this admits levels
    // 0..MAX_TREE_DEPTH-1 -- exactly MAX_TREE_DEPTH of them, which is what the constant and the
    // error message both say. `>` admitted MAX_TREE_DEPTH + 1 levels while claiming
    // MAX_TREE_DEPTH, and disagreed with the signal cap's `>= MAX_SIGNAL_NESTING_DEPTH` about
    // what "maximum depth" means. The reported `depth` is 1-based (the level being refused) so
    // "maximum depth of 64 levels ... got at least 65" reads as the truth.
    if depth >= MAX_TREE_DEPTH {
        return Err(LayoutError::TreeTooDeep {
            kind: kind.to_string(),
            depth: depth + 1,
            max: MAX_TREE_DEPTH,
        });
    }
    Ok(())
}

/// Dispatches to the right raw property (`child` for the three surface roles, `children` for the
/// container kinds, none for leaves) -- only called after [`ensure_supported_kind`] already
/// validated `node.kind`, so the fallback arm is unreachable, not a silent default.
///
/// `window` and `popup` share `panel`'s arm because § 6.2 and § 6.3 give each of them exactly one
/// `child`, the same as § 6.1 does: a surface holds one root visual node, and the difference
/// between the three roles is which protocol assigns the surface its role, not what hangs under it.
///
/// `textfield` (`oblisk-idl-api-specs.md` § 5.2 item 8) is a leaf like `text`/`icon`: it never
/// takes `children`. Its own properties (`mask_character`, `secure_submit`, `on_change`,
/// `on_submit`) ride along unvalidated in `RetainedNode.properties`, same as `button`'s
/// `on_click` -- no GPU painting or `wp-text-input-v3` wiring reads them from the scene graph
/// yet (build-steps.md Phase 15 item 2 scopes this to a valid, parseable node kind; the protocol
/// state itself lives entirely outside the scene graph, on `App` in `renderer/src/wayland/mod.rs`
/// -- ADR-0009 named this a `TextInputService`, but this slice inlined the fields onto `App`
/// directly rather than extracting that type; see that file's own `bind_text_input` doc comment
/// for the upgrade path).
fn children_of(kind: &str, properties: &HashMap<String, Value>) -> Result<Vec<VirtualNode>, LayoutError> {
    match kind {
        "panel" | "window" | "popup" => Ok(node::parse_single_child(properties, "child")?
            .into_iter()
            .collect()),
        "rect" | "row" | "column" | "button" => node::parse_children(properties),
        // ADR-0045 decision 3, build-steps.md Phase 19 item 12: a `list`'s children don't exist
        // as a literal Lua table -- they're generated from `source`, one per element, which is
        // why this is its own parser rather than a `parse_children` variant.
        "list" => node::parse_list_children(properties),
        "text" | "icon" | "textfield" => Ok(Vec::new()),
        other => unreachable!("ensure_supported_kind already rejected `{other}`"),
    }
}

/// `Content` stays unresolved (`None`) until children are known; every other mode is known
/// upfront from `available` alone.
fn resolve_non_content(mode: SizeMode, available: f32) -> Option<f32> {
    match mode {
        SizeMode::Content => None,
        SizeMode::Pixels(n) => Some(n),
        SizeMode::Fill => Some(available),
        SizeMode::Percent(p) => Some(available * p),
    }
}

/// Whether `child_properties` requests a `Stretch` cross-alignment from `parent_kind`, and if so,
/// the forced size to hand `resolve_and_reconcile` for that axis -- `margined_budget`, the same
/// budget the child would get anyway, so stretching is "take the whole margined slot" rather than
/// a distinct sizing rule. Only fires when the parent's own size in that axis is already known
/// (`own_*_known` is `Some`): when the parent's axis is `Content`-sized, its final size isn't
/// determined until after this child resolves, so there's nothing to force yet -- `position_children`'s
/// post-hoc patch is the fallback for that remaining case (docs/adr/0023).
fn stretch_forced_size(
    parent_kind: &str,
    child_properties: &HashMap<String, Value>,
    own_width_known: Option<f32>,
    own_height_known: Option<f32>,
    margined_budget: LogicalSize,
) -> Result<(Option<f32>, Option<f32>), LayoutError> {
    match parent_kind {
        "row" => {
            let cross = node::parse_align(child_properties, "align_v")?;
            let forced_h = (cross == Align::Stretch && own_height_known.is_some()).then_some(margined_budget.height);
            Ok((None, forced_h))
        }
        "column" => {
            let cross = node::parse_align(child_properties, "align_h")?;
            let forced_w = (cross == Align::Stretch && own_width_known.is_some()).then_some(margined_budget.width);
            Ok((forced_w, None))
        }
        // The stacking kinds, all three surface roles included: a child aligned `Stretch` on an
        // axis takes the whole margined slot in it (docs/adr/0023 item 4). A `window`'s or a
        // `popup`'s root is a surface container like a `panel`'s, so a full-bleed background inside
        // one has to work the same way.
        "rect" | "button" | "panel" | "window" | "popup" => {
            let align_h = node::parse_align(child_properties, "align_h")?;
            let align_v = node::parse_align(child_properties, "align_v")?;
            let forced_w = (align_h == Align::Stretch && own_width_known.is_some()).then_some(margined_budget.width);
            let forced_h = (align_v == Align::Stretch && own_height_known.is_some()).then_some(margined_budget.height);
            Ok((forced_w, forced_h))
        }
        _ => Ok((None, None)),
    }
}

/// Pairs `fresh_children` against `old_children` (`docs/oblisk-layout-engine-geometry.md` § 4,
/// docs/adr/0045 decisions 1-2) by partitioning both sides into identified and unidentified
/// subsequences, because an `id` means "this is the same node, and *only* the same node", in both
/// directions:
///
/// - a fresh child **with** an `id` matches only a retained child carrying the same `id`. If none
///   exists it is new, and it must not fall through to the positional pool -- otherwise a node
///   declared brand new would inherit the NodeId, and therefore the entire retained subtree, of an
///   unrelated node that happened to be left over, which is exactly what a paint stage keying GPU
///   resources on NodeId cannot survive;
/// - a fresh child **without** an `id` matches only a retained child **without** one, positionally
///   among themselves. That is ADR-0023's original positional rule applied to a smaller set, and it
///   is what keeps adding or removing an `id` an honest change of identity rather than a silent
///   preservation of it;
/// - every retained child claimed by neither rule is retired child-first (`CONTEXT.md`, Lease),
///   whether it shrank out of an id-less list or its `id` disappeared from the fresh tree
///   (decision 2's last sentence). An identified child gets no special teardown, just the existing
///   lease path. Retirement depends on whether the child was claimed, never on whether the fresh
///   list happened to run short.
///
/// Returns one `Option<RetainedNode>` per `fresh_children` entry, in the same order, for the
/// caller's own kind-match check: a same-id pair whose kind changed is still not reusable, same
/// as a positionally-matched pair whose kind changed always was.
///
/// Linear in this parent's child count: each side's ids are parsed exactly once, and the id match
/// goes through a `HashMap<&str, usize>` built once per parent rather than a scan per fresh child.
/// That matters because nothing bounds sibling count -- a config can emit
/// `for i = 1, 10000 do c[i] = rect { id = "n" .. i } end` -- and this runs on the Wayland dispatch
/// thread at capability-push cadence (ADR-0044 decision 2's dirty flag), not once per config edit.
/// Every temporary below is sized to this parent's own children and dropped on return, so peak
/// allocation stays proportional to the widest single parent rather than to the whole tree.
fn pair_children_by_id_then_position(
    scene: &mut Scene,
    fresh_children: &[VirtualNode],
    old_children: Vec<RetainedNode>,
) -> Result<Vec<Option<RetainedNode>>, LayoutError> {
    let fresh_ids: Vec<Option<String>> = fresh_children
        .iter()
        .map(|c| node::parse_node_id(&c.properties))
        .collect::<Result<_, _>>()?;

    // Decision 1: a duplicate id among siblings is a LayoutError, not silent last-one-wins.
    // Checked before touching `old_children` at all, so a bad config never partially retires
    // anything before this fails.
    let mut seen: HashSet<&str> = HashSet::with_capacity(fresh_ids.len());
    for id in fresh_ids.iter().flatten() {
        if !seen.insert(id.as_str()) {
            return Err(LayoutError::InvalidProperty {
                property: "id".to_string(),
                detail: format!("duplicate id `{id}` among siblings"),
            });
        }
    }

    // Each retained child's id, parsed once per parent rather than once per comparison. A retained
    // child's id can't hold a Signal or duplicate a sibling's -- both were rejected in the pass
    // that first made this node retained -- so `.ok().flatten()` collapsing "no id" and "already
    // validated" together is safe here.
    let old_ids: Vec<Option<String>> = old_children
        .iter()
        .map(|c| node::parse_node_id(&c.properties).ok().flatten())
        .collect();
    let mut old_slots: Vec<Option<RetainedNode>> = old_children.into_iter().map(Some).collect();

    let mut retained_by_id: HashMap<&str, usize> = HashMap::with_capacity(old_ids.len());
    for (index, id) in old_ids.iter().enumerate() {
        if let Some(id) = id {
            retained_by_id.insert(id.as_str(), index);
        }
    }

    // Step 1: the identified subsequence. A miss stays `None` and is *not* refilled below, which
    // is the whole point -- an id that matched nothing is a new node, not an unclaimed slot.
    let mut matched: Vec<Option<RetainedNode>> = Vec::with_capacity(fresh_children.len());
    for fresh_id in &fresh_ids {
        let claimed = fresh_id
            .as_deref()
            .and_then(|id| retained_by_id.get(id).copied())
            .and_then(|index| old_slots[index].take());
        matched.push(claimed);
    }

    // Step 2: the unidentified subsequence on both sides, zipped in order -- ADR-0023's positional
    // rule, restricted to the children that never claimed an identity. Identified retained
    // children are excluded by construction, so a leftover identified node can never be handed to
    // an anonymous fresh child.
    let mut unidentified_old = old_ids.iter().enumerate().filter(|(_, id)| id.is_none()).map(|(index, _)| index);
    for (slot, fresh_id) in matched.iter_mut().zip(&fresh_ids) {
        if fresh_id.is_none()
            && let Some(index) = unidentified_old.next()
        {
            *slot = old_slots[index].take();
        }
    }

    // Whatever neither rule claimed: id-less shrinkage, or a retained child whose id vanished from
    // the fresh tree. Both retire through the same child-first path, in the order they sat in.
    for slot in &mut old_slots {
        if let Some(leftover) = slot.take() {
            scene.retire_child_first(leftover);
        }
    }

    Ok(matched)
}

/// The single recursive walk: constraint pass on entry, size pass from the recursive children's
/// results, position pass once this node's own size is known. Reconciles as it goes: `retained`
/// is `Some` only when the caller already matched `fresh`'s kind at this position (§ 4).
///
/// `forced_width`/`forced_height` let the caller override this node's own size in an axis instead
/// of resolving it from `width`/`height` -- how a `Stretch` cross-alignment (§ 3.3) is applied
/// when the parent's own size in that axis is already known: the parent computes the forced size
/// *before* recursing here, so this node's own children get positioned against the final,
/// stretched size in the same pass instead of being patched afterward (which would leave a
/// stretched node's own descendants positioned against its pre-stretch size). `None` (the normal
/// case, and always the case when the parent's own axis is itself `Content`-sized, since its
/// final size isn't known until after its children resolve) falls back to the usual
/// `width`/`height`-mode resolution.
///
/// `properties` arrives already resolved -- `node::resolve_properties` has run over this node's
/// raw map exactly once for this pass, replacing every `Signal` with its current value
/// (build-steps.md Phase 19 item 5). The caller does that rather than this function, because a
/// parent has to read a child's `margin` to compute the budget it recurses with, so the one read
/// has to happen in the parent's loop; `Scene::apply_one_surface` does it for a surface root,
/// which has no parent. Everything from here down -- this node's own parsing,
/// `intrinsic_content_size`, `position_children`, and the `RetainedNode` this returns -- reads that
/// one map, so a `Signal` behind a property is read exactly once per node per pass and the sizing
/// and positioning passes cannot disagree about *it*. They can still disagree about a property
/// whose resolved value is a plain table with an `__index` metamethod, which every `table.get`
/// re-runs: see `node::parse_edge_insets`'s `ponytail:`, since `margin` is the property both passes
/// read and the one that shape breaks.
// Ten parameters, but each is load-bearing for this single recursive pass (§ 3's "single...
// pass", see the module doc comment); splitting them into a struct would just be a bag carrying
// the same ten fields through the same one caller.
#[allow(clippy::too_many_arguments)]
fn resolve_and_reconcile(
    scene: &mut Scene,
    retained: Option<RetainedNode>,
    kind: &str,
    properties: HashMap<String, Value>,
    available: LogicalSize,
    shaping: &ShapingHandle,
    forced_width: Option<f32>,
    forced_height: Option<f32>,
    lua: &Lua,
    depth: u32,
) -> Result<RetainedNode, LayoutError> {
    // Already run by whoever resolved `properties` (that is the ordering `ensure_node_admissible`
    // exists to enforce), and repeated here so this function holds its own preconditions rather
    // than trusting a call site -- notably `children_of`'s `unreachable!` arm below.
    ensure_node_admissible(kind, depth)?;

    let id = retained.as_ref().map(|r| r.id);
    let old_children = retained.map(|r| r.children).unwrap_or_default();
    let id = id.unwrap_or_else(|| scene.alloc_id());

    let padding = node::parse_edge_insets(&properties, "padding")?;
    let visible = node::parse_visible(&properties)?;
    let width_mode = node::parse_size_mode(&properties, "width")?;
    let height_mode = node::parse_size_mode(&properties, "height")?;

    let own_width_known = forced_width.or_else(|| resolve_non_content(width_mode, available.width));
    let own_height_known = forced_height.or_else(|| resolve_non_content(height_mode, available.height));

    // The wrap boundary a `text` child measures against (§ 3.2: "wraps text bounds when
    // exceeding available width limits"): its own explicit/forced width when known, otherwise
    // whatever room its parent handed down. Unused by every non-text kind.
    let text_wrap_width = own_width_known.unwrap_or_else(|| (available.width - padding.left - padding.right).max(0.0));

    // A `Content`-sized axis has no real budget to hand children yet (this node's own size in
    // that axis isn't known until *after* they resolve) -- a `Fill`/`Percent` child in that axis
    // resolves to 0.0 rather than something derived from the outer `available`.
    //
    // ponytail: one recursive pass, not a full two-pass constraint solver (docs/adr/0023 item
    // 10). Correct for the Content-sized-parent case; wrong only for a child that specifically
    // wants to fill a Content-sized ancestor in that same axis, which no fixture here needs. A
    // second pass over just that axis is the upgrade path if this ever matters.
    let child_budget = LogicalSize {
        width: own_width_known.map_or(0.0, |w| (w - padding.left - padding.right).max(0.0)),
        height: own_height_known.map_or(0.0, |h| (h - padding.top - padding.bottom).max(0.0)),
    };

    let fresh_children = children_of(kind, &properties)?;
    let matched_candidates = pair_children_by_id_then_position(scene, &fresh_children, old_children)?;

    let mut new_children = Vec::with_capacity(fresh_children.len());
    for (fresh_child, candidate) in fresh_children.iter().zip(matched_candidates) {
        // Before this child's own getters run, not after: resolving its property map calls back
        // into Lua, and a child the walk is about to refuse must not get to execute anything on the
        // way to being refused. `depth + 1` is the level this child would occupy, so the error is
        // the same variant, kind and level the recursive call raised when this check lived one
        // frame further down -- see `ensure_node_admissible`.
        ensure_node_admissible(&fresh_child.kind, depth + 1)?;

        let reusable = match candidate {
            Some(c) if c.kind == fresh_child.kind => Some(c),
            Some(stale) => {
                scene.retire_child_first(stale);
                None
            }
            None => None,
        };

        // This child's one resolve for this pass (build-steps.md Phase 19 item 5). It happens here
        // rather than inside the recursive call because the budget that call receives depends on
        // this child's own `margin`, and a second read of an impure signal would hand it a budget
        // computed from a value nothing downstream would ever see again.
        let child_properties = node::resolve_properties(&fresh_child.properties, &fresh_child.kind, lua)?;

        // § 3.1: a child's own margin comes out of the same available-space budget its parent's
        // padding already inset -- subtracted here, per child, since each child can carry a
        // different margin.
        let child_margin = node::parse_edge_insets(&child_properties, "margin")?;
        let margined_budget = LogicalSize {
            width: (child_budget.width - child_margin.left - child_margin.right).max(0.0),
            height: (child_budget.height - child_margin.top - child_margin.bottom).max(0.0),
        };
        let (child_forced_width, child_forced_height) =
            stretch_forced_size(kind, &child_properties, own_width_known, own_height_known, margined_budget)?;

        new_children.push(resolve_and_reconcile(
            scene,
            reusable,
            &fresh_child.kind,
            child_properties,
            margined_budget,
            shaping,
            child_forced_width,
            child_forced_height,
            lua,
            depth + 1,
        )?);
    }

    // Padding is added here rather than inside `intrinsic_content_size`, and only on an axis whose
    // size the config did not state. `intrinsic_content_size` answers "how much room do the
    // contents need", which is the number the row/column sums and the stacking union are about; a
    // node's own padding is a property of the node, not of what it contains. On a stated axis
    // padding has already done its work by insetting `child_budget` above, so adding it here too
    // would count it twice. On a `Content` axis nothing has accounted for it yet, and
    // `position_children` below offsets every child by exactly this much inside `size`: without
    // this the children are placed past the edge of the box meant to contain them (measured live
    // against dev-config, where a padded column reported its child's bare height at both 8px and
    // 50px of padding -- build-steps.md Phase 19 item 14).
    let intrinsic = intrinsic_content_size(kind, &properties, &new_children, text_wrap_width, shaping)?;
    let own_width = own_width_known.unwrap_or(intrinsic.width + padding.horizontal());
    let own_height = own_height_known.unwrap_or(intrinsic.height + padding.vertical());
    let size = LogicalSize {
        width: own_width,
        height: own_height,
    };

    position_children(kind, &properties, &mut new_children, size, padding, own_width_known, own_height_known)?;

    Ok(RetainedNode {
        id,
        kind: kind.to_string(),
        rect: LogicalRect {
            x: 0.0,
            y: 0.0,
            width: size.width,
            height: size.height,
        },
        visible,
        properties,
        children: new_children,
    })
}

/// § 3.2's bottom-up size resolution, per kind. An invisible child contributes nothing here (see
/// `resolve_and_reconcile`'s doc comment on the collapse-vs-reserve-space choice) -- filtered out
/// before the row/column sum and the stacking union.
fn intrinsic_content_size(
    kind: &str,
    properties: &HashMap<String, Value>,
    children: &[RetainedNode],
    text_wrap_width: f32,
    shaping: &ShapingHandle,
) -> Result<LogicalSize, LayoutError> {
    match kind {
        "text" => {
            let content = node::parse_content(properties)?;
            let font_size = node::parse_font_size(properties)?;
            // A wrap width of 0 means nothing is known yet (a Content-sized text inside a
            // Content-sized ancestor with no room resolved so far) -- treat that as unconstrained
            // rather than forcing every word onto its own line.
            let max_width = (text_wrap_width > 0.0).then_some(text_wrap_width);
            // ponytail: this reshapes every text node on every `Scene::apply`, and there is no
            // cache -- `ShapingHandle::shape` blocks on a cross-thread round trip to the shaping
            // worker for each call (see `crate::text::shaping`). That was once per config edit; it
            // is now up to once per poll turn (ADR-0044 decision 2's dirty flag), so a bar with 20
            // text nodes on a 15ms poll driven by a high-frequency capability is roughly 1300
            // blocking round trips per second on the Wayland dispatch thread, which is also the
            // thread that answers `configure` and runs the VM (docs/adr/0039). The remaining fix
            // is a shape cache keyed on (text, font_size, max_width); not built here.
            let shaped = shaping.shape(ShapeRequest {
                text: content,
                font_size,
                line_height: font_size * 1.2,
                max_width,
            });
            Ok(LogicalSize {
                width: shaped.width,
                height: shaped.height,
            })
        }
        "icon" => {
            let size = node::parse_icon_size(properties)?;
            Ok(LogicalSize {
                width: size,
                height: size,
            })
        }
        "rect" if children.is_empty() => Ok(LogicalSize::default()),
        "row" => {
            let spacing = node::parse_spacing(properties)?;
            let visible: Vec<&RetainedNode> = children.iter().filter(|c| c.visible).collect();
            // § 3.1: a child's margin travels with it -- its footprint on the row's main axis is
            // its own width plus its margin, matching `position_children`'s identical footprint
            // math (the two must agree, or a child would be sized to fit but then overlap or
            // leave a gap once positioned).
            let width = visible
                .iter()
                .map(|c| Ok(c.rect.width + node::parse_edge_insets(&c.properties, "margin")?.horizontal()))
                .collect::<Result<Vec<f32>, LayoutError>>()?
                .into_iter()
                .sum::<f32>()
                + spacing * visible.len().saturating_sub(1) as f32;
            let height = visible
                .iter()
                .map(|c| Ok(c.rect.height + node::parse_edge_insets(&c.properties, "margin")?.vertical()))
                .collect::<Result<Vec<f32>, LayoutError>>()?
                .into_iter()
                .fold(0.0_f32, f32::max);
            Ok(LogicalSize { width, height })
        }
        // `list` sizes exactly like `column` -- ADR-0045 decision 3 and § 5.2 item 7 say nothing
        // about a list's geometry, only its reconciliation, so this is this slice's own
        // interpretation, the same way ADR-0023 item 4 labels the stacking model as one. Sharing
        // `column`'s arm rather than duplicating its body is deliberate: the two are the same
        // vertical-stack formula, and a future divergence should be a real design decision, not
        // drift between two copies nobody remembers to keep in sync.
        //
        // ponytail: no horizontal list. A repeated tray (icons flowing left to right) can't be
        // expressed today, and neither upgrade path is built because both cost more than this
        // slice needs. A `direction` property on `list` would invent API § 5.2 doesn't have --
        // `list`'s own spec entry lists only `source`/`itemfn`/`key`. Splicing a list's generated
        // children into its *parent's* child list -- a true repeater, which would inherit
        // whichever direction that parent already lays out in -- conflicts with ADR-0045's "a
        // node is identified by its position in one parent's child list": a spliced list has no
        // single parent's child list to hold a position in, so that ADR would need amending
        // first, not just this code.
        "column" | "list" => {
            let spacing = node::parse_spacing(properties)?;
            let visible: Vec<&RetainedNode> = children.iter().filter(|c| c.visible).collect();
            let height = visible
                .iter()
                .map(|c| Ok(c.rect.height + node::parse_edge_insets(&c.properties, "margin")?.vertical()))
                .collect::<Result<Vec<f32>, LayoutError>>()?
                .into_iter()
                .sum::<f32>()
                + spacing * visible.len().saturating_sub(1) as f32;
            let width = visible
                .iter()
                .map(|c| Ok(c.rect.width + node::parse_edge_insets(&c.properties, "margin")?.horizontal()))
                .collect::<Result<Vec<f32>, LayoutError>>()?
                .into_iter()
                .fold(0.0_f32, f32::max);
            Ok(LogicalSize { width, height })
        }
        // Stacking model (rect-with-children, button, and the three surface roles): § 3.2 gives no
        // formula for a container that isn't row/column -- docs/adr/0023 item 4 documents this as
        // this phase's own interpretation. Content size is the bounding union over
        // independently-positioned children, each inflated by its own margin. `window` and `popup`
        // reach it through the same catch-all `panel` does, which is right for both: neither role
        // has a layout model of its own, and both are normally sized explicitly anyway (a `popup`
        // must be, § 6.3).
        _ => {
            let visible: Vec<&RetainedNode> = children.iter().filter(|c| c.visible).collect();
            let width = visible
                .iter()
                .map(|c| Ok(c.rect.width + node::parse_edge_insets(&c.properties, "margin")?.horizontal()))
                .collect::<Result<Vec<f32>, LayoutError>>()?
                .into_iter()
                .fold(0.0_f32, f32::max);
            let height = visible
                .iter()
                .map(|c| Ok(c.rect.height + node::parse_edge_insets(&c.properties, "margin")?.vertical()))
                .collect::<Result<Vec<f32>, LayoutError>>()?
                .into_iter()
                .fold(0.0_f32, f32::max);
            Ok(LogicalSize { width, height })
        }
    }
}

fn cross_axis_offset(align: Align, container: f32, child: f32) -> f32 {
    match align {
        Align::Start | Align::Stretch => 0.0,
        Align::Center => ((container - child) / 2.0).max(0.0),
        Align::End => (container - child).max(0.0),
    }
}

/// § 3.3's top-down position/stretch pass, per kind. `own_width_known`/`own_height_known` are the
/// same values `resolve_and_reconcile` already resolved for this node: `Some` means a `Stretch`
/// child in that axis was already forced to its final size before it resolved (see
/// `stretch_forced_size`), so patching it again here would be redundant (harmless were the values
/// still equal, but wrong once margin is subtracted below -- so it's skipped outright); `None`
/// means this axis was `Content`-sized, so no forcing was possible and the post-hoc patch here is
/// the only place a `Stretch` child in that axis gets its final size (its own descendants, if any,
/// won't be repositioned for that new size -- the same Content-sized-parent limitation as
/// docs/adr/0023 item 10, not the general bug that forcing above fixes).
fn position_children(
    kind: &str,
    properties: &HashMap<String, Value>,
    children: &mut [RetainedNode],
    size: LogicalSize,
    padding: EdgeInsets,
    own_width_known: Option<f32>,
    own_height_known: Option<f32>,
) -> Result<(), LayoutError> {
    let content_x = padding.left;
    let content_y = padding.top;
    let content_width = (size.width - padding.left - padding.right).max(0.0);
    let content_height = (size.height - padding.top - padding.bottom).max(0.0);

    let margins: Vec<EdgeInsets> = children
        .iter()
        .map(|c| node::parse_edge_insets(&c.properties, "margin"))
        .collect::<Result<_, _>>()?;

    match kind {
        "row" => {
            let spacing = node::parse_spacing(properties)?;
            let main_align = node::parse_align(properties, "align_h")?;
            let visible_indices: Vec<usize> = children
                .iter()
                .enumerate()
                .filter(|(_, c)| c.visible)
                .map(|(i, _)| i)
                .collect();
            // § 3.1: a child's margin travels with it on the main axis, so a margined child pushes
            // its neighbors apart instead of overlapping them.
            let footprints: Vec<f32> = (0..children.len())
                .map(|i| children[i].rect.width + margins[i].left + margins[i].right)
                .collect();
            let total_main = visible_indices.iter().map(|&i| footprints[i]).sum::<f32>()
                + spacing * visible_indices.len().saturating_sub(1) as f32;
            let spare = (content_width - total_main).max(0.0);
            let mut cursor = match main_align {
                Align::Start | Align::Stretch => 0.0,
                Align::Center => spare / 2.0,
                Align::End => spare,
            };
            for &i in &visible_indices {
                let cross_align = node::parse_align(&children[i].properties, "align_v")?;
                let slot_h = (content_height - margins[i].top - margins[i].bottom).max(0.0);
                let child_h = children[i].rect.height;
                let y = cross_axis_offset(cross_align, slot_h, child_h);
                children[i].rect.x = content_x + cursor + margins[i].left;
                children[i].rect.y = content_y + y + margins[i].top;
                if cross_align == Align::Stretch && own_height_known.is_none() {
                    children[i].rect.height = slot_h;
                }
                cursor += footprints[i] + spacing;
            }
        }
        // `list` positions exactly like `column` -- see `intrinsic_content_size`'s matching arm
        // for why this is a labeled interpretation rather than a spec requirement, and for the
        // no-horizontal-list ponytail.
        "column" | "list" => {
            let spacing = node::parse_spacing(properties)?;
            let main_align = node::parse_align(properties, "align_v")?;
            let visible_indices: Vec<usize> = children
                .iter()
                .enumerate()
                .filter(|(_, c)| c.visible)
                .map(|(i, _)| i)
                .collect();
            let footprints: Vec<f32> = (0..children.len())
                .map(|i| children[i].rect.height + margins[i].top + margins[i].bottom)
                .collect();
            let total_main = visible_indices.iter().map(|&i| footprints[i]).sum::<f32>()
                + spacing * visible_indices.len().saturating_sub(1) as f32;
            let spare = (content_height - total_main).max(0.0);
            let mut cursor = match main_align {
                Align::Start | Align::Stretch => 0.0,
                Align::Center => spare / 2.0,
                Align::End => spare,
            };
            for &i in &visible_indices {
                let cross_align = node::parse_align(&children[i].properties, "align_h")?;
                let slot_w = (content_width - margins[i].left - margins[i].right).max(0.0);
                let child_w = children[i].rect.width;
                let x = cross_axis_offset(cross_align, slot_w, child_w);
                children[i].rect.x = content_x + x + margins[i].left;
                children[i].rect.y = content_y + cursor + margins[i].top;
                if cross_align == Align::Stretch && own_width_known.is_none() {
                    children[i].rect.width = slot_w;
                }
                cursor += footprints[i] + spacing;
            }
        }
        // Stacking model: each child independently aligned within the full content box on both
        // axes, no spare-space distribution across siblings -- they can overlap. See docs/adr/0023
        // item 4. Reached by `rect`/`button` and by all three surface roles, matching
        // `intrinsic_content_size`'s catch-all and `stretch_forced_size`'s named arm.
        _ => {
            for (i, child) in children.iter_mut().enumerate() {
                if !child.visible {
                    continue;
                }
                let align_h = node::parse_align(&child.properties, "align_h")?;
                let align_v = node::parse_align(&child.properties, "align_v")?;
                let slot_w = (content_width - margins[i].left - margins[i].right).max(0.0);
                let slot_h = (content_height - margins[i].top - margins[i].bottom).max(0.0);
                let x = cross_axis_offset(align_h, slot_w, child.rect.width);
                let y = cross_axis_offset(align_v, slot_h, child.rect.height);
                child.rect.x = content_x + x + margins[i].left;
                child.rect.y = content_y + y + margins[i].top;
                if align_h == Align::Stretch && own_width_known.is_none() {
                    child.rect.width = slot_w;
                }
                if align_v == Align::Stretch && own_height_known.is_none() {
                    child.rect.height = slot_h;
                }
            }
        }
    }
    Ok(())
}

/// § 5.1's input-region scan: `surface_root`'s direct, visible children, projected to physical
/// pixels. Pure, and the `wl_region`/`wl_surface::set_input_region` push it feeds lives in
/// `crate::wayland::App::apply_input_region`, which is the only place a Wayland object exists to
/// push it to.
///
/// Per surface since docs/adr/0038 decision 5, which is why the name is the one thing here that
/// still says "overlay": § 5.1's bounding-box union was written for a single fullscreen
/// `overlay_canvas` and was not deleted with it, it was generalized. It applies to any surface
/// whose visible content is smaller than the surface itself -- load-bearing for a fullscreen
/// transparent panel, an empty region (so clicks pass straight through) for one with nothing
/// visible in it, and a no-op for a tightly-sized bar whose child fills it.
///
/// ponytail: direct children only, not a recursive union over the whole visible subtree. A panel
/// whose child is a full-surface transparent container holding one small button therefore claims
/// the container's whole box for input, not the button's. That is § 5.1's own wording ("the union
/// of its visible children's absolute bounding boxes") and it is exactly right for the shape the
/// spec has in mind, where each direct child *is* one floating panel. Upgrade path: recurse into a
/// child whose own `background` is absent or fully transparent, once a config writes that shape
/// and the extra walk earns itself.
pub fn overlay_input_regions(surface_root: &ResolvedNode, scale: f32) -> Vec<PhysicalRect> {
    surface_root
        .children
        .iter()
        .filter(|child| child.visible)
        .map(|child| snap_to_physical(child.rect, scale))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::nodes::{deserialize_lua_table, register_node_constructors};

    /// Returns the `Lua` alongside the parsed node: an `mlua::Value` (every string/table
    /// property) is tied to the state that created it and panics on use once that state drops,
    /// so callers must keep the returned `Lua` alive for as long as the `VirtualNode` (and
    /// anything resolved from it) is used.
    fn surface_from(lua_src: &str) -> (mlua::Lua, VirtualNode) {
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        let table: mlua::Table = lua.load(lua_src).eval().unwrap();
        let node = deserialize_lua_table(&table).unwrap();
        (lua, node)
    }

    fn full() -> LogicalSize {
        LogicalSize {
            width: 1000.0,
            height: 500.0,
        }
    }

    /// The single-output shorthand every fixture below uses: one instance per declared surface,
    /// against one output named `"TEST"`, so a fixture declaring `id = "bar"` reads back as
    /// `scene.surface("bar@TEST")`. Deliberately not `layout::instance::expand_instances` -- that
    /// function takes `SurfaceSpec`s, whose `panel` arm requires a `layer`, and these fixtures
    /// test layout rather than topology; `expand_instances` has its own direct tests in
    /// `layout::instance`.
    fn apply_at(
        scene: &mut Scene,
        surfaces: &[VirtualNode],
        available: LogicalSize,
        shaping: &ShapingHandle,
        lua: &Lua,
    ) -> Result<(), LayoutError> {
        let instances: Vec<SurfaceInstance> = surfaces
            .iter()
            .map(|surface| {
                let declared_id = node::parse_surface_id(&surface.properties).expect("every fixture declares an `id`");
                SurfaceInstance {
                    instance_id: format!("{declared_id}@TEST"),
                    declared_id,
                    output: "TEST".to_string(),
                    available,
                }
            })
            .collect();
        scene.apply(surfaces, &instances, shaping, lua)
    }

    #[test]
    fn a_signal_valued_width_resolves_to_its_current_value_in_the_resolved_node() {
        // The Scene::apply seam (ADR-0044 decision 1): a Signal in a geometry slot must reach
        // the ResolvedNode's rect, not error the whole apply.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Integer(40), crate::lua::signal::DirtyFlag::new()).0;
        lua.globals().set("w", signal).unwrap();
        let table: mlua::Table = lua
            .load(r#"return panel { id = "bar", child = rect { width = w, height = 20 } }"#)
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        let child = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(child.rect.width, 40.0, "a Signal-valued width must resolve at layout time");
    }

    #[test]
    fn a_state_signal_in_a_property_resolves_at_layout_time_and_a_set_between_applies_moves_it() {
        // ADR-0044 decision 5's end of the seam: a `state` signal is a `Signal` like any other, so
        // `resolve_properties` reads it with no new arm, and what a handler's `:set()` wrote is
        // what the next apply lays out.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(r#"return panel { id = "bar", child = rect { width = state("w", 40), height = 20 } }"#)
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        assert_eq!(scene.surface("bar@TEST").unwrap().children[0].rect.width, 40.0);

        lua.load(r#"state("w", 0):set(90)"#).exec().unwrap();
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        assert_eq!(
            scene.surface("bar@TEST").unwrap().children[0].rect.width,
            90.0,
            "a re-resolve after :set() must lay out the written value, not the initial one"
        );
    }

    #[test]
    fn a_pixels_sized_rect_resolves_to_its_explicit_size() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) =
            surface_from(r#"panel { id = "bar", child = rect { width = 40, height = 20 } }"#);
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let root = scene.surface("bar@TEST").unwrap();
        let child = &root.children[0];
        assert_eq!(child.rect.width, 40.0);
        assert_eq!(child.rect.height, 20.0);
    }

    #[test]
    fn a_childless_rect_with_no_explicit_size_resolves_to_zero() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(r#"panel { id = "bar", child = rect {} }"#);
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let child = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(child.rect.width, 0.0);
        assert_eq!(child.rect.height, 0.0);
    }

    #[test]
    fn a_fill_child_takes_its_parents_available_bounds() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", width = 1000, height = 500, child = rect { width = "Fill", height = "Fill" } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let child = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(child.rect.width, 1000.0);
        assert_eq!(child.rect.height, 500.0);
    }

    #[test]
    fn a_percent_child_scales_against_its_parents_available_bounds() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", width = 1000, height = 500, child = rect { width = "50%" } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let child = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(child.rect.width, 500.0);
    }

    #[test]
    fn row_intrinsic_width_sums_children_plus_spacing_gaps() {
        // Hand-computed: two 10-wide children + 1 gap of 5 = 25, independent of the resolve code.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { spacing = 5, children = { rect { width = 10, height = 8 }, rect { width = 10, height = 4 } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.rect.width, 25.0, "10 + 10 + 5 spacing");
        assert_eq!(row.rect.height, 8.0, "max of children's heights");
    }

    #[test]
    fn column_intrinsic_height_sums_children_plus_spacing_gaps() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = column { spacing = 3, children = { rect { width = 6, height = 10 }, rect { width = 9, height = 10 } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let column = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(column.rect.height, 23.0, "10 + 10 + 3 spacing");
        assert_eq!(column.rect.width, 9.0, "max of children's widths");
    }

    #[test]
    fn a_childs_own_margin_pushes_it_inward_and_widens_the_rows_footprint() {
        // Hand-computed: child at x=0 has margin.left=4 -> lands at x=4. The row's own intrinsic
        // width is the child's 10 plus its margin.left+right (4+4) = 18, independent of the
        // resolve code.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { children = { rect { width = 10, height = 10, margin = { left = 4, right = 4 } } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.rect.width, 18.0, "10 + 4 + 4 margin");
        assert_eq!(row.children[0].rect.x, 4.0, "the child's own margin.left offsets it inward");
    }

    #[test]
    fn a_margined_row_child_pushes_its_sibling_apart_instead_of_overlapping() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 10, height = 10, margin = { right = 5 } },
                rect { width = 10, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(
            row.children[1].rect.x, 15.0,
            "10 (first child) + 5 (its margin.right) = 15, not overlapping at 10"
        );
    }

    #[test]
    fn stretching_a_child_that_itself_has_children_repositions_its_descendants_too() {
        // Regression test for the CONFIRMED correctness finding: a Stretch child that grows must
        // reposition its own children against the new, larger size in the same pass -- not leave
        // them positioned against the pre-stretch size.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", width = 100, height = 50, child = row { height = "Fill", children = {
                rect { width = 20, align_v = "Stretch", children = {
                    rect { width = 6, height = 6, align_h = "Center", align_v = "Center" },
                } },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        let stretched = &row.children[0];
        assert_eq!(stretched.rect.height, 50.0, "stretched to the row's full height");
        let inner = &stretched.children[0];
        assert_eq!(
            inner.rect.y,
            (50.0 - 6.0) / 2.0,
            "centered against the stretched (post-fix) height, not the pre-stretch intrinsic height"
        );
    }

    #[test]
    fn row_start_alignment_packs_children_at_the_beginning() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { children = { rect { width = 10, height = 10 }, rect { width = 10, height = 10 } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.x, 0.0);
        assert_eq!(row.children[1].rect.x, 10.0);
    }

    #[test]
    fn row_end_alignment_packs_children_against_the_far_edge() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", width = 100, height = 20, child = row { width = "Fill", align_h = "End", children = { rect { width = 10, height = 10 } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.x, 90.0);
    }

    #[test]
    fn row_child_stretch_alignment_fills_the_cross_axis() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", width = 100, height = 50, child = row { height = "Fill", children = { rect { width = 10, height = 5, align_v = "Stretch" } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.height, 50.0);
    }

    #[test]
    fn stacking_container_aligns_each_child_independently_and_they_can_overlap() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", width = 100, height = 100, child = rect { width = "Fill", height = "Fill", children = {
                rect { width = 20, height = 20, align_h = "Start", align_v = "Start" },
                rect { width = 20, height = 20, align_h = "End", align_v = "End" },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let outer = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(outer.children[0].rect.x, 0.0);
        assert_eq!(outer.children[1].rect.x, 80.0);
        assert_eq!(outer.children[1].rect.y, 80.0);
    }

    #[test]
    fn an_invisible_child_does_not_consume_row_space() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 10, height = 10, visible = false },
                rect { width = 10, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(
            row.rect.width, 10.0,
            "the invisible child must not widen the row or add a spacing gap"
        );
        assert_eq!(
            row.children[1].rect.x, 0.0,
            "the visible child packs at the start as if the hidden one weren't there"
        );
    }

    /// Found live against `dev-config`: a content-sized `column` with 8px of padding reported the
    /// bare height of its one child, and reported the same height with 50px of padding. Padding
    /// insets the box children are laid out in (`position_children` offsets them by exactly this
    /// much), so a content-sized container that does not also grow by it positions its children
    /// past its own edge.
    #[test]
    fn a_content_sized_container_grows_by_its_own_padding() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = column {
                padding = { top = 8, right = 10, bottom = 8, left = 10 },
                children = { rect { width = 20, height = 20 } },
            } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let column = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(column.rect.width, 40.0, "20 wide child plus 10 of padding on each side");
        assert_eq!(column.rect.height, 36.0, "20 tall child plus 8 of padding top and bottom");
        // The child sits at the padding offset, which is the half that already worked, and is
        // what makes the sizes above the ones that keep it inside the box.
        assert_eq!(column.children[0].rect.x, 10.0);
        assert_eq!(column.children[0].rect.y, 8.0);
    }

    /// The surface root resolves through the same `unwrap_or`, and a popup sized to its contents
    /// is the case this actually bites (build-steps.md Phase 19 item 14).
    #[test]
    fn padding_grows_a_content_sized_surface_too() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", padding = { top = 6, right = 6, bottom = 6, left = 6 },
                child = rect { width = 20, height = 20 } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let root = scene.surface("bar@TEST").unwrap();
        assert_eq!(root.rect.width, 32.0);
        assert_eq!(root.rect.height, 32.0);
    }

    /// Padding must not double-count on an axis whose size the config stated: there it correctly
    /// shrinks the child budget and leaves the parent alone.
    #[test]
    fn padding_does_not_grow_an_explicitly_sized_container() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = column {
                width = 100,
                padding = { top = 8, right = 10, bottom = 8, left = 10 },
                children = { rect { width = 20, height = 20 } },
            } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let column = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(column.rect.width, 100.0, "stated width wins, padding already inset the child");
        assert_eq!(column.rect.height, 36.0, "the Content axis still grows by its padding");
    }

    #[test]
    fn text_content_size_comes_from_a_real_shaping_round_trip() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) =
            surface_from(r#"panel { id = "bar", child = text { content = "Oblisk" } }"#);
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let text = &scene.surface("bar@TEST").unwrap().children[0];
        assert!(text.rect.width > 0.0);
        assert_eq!(
            text.rect.height,
            12.0 * 1.2,
            "default font_size 12 * the 1.2 line-height multiplier"
        );
    }

    #[test]
    fn an_unsupported_top_level_kind_is_rejected() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        // `list` used to be this fixture's example kind, back when it was registered as a Lua
        // constructor but rejected by `ensure_supported_kind` (build-steps.md Phase 19 item 12).
        // It is a real kind now, so a raw table naming a kind no constructor registers at all is
        // what "unsupported" actually means going forward.
        let (_lua, surface) = surface_from(r#"panel { id = "bar", child = { kind = "banana" } }"#);
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap_err();
        assert!(matches!(err, LayoutError::UnsupportedNodeKind(k) if k == "banana"));
    }

    #[test]
    fn an_unsupported_kind_nested_inside_children_is_rejected() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) =
            surface_from(r#"panel { id = "bar", child = row { children = { { kind = "banana" } } } }"#);
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap_err();
        assert!(matches!(err, LayoutError::UnsupportedNodeKind(k) if k == "banana"));
    }

    #[test]
    fn textfield_is_a_supported_leaf_kind_carrying_its_properties_unvalidated() {
        // build-steps.md Phase 15 item 2 / ADR-0027: `textfield` becomes a valid, parseable
        // scene-node kind here; no GPU painting or `wp-text-input-v3` wiring reads its
        // properties from the scene graph in this slice (see `children_of`'s doc comment).
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { children = { textfield { mask_character = "*", secure_submit = { capability = "polkit", action = "authenticate" } } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();

        let field = &scene.surface("bar@TEST").unwrap().children[0].children[0];
        assert_eq!(field.kind, "textfield");
        assert!(field.children.is_empty(), "textfield is a leaf, never a container");
        assert_eq!(
            field.properties.get("mask_character").unwrap().as_string().unwrap().to_string_lossy(),
            "*"
        );
    }

    #[test]
    fn one_declared_surface_resolves_one_tree_per_instance_each_against_its_own_size() {
        // The instance-keyed scene, directly (this type's doc comment, `CONTEXT.md`'s Surface
        // instance entry): a `"Fill"`-sized panel on two outputs of different sizes resolves to
        // two different widths, which one tree per declared surface cannot express.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(r#"panel { id = "bar", width = "Fill", height = "Fill" }"#);
        let instances = vec![
            SurfaceInstance {
                instance_id: "bar@eDP-1".to_string(),
                declared_id: "bar".to_string(),
                output: "eDP-1".to_string(),
                available: LogicalSize { width: 1920.0, height: 1080.0 },
            },
            SurfaceInstance {
                instance_id: "bar@DP-1".to_string(),
                declared_id: "bar".to_string(),
                output: "DP-1".to_string(),
                available: LogicalSize { width: 3840.0, height: 2160.0 },
            },
        ];

        scene.apply(&[surface], &instances, &shaping, &_lua).unwrap();

        assert_eq!(scene.surface("bar@eDP-1").unwrap().rect.width, 1920.0);
        assert_eq!(scene.surface("bar@DP-1").unwrap().rect.width, 3840.0);
        assert!(scene.surface("bar").is_none(), "the declared id alone is not a key any more");
    }

    #[test]
    fn a_declared_surface_with_no_instance_resolves_not_at_all() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(r#"panel { id = "bar", width = 10, height = 10 }"#);

        scene.apply(&[surface], &[], &shaping, &_lua).unwrap();

        assert!(scene.surface("bar@TEST").is_none(), "no instance means no output to resolve against, which is not an error");
    }

    #[test]
    fn an_instance_naming_a_surface_the_evaluation_did_not_declare_is_a_caller_bug() {
        // Instances and declarations come from the same evaluation, so this can only be a bug in
        // whatever expanded them -- distinct from the legal "declared but no instance" case above.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(r#"panel { id = "bar", width = 10, height = 10 }"#);
        let instances = vec![SurfaceInstance {
            instance_id: "ghost@TEST".to_string(),
            declared_id: "ghost".to_string(),
            output: "TEST".to_string(),
            available: full(),
        }];

        let err = scene.apply(&[surface], &instances, &shaping, &_lua).unwrap_err();

        assert!(err.to_string().contains("ghost@TEST"), "the message must name the offending instance: {err}");
        assert!(scene.surface("ghost@TEST").is_none());
    }

    #[test]
    fn a_failed_apply_leaves_the_scene_exactly_as_it_was() {
        // The rollback invariant this fix establishes: `Scene::apply` returning `Err` must not
        // observably mutate the `Scene` at all. Apply a good tree, capture its `NodeId`s, then
        // apply a tree whose property resolve fails partway (a Signal getter erroring, ADR-0044
        // decision 1) and assert everything -- surfaces, `NodeId`s, `retiring`, `next_id` -- is
        // unchanged. Checking only `surfaces` wouldn't catch a `next_id` bump or a stray
        // `retiring` entry left over from the aborted pass.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 10, height = 10 },
                rect { width = 20, height = 20 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();

        let ids_before: Vec<NodeId> = {
            let root = scene.surfaces.get("bar@TEST").unwrap();
            let row = &root.children[0];
            vec![root.id, row.id, row.children[0].id, row.children[1].id]
        };
        let next_id_before = scene.next_id;
        let retiring_before = scene.retiring_ids();

        // A getter that errors on read: `resolve_properties` propagates that as a `LayoutError`
        // while building the row's second child's resolved map, after the first child's map (and
        // the row's, and the surface's) was built successfully this pass. The failure is therefore
        // partway through the walk, with `next_id` and `retiring` already mutated -- which is the
        // state this test exists to prove `apply` rolls back.
        let lua2 = mlua::Lua::new();
        register_node_constructors(&lua2).unwrap();
        crate::lua::signal::register(&lua2, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua2
            .load(
                r#"
                local bad_width = computed({}, function() error("boom") end)
                return panel { id = "bar", child = row { children = {
                    rect { width = 10, height = 10 },
                    rect { width = bad_width, height = 20 },
                } } }
                "#,
            )
            .eval()
            .unwrap();
        let surface_v2 = deserialize_lua_table(&table).unwrap();

        let err = apply_at(&mut scene, &[surface_v2], full(), &shaping, &lua2).unwrap_err();
        assert!(matches!(err, LayoutError::InvalidProperty { property, .. } if property == "width"));

        let root = scene.surfaces.get("bar@TEST").unwrap();
        let row = &root.children[0];
        let ids_after = vec![root.id, row.id, row.children[0].id, row.children[1].id];
        assert_eq!(ids_after, ids_before, "NodeIds must be stable across a failed apply, not reallocated");
        assert_eq!(scene.next_id, next_id_before, "next_id must not be left bumped by the aborted pass");
        assert_eq!(scene.retiring_ids(), retiring_before, "retiring must not gain entries from the aborted pass");
        assert_eq!(row.children[1].rect.width, 20.0, "the first tree's geometry must still be intact");
    }

    #[test]
    fn reapplying_the_same_shape_at_the_same_index_reuses_the_node_id() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) =
            surface_from(r#"panel { id = "bar", child = rect { width = 10, height = 10 } }"#);
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let first_id = {
            let key = "bar@TEST";
            scene.surfaces.get(key).unwrap().children[0].id
        };

        let (_lua2, surface_v2) =
            surface_from(r#"panel { id = "bar", child = rect { width = 99, height = 99 } }"#);
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let second_id = scene.surfaces.get("bar@TEST").unwrap().children[0].id;

        assert_eq!(
            first_id, second_id,
            "same kind at the same position must reuse the retained node's identity"
        );
        assert_eq!(
            scene.surface("bar@TEST").unwrap().children[0].rect.width,
            99.0,
            "but its geometry must still refresh"
        );
    }

    #[test]
    fn a_kind_change_at_the_same_index_replaces_and_retires_the_old_node() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) =
            surface_from(r#"panel { id = "bar", child = rect { width = 10, height = 10 } }"#);
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        assert!(scene.retiring_ids().is_empty());

        let (_lua2, surface_v2) =
            surface_from(r#"panel { id = "bar", child = text { content = "hi" } }"#);
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();

        assert_eq!(scene.surface("bar@TEST").unwrap().children[0].kind, "text");
        assert_eq!(
            scene.retiring_ids().len(),
            1,
            "the replaced rect must be retired, not dropped"
        );
    }

    #[test]
    fn a_shrinking_child_list_retires_the_removed_tail() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = { rect { width = 1, height = 1 }, rect { width = 2, height = 2 } } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();

        let (_lua2, surface_v2) = surface_from(
            r#"panel { id = "bar", child = row { children = { rect { width = 1, height = 1 } } } }"#,
        );
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();

        assert_eq!(scene.surface("bar@TEST").unwrap().children[0].children.len(), 1);
        assert_eq!(scene.retiring_ids().len(), 1);
    }

    #[test]
    fn child_first_teardown_order_visits_children_before_their_parent() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = { rect { width = 1, height = 1, children = { rect { width = 1, height = 1 } } } } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let outer_id = scene.surfaces.get("bar@TEST").unwrap().children[0].children[0].id;
        let inner_id = scene.surfaces.get("bar@TEST").unwrap().children[0].children[0].children[0].id;

        // Remove the whole subtree by shrinking the row to zero children.
        let (_lua2, surface_v2) =
            surface_from(r#"panel { id = "bar", child = row { children = {} } }"#);
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();

        let order = scene.retiring_ids();
        let inner_pos = order.iter().position(|id| *id == inner_id).unwrap();
        let outer_pos = order.iter().position(|id| *id == outer_id).unwrap();
        assert!(
            inner_pos < outer_pos,
            "the child must be retired before its parent"
        );
    }

    #[test]
    fn release_removes_exactly_one_retiring_entry_and_is_false_on_an_unknown_id() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) =
            surface_from(r#"panel { id = "bar", child = rect { width = 1, height = 1 } }"#);
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let (_lua2, surface_v2) =
            surface_from(r#"panel { id = "bar", child = text { content = "x" } }"#);
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();

        let id = scene.retiring_ids()[0];
        assert!(scene.release(id));
        assert!(scene.retiring_ids().is_empty());
        assert!(
            !scene.release(id),
            "releasing an already-released id must return false"
        );
        assert!(
            !scene.release(NodeId(9999)),
            "releasing an id that never existed must return false"
        );
    }

    #[test]
    fn a_self_referential_literal_tree_is_rejected_with_a_layout_error() {
        // build-steps.md Phase 19 item 3, defect 1: `local r = rect {}; r.children = { r }`
        // recurses through resolve_and_reconcile with no bound. Before the depth cap this aborted
        // the process with a stack overflow rather than returning an Err; see this test's sibling
        // run alone (`-- --test-threads=1`) for the observed abort, recorded in the task report.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                local r = rect {}
                r.children = { r }
                return panel { id = "bar", child = r }
                "#,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(
            matches!(err, LayoutError::TreeTooDeep { .. }),
            "a cyclic literal tree must return a LayoutError, not abort the process: {err:?}"
        );
    }

    #[test]
    fn a_computed_children_signal_generating_fresh_depth_is_rejected_with_a_layout_error() {
        // build-steps.md Phase 19 item 3, defect 2: `resolve_properties`' "a Signal resolved to
        // another Signal" guard never fires here, because each read of `children` answers with a
        // fresh Table rather than a Signal. The recursion is pure Rust through
        // resolve_and_reconcile -- one `resolve_properties` per node level, each running the
        // generator once more -- so the same tree-depth cap that catches defect 1 must catch this
        // too (verified, not assumed, per the task brief).
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                local deep
                deep = computed({}, function()
                    return { rect { width = 1, height = 1, children = deep } }
                end)
                return panel { id = "bar", child = rect { children = deep } }
                "#,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        // Either cap is a pass, because the property under test is "bounded, not aborted". Two
        // caps race here and which one wins is a scheduling detail: every tree level resolves its
        // own `children` signal in a fresh `Signal::get_value`, so each level gets a fresh 5ms
        // budget, and on a loaded machine (the whole suite in parallel) one level can be
        // descheduled past 5ms and refuse before the tree ever reaches MAX_TREE_DEPTH. Observed
        // roughly once in ten full-suite runs as `InvalidProperty { property: "children", detail:
        // "Signal getter failed: runtime error: computed/map exceeded its 5ms CPU budget" }`.
        // Asserting only `TreeTooDeep` would make this test a load meter.
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        let capped = matches!(err, LayoutError::TreeTooDeep { .. })
            || matches!(&err, LayoutError::InvalidProperty { detail, .. } if detail.contains("5ms CPU budget"));
        assert!(capped, "a computed children signal generating fresh depth must be capped, not abort: {err:?}");
    }

    /// A `panel` wrapping `rows` nested `row`s around one `rect`, so the deepest level is
    /// `rows + 2`. Built with a Lua loop rather than nested table literals: at these depths the
    /// literal form runs into Lua's own `LUAI_MAXCCALLS` parser nesting limit, which would be
    /// testing the parser rather than this cap.
    fn surface_nested(lua: &mlua::Lua, rows: usize) -> VirtualNode {
        let table: mlua::Table = lua
            .load(format!(
                r#"
                local n = rect {{ width = 1, height = 1 }}
                for _ = 1, {rows} do n = row {{ children = {{ n }} }} end
                return panel {{ id = "bar", child = n }}
                "#
            ))
            .eval()
            .unwrap();
        deserialize_lua_table(&table).unwrap()
    }

    #[test]
    fn a_tree_at_the_depth_cap_is_accepted_and_one_level_past_it_is_rejected() {
        // Both caps mean the same thing: at most N levels are admitted, the N+1th is refused. The
        // `>` this used to be admitted MAX_TREE_DEPTH + 1 levels while its message claimed
        // MAX_TREE_DEPTH, and refused on the level after that.
        let deepest = MAX_TREE_DEPTH as usize;
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();

        // panel + (deepest - 2) rows + rect == exactly MAX_TREE_DEPTH levels.
        let mut scene = Scene::new();
        apply_at(&mut scene, &[surface_nested(&lua, deepest - 2)], full(), &shaping, &lua).unwrap();

        let mut scene = Scene::new();
        let err = apply_at(&mut scene, &[surface_nested(&lua, deepest - 1)], full(), &shaping, &lua).unwrap_err();
        assert!(
            matches!(err, LayoutError::TreeTooDeep { depth, max, .. }
                if depth == MAX_TREE_DEPTH + 1 && max == MAX_TREE_DEPTH),
            "one level past the cap must be refused, reporting the limit actually enforced: {err:?}"
        );
    }

    #[test]
    fn a_legitimately_deep_but_reasonable_tree_still_applies() {
        // The cap must not be tightenable into rejecting a real config: a bar with nested
        // rows/columns runs 10-15 levels deep (MAX_TREE_DEPTH's doc comment), so 20 levels of
        // plain nesting -- well above any real shell.lua -- must still apply cleanly.
        const NESTING: usize = 20;
        let mut lua_src = String::from(r#"panel { id = "bar", child = "#);
        for _ in 0..NESTING {
            lua_src.push_str(r#"row { children = { "#);
        }
        lua_src.push_str(r#"rect { width = 4, height = 4 }"#);
        for _ in 0..NESTING {
            lua_src.push_str(" } }");
        }
        lua_src.push('}');

        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(&lua_src);
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();

        // NESTING `row` levels plus one final descent into the innermost `rect`.
        let mut node = scene.surface("bar@TEST").unwrap();
        for _ in 0..=NESTING {
            assert_eq!(node.children.len(), 1);
            node = node.children.into_iter().next().unwrap();
        }
        assert_eq!(node.kind, "rect");
        assert_eq!(node.rect.width, 4.0, "the innermost rect's own geometry must have resolved");
    }

    #[test]
    fn an_identified_child_keeps_its_node_id_across_applies_when_a_sibling_is_inserted_above_it() {
        // docs/adr/0045 decision 2: an id-bearing sibling pairs by id, not by position, so
        // inserting a fresh node above it must not shift it onto a different retained
        // counterpart. Under the pre-ADR-0045 purely-positional rule (ADR-0023 § 4) this fails:
        // index 0's retained node gets handed to whatever fresh child now sits at index 0 (the
        // newly inserted one), not to "keep".
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "keep", width = 10, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let keep_id_before = scene.surfaces.get("bar@TEST").unwrap().children[0].children[0].id;

        let (_lua2, surface_v2) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 5, height = 5 },
                rect { id = "keep", width = 10, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let row = &scene.surfaces.get("bar@TEST").unwrap().children[0];

        assert_eq!(
            row.children[1].id, keep_id_before,
            "the id-matched sibling must keep its NodeId despite the insertion above it"
        );
    }

    #[test]
    fn an_unidentified_child_list_keeps_node_ids_across_applies_by_position_only() {
        // docs/adr/0045 decision 2's "degrades to today's behavior when a config uses no ids at
        // all": with no id anywhere, pairing falls back exactly to index order, so an id-less
        // list is unaffected by this slice -- inserting above a sibling still shifts it onto a
        // different retained counterpart, unlike the identified case above.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 10, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let original_id = scene.surfaces.get("bar@TEST").unwrap().children[0].children[0].id;

        let (_lua2, surface_v2) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 5, height = 5 },
                rect { width = 10, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let row = &scene.surfaces.get("bar@TEST").unwrap().children[0];

        assert_eq!(row.children[0].id, original_id, "position 0 still reuses the retained identity, since matching stays positional");
        assert_ne!(
            row.children[1].id, original_id,
            "position 1 is a fresh allocation, not a reused identity -- matching today's rule with no ids present"
        );
    }

    #[test]
    fn a_mixed_child_list_keeps_identified_node_ids_across_applies_and_the_rest_only_by_position() {
        // v1: [B (no id), anchor (id), C (no id)]. v2 inserts a new unidentified node between
        // anchor and C: [B, anchor, NEW, C]. Once "anchor" claims its retained counterpart by
        // id, the *remaining* fresh/retained subsequences are [B, NEW, C] against [B's old node,
        // C's old node] -- a plain positional zip (decision 2's fallback), so B (unmoved ahead of
        // the insertion) keeps its slot, but the insertion still bumps C onto a fresh id. This is
        // the same "no special treatment for unidentified nodes" the dedicated positional test
        // shows in isolation; here it holds even with an identified sibling in the same parent.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 1, height = 1 },
                rect { id = "anchor", width = 2, height = 2 },
                rect { width = 3, height = 3 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let (b_id_before, anchor_id_before, c_id_before) = {
            let root = scene.surfaces.get("bar@TEST").unwrap();
            let row = &root.children[0].children;
            (row[0].id, row[1].id, row[2].id)
        };

        let (_lua2, surface_v2) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 1, height = 1 },
                rect { id = "anchor", width = 2, height = 2 },
                rect { width = 9, height = 9 },
                rect { width = 3, height = 3 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let root = scene.surfaces.get("bar@TEST").unwrap();
        let row2 = &root.children[0].children;

        assert_eq!(row2.len(), 4);
        assert_eq!(row2[0].id, b_id_before, "B sits ahead of the insertion, so it keeps its slot either way");
        assert_eq!(row2[1].id, anchor_id_before, "the identified sibling keeps its id regardless of position");
        assert_ne!(
            row2[3].id, c_id_before,
            "C is unidentified, so the inserted node claims its retained slot positionally -- same rule an id-less list already had"
        );
    }

    #[test]
    fn duplicate_sibling_ids_are_rejected_as_a_layout_error() {
        // docs/adr/0045 decision 1: a duplicate id among siblings is a LayoutError routed to
        // rescue, not silent last-one-wins.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "dup", width = 1, height = 1 },
                rect { id = "dup", width = 2, height = 2 },
            } } }"#,
        );
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "id" && detail.contains("dup")),
            "duplicate sibling ids must be rejected, naming the offending id: {err:?}"
        );
    }

    #[test]
    fn the_same_id_under_two_different_parents_does_not_collide() {
        // docs/adr/0045 decision 1: id is scoped to its parent, not the tree, so two nodes under
        // different parents may share one -- what makes a reusable component composable.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                column { children = { rect { id = "inner", width = 1, height = 1 } } },
                column { children = { rect { id = "inner", width = 2, height = 2 } } },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children.len(), 2, "two columns, each with its own 'inner' child, must not collide across parents");
        assert_eq!(row.children[0].children[0].rect.width, 1.0);
        assert_eq!(row.children[1].children[0].rect.width, 2.0);
    }

    #[test]
    fn a_signal_valued_id_is_rejected() {
        // docs/adr/0045 decision 1: id follows SurfaceTopology's four fields (ADR-0044's
        // amendment banner) in rejecting a Signal outright rather than resolving it -- a
        // reconcile identity that changes from pass to pass is meaningless.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(
            Value::String(lua.create_string("x").unwrap()),
            crate::lua::signal::DirtyFlag::new(),
        )
        .0;
        lua.globals().set("sig", signal).unwrap();
        let table: mlua::Table = lua
            .load(r#"return panel { id = "bar", child = rect { id = sig, width = 1, height = 1 } }"#)
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();
        assert!(
            matches!(&err, LayoutError::UnsupportedSignalProperty(p) if p == "id"),
            "a Signal-valued id must be rejected outright, not resolved: {err:?}"
        );
    }

    #[test]
    fn a_retained_identified_child_whose_id_vanishes_is_retired_child_first_when_the_fresh_list_empties() {
        // docs/adr/0045 decision 2's last sentence: a retained child whose id disappears is
        // retired through the existing child-first path (`CONTEXT.md`, Lease), same as any other
        // removed subtree -- an identified child gets no special teardown.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "gone", width = 1, height = 1, children = { rect { width = 1, height = 1 } } },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let (outer_id, inner_id) = {
            let root = scene.surfaces.get("bar@TEST").unwrap();
            let outer = &root.children[0].children[0];
            (outer.id, outer.children[0].id)
        };

        let (_lua2, surface_v2) = surface_from(r#"panel { id = "bar", child = row { children = {} } }"#);
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();

        let order = scene.retiring_ids();
        let inner_pos = order.iter().position(|id| *id == inner_id).unwrap();
        let outer_pos = order.iter().position(|id| *id == outer_id).unwrap();
        assert!(
            inner_pos < outer_pos,
            "the id-matched subtree's retirement is still child-first, same as the positional path"
        );
    }

    #[test]
    fn a_fresh_id_that_matches_nothing_gets_a_new_node_id_and_the_vanished_id_is_retired() {
        // docs/adr/0045 decision 2, both directions of "an id means the same node and only the
        // same node": retained [a, b, c] against fresh [b, c, d]. `d` is a brand new declaration,
        // so it must not fall through to the positional pool and inherit `a`'s NodeId (and with
        // it `a`'s whole retained subtree, which a paint stage keying GPU resources on NodeId
        // would then paint `d` into). `a` left the config, so it must be retired, not silently
        // handed to `d`.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "a", width = 1, height = 1 },
                rect { id = "b", width = 2, height = 2 },
                rect { id = "c", width = 3, height = 3 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let (a_id, b_id, c_id) = {
            let row = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;
            (row[0].id, row[1].id, row[2].id)
        };

        let (_lua2, surface_v2) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "b", width = 2, height = 2 },
                rect { id = "c", width = 3, height = 3 },
                rect { id = "d", width = 4, height = 4 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let row = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;

        assert_eq!(row[0].id, b_id, "b keeps its identity across the apply");
        assert_eq!(row[1].id, c_id, "c keeps its identity across the apply");
        assert!(
            row[2].id != a_id && row[2].id != b_id && row[2].id != c_id,
            "d declared a new id, so it must get a fresh NodeId rather than adopt a retained one"
        );
        assert!(
            scene.retiring_ids().contains(&a_id),
            "a's id vanished from the config, so a must be retired rather than reused: {:?}",
            scene.retiring_ids()
        );
    }

    #[test]
    fn an_anonymous_fresh_child_never_inherits_an_identified_retained_node() {
        // The other direction of the same rule: an unidentified fresh child pairs positionally
        // only against unidentified retained children. Retained [x (id), anon] against fresh
        // [anon, anon] must hand the retained *anonymous* node to fresh slot 0 and retire `x`;
        // drawing `x` out of a shared positional pool would let an id-less node inherit the
        // identity of one that was explicitly named.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "x", width = 1, height = 1 },
                rect { width = 2, height = 2 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let (x_id, anon_id) = {
            let row = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;
            (row[0].id, row[1].id)
        };

        let (_lua2, surface_v2) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { width = 1, height = 1 },
                rect { width = 2, height = 2 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let row = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;

        assert_eq!(
            row[0].id, anon_id,
            "the unidentified subsequence pairs among itself, so the retained anonymous node lands in slot 0"
        );
        assert!(row[1].id != x_id && row[1].id != anon_id, "slot 1 has no unidentified counterpart left, so it is new");
        assert!(
            scene.retiring_ids().contains(&x_id),
            "x's id is gone from the fresh tree, so x is retired rather than adopted by an anonymous child: {:?}",
            scene.retiring_ids()
        );
    }

    #[test]
    fn removing_an_id_retires_the_old_node_and_allocates_a_new_one() {
        // Adding or removing an `id` is a change of identity, not a cosmetic edit: a node that
        // stops declaring one is a different node from docs/adr/0045's point of view, so its
        // retained counterpart is retired and the anonymous replacement allocates fresh.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) =
            surface_from(r#"panel { id = "bar", child = row { children = { rect { id = "x", width = 1, height = 1 } } } }"#);
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let x_id = scene.surfaces.get("bar@TEST").unwrap().children[0].children[0].id;

        let (_lua2, surface_v2) =
            surface_from(r#"panel { id = "bar", child = row { children = { rect { width = 1, height = 1 } } } }"#);
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let row = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;

        assert_ne!(row[0].id, x_id, "dropping the id allocates a new node rather than silently preserving identity");
        assert!(
            scene.retiring_ids().contains(&x_id),
            "the identified node that vanished must be retired: {:?}",
            scene.retiring_ids()
        );
    }

    #[test]
    fn a_vanished_id_is_retired_child_first_even_when_fresh_slots_are_still_unmatched() {
        // The non-empty companion to the test above: the fresh list is the *same length* as the
        // retained one, so there is an unclaimed slot the old fallthrough would have filled from
        // the leftover pool. Retiring must depend on whether the id was claimed, not on whether
        // the fresh list ran short.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "gone", width = 1, height = 1, children = { rect { width = 1, height = 1 } } },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let (outer_id, inner_id) = {
            let outer = &scene.surfaces.get("bar@TEST").unwrap().children[0].children[0];
            (outer.id, outer.children[0].id)
        };

        let (_lua2, surface_v2) = surface_from(
            r#"panel { id = "bar", child = row { children = {
                rect { id = "other", width = 1, height = 1 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let row = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;

        assert!(row[0].id != outer_id, "`other` is a new id, so it must not adopt `gone`'s retained node");
        let order = scene.retiring_ids();
        let inner_pos = order.iter().position(|id| *id == inner_id).expect("the vanished subtree's child must be retired");
        let outer_pos = order.iter().position(|id| *id == outer_id).expect("the vanished node must be retired");
        assert!(inner_pos < outer_pos, "retirement stays child-first even with a same-length fresh list");
    }

    #[test]
    fn many_identified_siblings_reconcile_in_reversed_order_without_a_quadratic_scan() {
        // A config is free to emit thousands of identified siblings
        // (`for i = 1, 10000 do c[i] = rect { id = "n" .. i } end`), and this pairing runs on the
        // Wayland dispatch thread at capability-push cadence (ADR-0044 decision 2's dirty flag),
        // so it must be linear. N is 2000 because that is where the gap is unmistakable without
        // making the suite slow: reversed order is the worst case for the old nested `position`
        // scan plus `Vec::remove`, and this test measured 0.80s against it versus 0.06s against
        // the map lookup, both in a debug test build. A regression to quadratic therefore shows up
        // as a test that visibly drags, without a flaky wall-clock assertion in the test itself.
        //
        // Correctness is what is asserted: every re-emitted id keeps its NodeId despite the
        // reversal, the one id that was dropped is retired, and the one id that is new allocates
        // fresh rather than inheriting the dropped node.
        const N: usize = 2000;
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();

        let mut v1_children = String::new();
        for i in 0..N {
            v1_children.push_str(&format!("rect {{ id = \"n{i}\", width = 1, height = 1 }},\n"));
        }
        let (_lua1, surface_v1) =
            surface_from(&format!("panel {{ id = \"bar\", child = row {{ children = {{ {v1_children} }} }} }}"));
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let before: Vec<NodeId> = scene.surfaces.get("bar@TEST").unwrap().children[0].children.iter().map(|c| c.id).collect();

        // Reversed, with "n0" replaced by a never-seen "fresh" so the run also covers the
        // no-counterpart path at scale.
        let mut v2_children = String::new();
        v2_children.push_str("rect { id = \"fresh\", width = 1, height = 1 },\n");
        for i in (1..N).rev() {
            v2_children.push_str(&format!("rect {{ id = \"n{i}\", width = 1, height = 1 }},\n"));
        }
        let (_lua2, surface_v2) =
            surface_from(&format!("panel {{ id = \"bar\", child = row {{ children = {{ {v2_children} }} }} }}"));
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let after: Vec<NodeId> = scene.surfaces.get("bar@TEST").unwrap().children[0].children.iter().map(|c| c.id).collect();

        assert_eq!(after.len(), N);
        for (slot, i) in (1..N).rev().enumerate() {
            assert_eq!(after[slot + 1], before[i], "n{i} must keep its NodeId across the reversal");
        }
        assert!(!before.contains(&after[0]), "the never-seen `fresh` id must allocate rather than inherit n0's node");
        assert!(scene.retiring_ids().contains(&before[0]), "n0 left the config, so it is retired");
    }

    /// The three tests below share one shape: a `margin` that is a `computed` signal counting its
    /// own reads into a Lua global, so what a single `Scene::apply` does with that property is
    /// observable from the config's side (build-steps.md Phase 19 item 5).
    fn surface_with_a_read_counting_margin(lua: &mlua::Lua) -> VirtualNode {
        register_node_constructors(lua).unwrap();
        crate::lua::signal::register(lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                reads = 0
                local m = computed({}, function() reads = reads + 1; return { left = reads } end)
                return panel { id = "bar", child = row { children = {
                    rect { width = 10, height = 10, margin = m },
                } } }
                "#,
            )
            .eval()
            .unwrap();
        deserialize_lua_table(&table).unwrap()
    }

    #[test]
    fn an_impure_margin_closure_positions_a_child_inside_the_size_its_parent_was_measured_at() {
        // build-steps.md Phase 19 item 5's headline defect. `margin` used to be resolved four
        // separate times in one apply (the parent loop, both `intrinsic_content_size` folds, and
        // `position_children`), each an independent `Signal::get_value`, so a closure that is not
        // a pure function of unchanged state answered differently within one pass: the row was
        // measured 12 wide from the second read and positioned its child at x 4 from the fourth,
        // a 10-wide child spanning 4..14 inside a 12-wide parent. That breaks the invariant
        // `intrinsic_content_size`'s own comment states -- the sizing and positioning passes "must
        // agree, or a child would be sized to fit but then overlap or leave a gap once
        // positioned".
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        let surface = surface_with_a_read_counting_margin(&lua);

        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        let root = scene.surface("bar@TEST").unwrap();
        let row = &root.children[0];
        let child = &row.children[0];
        assert_eq!(
            child.rect.x + child.rect.width,
            row.rect.width,
            "the margin the row was measured with must be the margin its child was positioned with: the child spans {}..{} inside a {}-wide row",
            child.rect.x,
            child.rect.x + child.rect.width,
            row.rect.width
        );
    }

    #[test]
    fn a_signal_valued_property_resolves_exactly_once_per_apply() {
        // The mechanism behind the test above, asserted directly: one pass, one answer per
        // property. `margin` is the property measured at four reads, so it is the one counted
        // here. This is not the memoization docs/adr/0044 decision 3 rejects -- nothing is cached
        // past the end of this apply, and the next one resolves again.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        let surface = surface_with_a_read_counting_margin(&lua);

        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        assert_eq!(
            lua.globals().get::<i64>("reads").unwrap(),
            1,
            "one Scene::apply must read a property's Signal exactly once"
        );
    }

    #[test]
    fn a_second_apply_resolves_the_property_again_rather_than_reusing_the_first_passes_answer() {
        // The other side of the same rule: resolve-once is scoped to one pass, so a push-driven
        // re-resolve (docs/adr/0044 decision 2) reads the signal again. A cache across applies
        // would be the memoization decision 3 refuses.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        let surface = surface_with_a_read_counting_margin(&lua);

        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        assert_eq!(lua.globals().get::<i64>("reads").unwrap(), 2, "each apply resolves afresh");
    }

    #[test]
    fn the_resolved_tree_holds_a_signals_current_value_not_the_handle() {
        // build-steps.md Phase 19 item 5: `ResolvedNode::properties` is a snapshot of one pass,
        // which is what makes it safe for a later paint stage to read a colour or a radius off it
        // without resolving anything itself. A property no parser reads yet (`background`) is the
        // honest test of that, since only the resolve step can have replaced it.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let white = lua.create_string("#FFFFFF").unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::String(white), crate::lua::signal::DirtyFlag::new()).0;
        lua.globals().set("bg", signal).unwrap();
        let table: mlua::Table = lua
            .load(r#"return panel { id = "bar", child = rect { background = bg, width = 4, height = 4 } }"#)
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        let child = &scene.surface("bar@TEST").unwrap().children[0];
        let background = child.properties.get("background").expect("background must survive into the resolved tree");
        assert_eq!(
            background.as_string().map(|s| s.to_string_lossy()),
            Some("#FFFFFF".to_string()),
            "the resolved tree must hold the value, not the Signal handle: {background:?}"
        );
    }

    #[test]
    fn a_child_whose_kind_is_rejected_never_runs_its_property_getters() {
        // Resolution calls back into Lua (docs/adr/0044 decision 1), so *when* it happens relative
        // to the checks is observable from the config's side. Resolving a child's map in the parent
        // loop and only then recursing into `ensure_supported_kind` meant every property getter of
        // a node the walk was about to refuse ran first: arbitrary Lua side effects on behalf of a
        // node that never enters the accepted tree. `ensure_node_admissible` runs in the parent
        // loop ahead of the resolve, so nothing of a refused child is evaluated. The depth cap is
        // the same ordering and the same fix -- one level's worth of generator body, rather than a
        // whole property map.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                ran = false
                local w = computed({}, function() ran = true; return 10 end)
                return panel { id = "bar", child = row { children = {
                    { kind = "banana", width = w },
                } } }
                "#,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err();

        assert!(
            matches!(&err, LayoutError::UnsupportedNodeKind(kind) if kind == "banana"),
            "the error must be unchanged by moving the check earlier: {err:?}"
        );
        assert!(
            !lua.globals().get::<bool>("ran").unwrap(),
            "a rejected child's property getters must not run"
        );
    }

    #[test]
    fn overlay_input_regions_includes_only_visible_direct_children() {
        let visible_child = ResolvedNode {
            kind: "rect".to_string(),
            rect: LogicalRect {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            },
            visible: true,
            properties: HashMap::new(),
            children: Vec::new(),
        };
        let hidden_child = ResolvedNode {
            kind: "rect".to_string(),
            rect: LogicalRect {
                x: 20.0,
                y: 20.0,
                width: 10.0,
                height: 10.0,
            },
            visible: false,
            properties: HashMap::new(),
            children: Vec::new(),
        };
        let root = ResolvedNode {
            kind: "panel".to_string(),
            rect: LogicalRect {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
            visible: true,
            properties: HashMap::new(),
            children: vec![visible_child, hidden_child],
        };

        let regions = overlay_input_regions(&root, 1.0);
        assert_eq!(regions.len(), 1);
        assert_eq!(
            regions[0],
            PhysicalRect {
                x0: 0,
                y0: 0,
                x1: 10,
                y1: 10
            }
        );
    }

    #[test]
    fn a_surface_with_nothing_visible_in_it_claims_no_input_at_all() {
        // The click-through case, and since docs/adr/0038 decision 5 it is the ordinary answer for
        // any surface rather than a boot-time special case for one overlay: an empty region means
        // every pointer event reaches the application window behind the surface. A root whose only
        // child is hidden and a root with no children at all must agree about that.
        let hidden_child = ResolvedNode {
            kind: "rect".to_string(),
            rect: LogicalRect { x: 0.0, y: 0.0, width: 100.0, height: 100.0 },
            visible: false,
            properties: HashMap::new(),
            children: Vec::new(),
        };
        let mut root = ResolvedNode {
            kind: "panel".to_string(),
            rect: LogicalRect { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0 },
            visible: true,
            properties: HashMap::new(),
            children: vec![hidden_child],
        };
        assert!(overlay_input_regions(&root, 1.0).is_empty());

        root.children.clear();
        assert!(overlay_input_regions(&root, 1.0).is_empty());
    }

    #[test]
    fn a_child_that_fills_its_surface_claims_the_whole_surface() {
        // The "no-op for a tightly-sized bar" build-steps.md Phase 20 item 5 predicts: the region
        // this produces is what the protocol default already is, which is why the wiring needs no
        // special case to skip it.
        let root = ResolvedNode {
            kind: "panel".to_string(),
            rect: LogicalRect { x: 0.0, y: 0.0, width: 1920.0, height: 32.0 },
            visible: true,
            properties: HashMap::new(),
            children: vec![ResolvedNode {
                kind: "row".to_string(),
                rect: LogicalRect { x: 0.0, y: 0.0, width: 1920.0, height: 32.0 },
                visible: true,
                properties: HashMap::new(),
                children: Vec::new(),
            }],
        };

        assert_eq!(overlay_input_regions(&root, 1.0), [PhysicalRect { x0: 0, y0: 0, x1: 1920, y1: 32 }]);
    }

    #[test]
    fn a_list_resolves_one_child_per_source_element_in_source_order() {
        // build-steps.md Phase 19 item 12: `list` expands `source` via `itemfn`, one child per
        // element, in the order `source` lists them -- catches both "list is still rejected as an
        // unsupported kind" and a `children_of` arm that drops or reorders elements.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = list {
                source = { 10, 20, 30 },
                itemfn = function(item) return rect { width = item, height = 5 } end,
            } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let list = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(list.children.len(), 3);
        assert_eq!(list.children[0].rect.width, 10.0);
        assert_eq!(list.children[1].rect.width, 20.0);
        assert_eq!(list.children[2].rect.width, 30.0);
    }

    #[test]
    fn a_list_with_key_keeps_existing_items_node_ids_when_a_new_element_is_inserted_at_the_front() {
        // The reason this item exists (docs/adr/0045 decision 3): `key(element)` becomes the
        // generated child's `id`, so `pair_children_by_id_then_position` (ADR-0045 decisions 1-2)
        // pairs list items by key rather than by index -- an insertion at the front must not
        // shift every existing item onto the wrong retained node.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let itemfn = r#"function(item) return rect { width = item.n, height = 1 } end"#;
        let key = r#"function(item) return item.id end"#;
        let (_lua1, surface_v1) = surface_from(&format!(
            r#"panel {{ id = "bar", child = list {{
                source = {{ {{ id = "a", n = 1 }}, {{ id = "b", n = 2 }}, {{ id = "c", n = 3 }} }},
                key = {key},
                itemfn = {itemfn},
            }} }}"#
        ));
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let (a_id, b_id, c_id) = {
            let list = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;
            (list[0].id, list[1].id, list[2].id)
        };

        let (_lua2, surface_v2) = surface_from(&format!(
            r#"panel {{ id = "bar", child = list {{
                source = {{ {{ id = "z", n = 9 }}, {{ id = "a", n = 1 }}, {{ id = "b", n = 2 }}, {{ id = "c", n = 3 }} }},
                key = {key},
                itemfn = {itemfn},
            }} }}"#
        ));
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let list = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;

        assert_eq!(list.len(), 4);
        assert_eq!(list[1].id, a_id, "a kept its retained node despite z inserted above it");
        assert_eq!(list[2].id, b_id, "b kept its retained node despite z inserted above it");
        assert_eq!(list[3].id, c_id, "c kept its retained node despite z inserted above it");
        assert!(
            list[0].id != a_id && list[0].id != b_id && list[0].id != c_id,
            "z is a genuinely new key, so it must get a freshly allocated node, not one borrowed from a's old slot"
        );
    }

    #[test]
    fn a_list_without_key_rebuilds_every_item_from_the_insertion_point_on() {
        // The documented contrast to the test above (docs/adr/0045 decision 3: "without key,
        // items match by index and every item below an insertion rebuilds"). With no key, list
        // items pair positionally, same as an id-less literal child list -- inserting at the
        // front silently hands the old position-0 retained node to whatever now sits at position
        // 0, rather than to the logical item that used to be there.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let itemfn = r#"function(item) return rect { width = item, height = 1 } end"#;
        let (_lua1, surface_v1) = surface_from(&format!(
            r#"panel {{ id = "bar", child = list {{ source = {{ 1 }}, itemfn = {itemfn} }} }}"#
        ));
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let x_id = scene.surfaces.get("bar@TEST").unwrap().children[0].children[0].id;

        let (_lua2, surface_v2) = surface_from(&format!(
            r#"panel {{ id = "bar", child = list {{ source = {{ 2, 1 }}, itemfn = {itemfn} }} }}"#
        ));
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let list = &scene.surfaces.get("bar@TEST").unwrap().children[0].children;

        assert_eq!(list.len(), 2);
        assert_eq!(
            list[0].id, x_id,
            "position 0 reuses the old retained node regardless of which logical item now occupies it"
        );
        assert_ne!(
            list[1].id, x_id,
            "the item that used to be first is now at position 1, a position with no retained counterpart, so it gets a fresh node instead of keeping x's"
        );
    }

    #[test]
    fn a_duplicate_list_key_is_rejected_naming_key_not_id() {
        // build-steps.md Phase 19 item 12 / ADR-0045 decision 3: a duplicate key is caught during
        // list expansion, before either key reaches `pair_children_by_id_then_position`, so the
        // message names `key` -- the property the config author actually wrote -- rather than
        // reusing `duplicate_sibling_ids_are_rejected_as_a_layout_error`'s "id" message.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = list {
                source = { { id = "dup" }, { id = "dup" } },
                key = function(item) return item.id end,
                itemfn = function(item) return rect { width = 1, height = 1 } end,
            } }"#,
        );
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "key" && detail.contains("dup")),
            "duplicate list keys must be rejected naming `key`, not `id`: {err:?}"
        );
    }

    #[test]
    fn a_list_with_no_source_is_a_layout_error_naming_source() {
        // Error case from build-steps.md Phase 19 item 12: `source` missing entirely.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = list { itemfn = function(item) return rect {} end } }"#,
        );
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "source"), "{err:?}");
    }

    #[test]
    fn a_list_source_that_is_not_a_table_is_a_layout_error_naming_source() {
        // Error case: `source` present but not resolving to a table/array.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = list { source = 5, itemfn = function(item) return rect {} end } }"#,
        );
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "source"), "{err:?}");
    }

    #[test]
    fn a_list_with_no_itemfn_is_a_layout_error_naming_itemfn() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(r#"panel { id = "bar", child = list { source = { 1 } } }"#);
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "itemfn"), "{err:?}");
    }

    #[test]
    fn a_list_itemfn_that_is_not_a_function_is_a_layout_error_naming_itemfn() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) =
            surface_from(r#"panel { id = "bar", child = list { source = { 1 }, itemfn = "nope" } }"#);
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "itemfn"), "{err:?}");
    }

    #[test]
    fn a_list_itemfn_raising_a_lua_error_is_a_layout_error_naming_itemfn() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = list { source = { 1 }, itemfn = function(item) error("boom") end } }"#,
        );
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap_err();
        assert!(
            matches!(&err, LayoutError::InvalidProperty { property, detail } if property == "itemfn" && detail.contains("boom")),
            "{err:?}"
        );
    }

    #[test]
    fn a_list_itemfn_returning_a_non_node_table_is_a_layout_error_naming_itemfn() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = list { source = { 1 }, itemfn = function(item) return 5 end } }"#,
        );
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "itemfn"), "{err:?}");
    }

    #[test]
    fn a_list_key_that_is_not_a_function_is_a_layout_error_naming_key() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = list { source = { 1 }, key = "nope", itemfn = function(item) return rect {} end } }"#,
        );
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "key"), "{err:?}");
    }

    #[test]
    fn a_list_key_returning_a_non_string_is_a_layout_error_naming_key() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = list {
                source = { 1 },
                key = function(item) return 5 end,
                itemfn = function(item) return rect {} end,
            } }"#,
        );
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "key"), "{err:?}");
    }

    #[test]
    fn a_list_with_an_empty_source_has_no_children_and_does_not_error() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = list { source = {}, itemfn = function(item) return rect {} end } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let list = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(list.children.len(), 0);
    }

    #[test]
    fn a_list_sizes_and_positions_like_a_column() {
        // Design decision 5: a `list` lays out exactly like a `column` -- vertical stack, own
        // width the widest child, own height the summed children plus spacing gaps.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = list {
                spacing = 3,
                source = { 1, 2 },
                itemfn = function(item) return rect { width = 6, height = 10 } end,
            } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let list = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(list.rect.width, 6.0, "own width is the widest child, same formula as column");
        assert_eq!(list.rect.height, 23.0, "10 + 10 + 3 spacing, same formula as column");
        assert_eq!(list.children[0].rect.y, 0.0);
        assert_eq!(list.children[1].rect.y, 13.0, "second item stacks below the first plus spacing");
    }

    #[test]
    fn window_and_popup_are_supported_kinds_carrying_a_single_child() {
        // docs/adr/0040 decision 1's other two roles. Both are top-level nodes, siblings of
        // `panel` in what `shell.lua` returns, and both take `child` rather than `children`
        // (§ 6.2, § 6.3). Written as raw tables because `lua::nodes::NODE_KINDS` has no
        // constructor for either yet -- that is the protocol commit's edit, not this one's.
        for kind in ["window", "popup"] {
            let mut scene = Scene::new();
            let shaping = ShapingHandle::spawn();
            let (lua, surface) = surface_from(&format!(
                r#"{{ kind = "{kind}", id = "s", child = rect {{ width = 40, height = 20 }} }}"#
            ));
            apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
            let root = scene.surface("s@TEST").unwrap();
            assert_eq!(root.kind, kind);
            assert_eq!(root.children.len(), 1, "`{kind}` must carry its `child`");
            assert_eq!(root.children[0].rect.width, 40.0);
            assert_eq!(root.children[0].rect.height, 20.0);
        }
    }

    #[test]
    fn a_window_or_popup_root_stacks_and_stretches_its_child_exactly_as_a_panel_does() {
        // Neither role has a layout model of its own: both are surface containers, so they take
        // the stacking model docs/adr/0023 item 4 already gives `panel`, including the
        // `Stretch` forcing in `stretch_forced_size`.
        for kind in ["window", "popup"] {
            let mut scene = Scene::new();
            let shaping = ShapingHandle::spawn();
            let (lua, surface) = surface_from(&format!(
                r#"{{ kind = "{kind}", id = "s", width = 100, height = 50,
                       child = rect {{ align_h = "Stretch", align_v = "Stretch" }} }}"#
            ));
            apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
            let child = &scene.surface("s@TEST").unwrap().children[0];
            assert_eq!(child.rect.width, 100.0, "`{kind}` must stretch its child like `panel`");
            assert_eq!(child.rect.height, 50.0);
        }
    }

    #[test]
    fn an_unsized_window_root_is_the_surface_so_a_fill_child_actually_fills_it() {
        // § 6.2 gives a `window` no `width`/`height`, because a toplevel's size is the compositor's
        // and reaches `available` as an `xdg_toplevel` configure. Left to `parse_size_mode`'s
        // `Content` default the root would hand its children a zero budget, and the obvious way to
        // write a window -- a `Fill` child -- resolved to nothing, so the toplevel mapped a fully
        // transparent buffer. Measured against niri (build-steps.md Phase 22 item 1).
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) =
            surface_from(r#"{ kind = "window", id = "settings", child = rect { width = "Fill", height = "Fill" } }"#);
        apply_at(&mut scene, &[surface], LogicalSize { width: 1920.0, height: 1168.0 }, &shaping, &lua).unwrap();

        let root = scene.surface("settings@TEST").unwrap();
        assert_eq!((root.rect.width, root.rect.height), (1920.0, 1168.0));
        assert_eq!((root.children[0].rect.width, root.children[0].rect.height), (1920.0, 1168.0));
    }

    #[test]
    fn a_window_root_that_does_write_a_size_still_gets_the_size_it_wrote() {
        // Per axis, and only the `Content` default is overridden: a `window` writing a `width` is
        // writing a property § 6.2 does not define, and honouring it is better than silently
        // discarding it.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(r#"{ kind = "window", id = "settings", width = 400, child = rect { width = "Fill", height = "Fill" } }"#);
        apply_at(&mut scene, &[surface], LogicalSize { width: 1920.0, height: 1168.0 }, &shaping, &lua).unwrap();

        let root = scene.surface("settings@TEST").unwrap();
        assert_eq!((root.rect.width, root.rect.height), (400.0, 1168.0), "the written width stands; the unwritten height fills");
    }

    #[test]
    fn an_unsupported_top_level_kind_is_still_rejected() {
        // The other half of adding two kinds: the list is still a list, not a catch-all.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(r#"{ kind = "dialog", id = "s", child = rect {} }"#);
        assert!(matches!(
            apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err(),
            LayoutError::UnsupportedNodeKind(k) if k == "dialog"
        ));
    }
}
