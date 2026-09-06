//! Resolve/reconcile transaction for the retained scene. [`prepare`] resolves and parses in
//! declaration order, matches ids within each parent (id-less children remain positional), and
//! retires removed subtrees child-first. [`solve`] delegates layout to taffy; [`finish`] reads
//! geometry back and performs scroll writeback and text elision. The seam that defines `row` and
//! `Fill` is [`taffy_style`] (ADR-0077).

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use mlua::{Lua, Value};

use crate::layout::instance::SurfaceInstance;
use crate::layout::node::{self, Align, EdgeInsets, LayoutError, PaintStyle, SizeMode, StyleRun};
use crate::lua::nodes::VirtualNode;
use crate::text::shaping::{self, ShapeRequest, ShapingHandle};
use crate::text::snap::{LogicalRect, PhysicalRect, snap_to_physical};
use taffy::prelude::{length, line, span};

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LogicalSize {
    pub width: f32,
    pub height: f32,
}

/// [`prepare`] admits 64 levels and refuses the next, matching
/// `lua::signal::MAX_SIGNAL_NESTING_DEPTH`'s boundary. This catches literal cycles and
/// depth-generating `children` signals; the two limits are sized together because each node can
/// nest signal evaluation.
///
/// Measured end to end on a 2 MiB debug thread with a 31-deep `computed` chain on every level; the
/// compound worst case reaches the abort boundary and counts all frames, including Lua and
/// refusal error-formatting frames. A 64-level tree without signals uses 590 KiB, about 8,960
/// B/level; 1,040 KiB with signals, the extra 450 KiB
/// paid once as the chain unwinds. The 64-level case is a 1.97x margin, versus 1,400 KiB/1.44x for
/// the old hand-written solver (ADR-0077). Production uses an 8 MiB main thread; real configs are
/// 10-15 levels deep. Signal nesting remains 32 for dependency chains.
const MAX_TREE_DEPTH: u32 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(u64);

#[cfg(test)]
impl NodeId {
    /// A hand-picked id for a `ResolvedNode` built by hand in a test. Production ids come from
    /// [`Scene::alloc_id`] and nothing else, which is what makes them unique; a test that builds a
    /// tree without a `Scene` still has to say which nodes are the same node and which are not.
    pub(crate) const fn test(raw: u64) -> Self {
        NodeId(raw)
    }
}

/// Geometry parsed once per node/pass. A resolved table's `__index` still runs on each access, so
/// this is separate from reading a `Signal`: the old pass made 16 `__index` calls for one child's
/// margin, measured a row at 18 wide, then placed its 10-wide child at 16..26. The parent parses a
/// child before recursing because it needs the margin and size for the solver (ADR-0077); ignored
/// fields are still validated so a later kind change cannot hide a malformed property (ADR-0068).
#[derive(Debug, Clone, Copy, PartialEq)]
struct LayoutStyle {
    margin: EdgeInsets,
    padding: EdgeInsets,
    width_mode: SizeMode,
    height_mode: SizeMode,
    /// Ceilings for `Content` growth; overflow goes to `scroll`.
    max_width: Option<f32>,
    max_height: Option<f32>,
    align_h: Align,
    align_v: Align,
    spacing: f32,
    visible: bool,
    opacity: f32,
}

impl LayoutStyle {
    /// The one parse of one node's geometry for one pass. `properties` must already be a
    /// [`node::resolve_properties`] result: this reads values, it does not resolve signals.
    fn parse(properties: &HashMap<String, Value>) -> Result<Self, LayoutError> {
        // Validated and not kept: the pointer path reads the name back off `properties` when it
        // needs it (`layout::hit::cursor_under`), and a pass is the place a misspelling fails.
        node::parse_cursor(properties)?;
        Ok(Self {
            margin: node::parse_edge_insets(properties, "margin")?,
            padding: node::parse_edge_insets(properties, "padding")?,
            width_mode: node::parse_size_mode(properties, "width")?,
            height_mode: node::parse_size_mode(properties, "height")?,
            max_width: node::parse_max_size(properties, "max_width")?,
            max_height: node::parse_max_size(properties, "max_height")?,
            align_h: node::parse_align(properties, "align_h")?,
            align_v: node::parse_align(properties, "align_v")?,
            spacing: node::parse_spacing(properties)?,
            visible: node::parse_visible(properties)?,
            opacity: node::parse_opacity(properties)?,
        })
    }

    /// This node's margin along `axis`, both edges.
    fn margin_on(&self, axis: MainAxis) -> f32 {
        match axis {
            MainAxis::Horizontal => self.margin.horizontal(),
            MainAxis::Vertical => self.margin.vertical(),
        }
    }
}

/// Public, ID-bearing output of resolution: geometry, parsed paint, and the resolved property map
/// retained for `hover`, callbacks, and surface specs (`wayland::surface::apply_resolved_state`
/// re-derives those at configure cadence). Used by `Scene::surface` and `overlay_input_regions`.
///
/// `resolve_properties` ran over this node's raw map exactly once, so this snapshot lets later
/// readers take values without resolving anything. `properties` holds resolved values, never a
/// `Signal` handle. Structural keys are copied raw by `node::is_structural_property`, whose parsers
/// reject signals.
///
/// ponytail: absent and nil are one state here (`node::resolve_properties` omits a key whose
/// signal resolved to `Value::Nil`, ADR-0044 decision 1's amendment), so a paint-only property
/// bound to a still-unresolved capability signal reads as its parser's default. Upgrade path: a
/// third state, `Value::Nil` retained as "bound but unresolved".
#[derive(Debug, Clone)]
pub struct ResolvedNode {
    /// The identity its retained counterpart was reconciled under, carried so a later reader can
    /// say "this node, again" across passes. Stable by construction: `reconcile_node` keeps the
    /// retained node's id and only allocates when there was nothing to match, so an id survives
    /// the node moving, resizing, or gaining siblings ahead of it (ADR-0099).
    ///
    /// Not addressable from Lua and not the § 5.1 `id` property, which is a reconciliation *hint*
    /// a config writes and this is the answer the engine reached.
    pub id: NodeId,
    pub kind: String,
    pub rect: LogicalRect,
    pub visible: bool,
    /// This node's own `opacity`, before any ancestor's. `layout::paint::build_node` multiplies
    /// the chain descending, the same way it intersects a clip, so a panel fades with everything
    /// in it from one property. 1.0 is the default and contributes nothing.
    pub opacity: f32,
    pub properties: HashMap<String, Value>,
    /// This node's paint properties, parsed here rather than by `layout::paint` on every frame
    /// (`node::paint_style`'s module doc comment says why). `None` for a kind that draws nothing.
    pub paint: Option<PaintStyle>,
    pub children: Vec<ResolvedNode>,
}

/// Retained geometry, properties, paint, and `NodeId` for the next reconcile. `Clone` exists only
/// for `Scene::apply`'s rollback snapshot.
#[derive(Clone)]
struct RetainedNode {
    id: NodeId,
    kind: String,
    rect: LogicalRect,
    /// Geometry for the pass that produced this node, including `visible` and `opacity`.
    style: LayoutStyle,
    properties: HashMap<String, Value>,
    paint: Option<PaintStyle>,
    children: Vec<RetainedNode>,
}

impl RetainedNode {
    fn to_resolved(&self) -> ResolvedNode {
        ResolvedNode {
            id: self.id,
            kind: self.kind.clone(),
            rect: self.rect,
            visible: self.style.visible,
            opacity: self.style.opacity,
            properties: self.properties.clone(),
            paint: self.paint.clone(),
            children: self.children.iter().map(RetainedNode::to_resolved).collect(),
        }
    }
}

/// Persistent trees keyed by surface instance (`"{id}@{output}"`), not declared id. A panel on
/// `monitor = "All"` needs separate trees for a laptop and 4K output because their geometry
/// differs. Surface `id` remains reconcile identity; the output suffix distinguishes instances
/// (ADR-0045). Descendants use per-parent id matching, with positional fallback for id-less nodes.
#[derive(Default)]
pub struct Scene {
    surfaces: HashMap<String, RetainedNode>,
    /// Removed subtrees in child-first order until [`Scene::release`] finalizes the drop
    /// (`CONTEXT.md`, Lease). `Vec` preserves that order; production has no release caller yet.
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

    /// Test-only unguarded apply; production uses the admitting path that the lock veto depends on.
    #[cfg(test)]
    pub fn apply(
        &mut self,
        fresh_surfaces: &[VirtualNode],
        instances: &[SurfaceInstance],
        shaping: &ShapingHandle,
        lua: &Lua,
    ) -> Result<(), LayoutError> {
        self.apply_admitting(fresh_surfaces, instances, shaping, lua, |_| Ok(()))
    }

    /// Reconciles one retained tree per mapped instance against `fresh_surfaces`, using each
    /// instance's `available` size. Missing declared instances are skipped for unplugged outputs;
    /// an instance naming no declaration is an `InvalidProperty`. Retained instances absent from
    /// this cycle stay for topology handling, not in-place apply.
    ///
    /// `admit` vetoes the finished apply after all instances, asking whether the whole resolved
    /// lock tree remains authenticatable; it rolls back on error. The snapshot restores exactly the
    /// pre-call state because a failing getter may already have changed `next_id`, `retiring`, or
    /// the trees (`CONTEXT.md`, Rollback; `socket.rs::handle_reevaluate`).
    ///
    /// ponytail: every apply deep-clones the tree, even on success; the dirty flag limits this to
    /// capability-push cadence. The structural clone is O(nodes), not O(Lua heap).
    pub fn apply_admitting(
        &mut self,
        fresh_surfaces: &[VirtualNode],
        instances: &[SurfaceInstance],
        shaping: &ShapingHandle,
        lua: &Lua,
        admit: impl Fn(&Scene) -> Result<(), LayoutError>,
    ) -> Result<(), LayoutError> {
        let next_id_snapshot = self.next_id;
        let retiring_snapshot_len = self.retiring.len();
        let surfaces_snapshot = self.surfaces.clone();

        // One budget for the whole pass: the hook covers gaps where a resolved table's `__index`
        // runs, and individually legal 5ms getters cannot add up without a pass deadline.
        let budget = match crate::lua::signal::LayoutPassBudget::enter(lua) {
            Ok(budget) => budget,
            Err(err) => return Err(node::invalid("layout", err.to_string())),
        };
        // A hook interruption can look like an arbitrary `InvalidProperty`; report the pass budget
        // instead whenever the deadline was exceeded.
        let blame_the_budget =
            |outcome: LayoutError| if budget.exceeded() { LayoutError::PassBudgetExceeded } else { outcome };

        for instance in instances {
            if let Err(err) = self.apply_one_instance(fresh_surfaces, instance, shaping, lua) {
                self.surfaces = surfaces_snapshot;
                self.next_id = next_id_snapshot;
                self.retiring.truncate(retiring_snapshot_len);
                return Err(blame_the_budget(err));
            }
        }
        if let Err(err) = admit(self) {
            self.surfaces = surfaces_snapshot;
            self.next_id = next_id_snapshot;
            self.retiring.truncate(retiring_snapshot_len);
            return Err(blame_the_budget(err));
        }
        // Lua can catch the hook error with `pcall`; the final deadline check cannot be caught.
        if budget.exceeded() {
            self.surfaces = surfaces_snapshot;
            self.next_id = next_id_snapshot;
            self.retiring.truncate(retiring_snapshot_len);
            return Err(LayoutError::PassBudgetExceeded);
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
        // Match the declared id, then key the retained tree by instance id (ADR-0045 decision 1).
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
        // Check admissibility before resolution runs Lua. Children get the same check in the loop
        // that parses their margin before recursing.
        ensure_node_admissible(&fresh.kind, 0)?;
        let properties = node::resolve_properties(&fresh.properties, &fresh.kind, lua)?;
        let properties = build_child_for_output(properties, &fresh.kind, &instance.output)?;
        // The root has no parent, so its once-per-node parse happens here; children parse in the
        // parent's loop.
        let style = LayoutStyle::parse(&properties)?;
        // An unsized `window` or `lock` root is its configured surface. `available` is the
        // compositor's `xdg_toplevel` configure size, already converted by `set_instance_size`.
        // A window's `Content`
        // default used to give children a zero budget, so `child = column { width = "Fill" }`
        // painted a 0x0 tree into niri's configured 1920x1168 tile. A lock has no width/height
        // (`lock_spec` refuses both), so the same default produced a transparent buffer over a
        // locked session, the passwordless black screen ADR-0052 decision 3 rejects. Override only
        // the `Content` axes; a window's explicitly supplied fields remain honoured.
        let forced = if matches!(fresh.kind.as_str(), "window" | "lock") {
            (
                (style.width_mode == SizeMode::Content).then_some(available.width),
                (style.height_mode == SizeMode::Content).then_some(available.height),
            )
        } else {
            (None, None)
        };

        // Taffy trees are per-instance and per-pass; only retained `NodeId`s cross the call, so a
        // failed walk drops the temporary tree without extra rollback state.
        let mut tree: taffy::TaffyTree<Measure> = taffy::TaffyTree::new();
        // Geometry stays fractional until `layout::text::snap` applies the surface scale at paint.
        // Taffy otherwise rounds layouts to whole numbers.
        tree.disable_rounding();
        let prepared = prepare(self, &mut tree, existing, &fresh.kind, properties, style, None, lua, 0)?;

        // Patch the root after the walk: its `Fill`/`Percent` resolve against configured room
        // because it has no parent; descendants take size from the solver.
        let mut root_style = tree.style(prepared.taffy).map_err(taffy_failed)?.clone();
        root_style.size = taffy::Size {
            width: match forced.0.or_else(|| resolve_non_content(style.width_mode, available.width)) {
                Some(width) => taffy::Dimension::length(width),
                None => taffy::Dimension::auto(),
            },
            height: match forced.1.or_else(|| resolve_non_content(style.height_mode, available.height)) {
                Some(height) => taffy::Dimension::length(height),
                None => taffy::Dimension::auto(),
            },
        };
        tree.set_style(prepared.taffy, root_style).map_err(taffy_failed)?;
        solve(&mut tree, prepared.taffy, available, shaping)?;
        self.surfaces.insert(key, finish(&tree, prepared, shaping)?);
        Ok(())
    }

    /// One surface instance's resolved tree, by its `"{id}@{output}"` instance id
    /// (`layout::instance::SurfaceInstance::instance_id`), not by the declared `id` a config
    /// writes. `crate::wayland::App::paint_surface` looks a tree up with exactly the id its
    /// `TrackedSurface` carries, which is what makes the two id spaces one (ADR-0038).
    pub fn surface(&self, instance_id: &str) -> Option<ResolvedNode> {
        self.surfaces.get(instance_id).map(RetainedNode::to_resolved)
    }

    /// Finalizes the drop of one retired subtree. Returns `false` if `id` isn't currently
    /// retiring (already released, or never retired).
    ///
    /// ponytail: no production caller yet, nothing in this codebase owns a per-node GPU resource
    /// to guard. Built ahead of that consumer (ADR-0023), matching
    /// `supervisor/src/socket.rs`'s `GenerationRegistry::send_to` precedent. Exercised by this
    /// module's own tests only.
    #[allow(dead_code)]
    pub fn release(&mut self, id: NodeId) -> bool {
        if let Some(pos) = self.retiring.iter().position(|(rid, _)| *rid == id) {
            self.retiring.remove(pos);
            true
        } else {
            false
        }
    }

    /// Releases every retired subtree at once, in the child-first order
    /// [`Self::retire_child_first`] established. `crate::socket`'s `RendererClient` calls this
    /// after each successful `Scene::apply`, stopping the lease bag growing forever now that apply
    /// runs at capability-push cadence (ADR-0044 decision 2), not once per config edit: every
    /// re-resolve that shortens a `children` list retires the tail, and each retired `RetainedNode`
    /// holds a `HashMap<String, mlua::Value>`, so an undrained bag leaks Lua heap and Rust memory
    /// in a process meant to live a whole session. Unconditional, correct only because nothing here
    /// holds a lease today, so nothing can be mid-teardown when this runs (ADR-0023,
    /// [`Self::release`]'s own ponytail).
    ///
    /// ponytail: wrong once a real lease holder exists, a paint stage owning per-node GPU resources
    /// would have a texture freed out from under it here. Upgrade path: release driven by the
    /// holder's own `release` calls per node, `apply` no longer the trigger.
    pub fn release_all_retired(&mut self) {
        self.retiring.clear();
    }

    /// Surfaces, total nodes across every retained tree, live `properties` values, and the lease
    /// bag's depth, for `crate::wayland::memory_profile`. Counts `properties` because that map is
    /// the one place a retained tree holds `mlua::Value`s, so it is where scene growth shows up in
    /// the Lua heap rather than the Rust one. Walks every tree, which is why the profile calls it
    /// once per report window and never per turn.
    pub fn census(&self) -> (usize, usize, usize, usize) {
        fn walk(node: &RetainedNode, nodes: &mut usize, properties: &mut usize) {
            *nodes += 1;
            *properties += node.properties.len();
            for child in &node.children {
                walk(child, nodes, properties);
            }
        }
        let mut nodes = 0;
        let mut properties = 0;
        for tree in self.surfaces.values() {
            walk(tree, &mut nodes, &mut properties);
        }
        // Retired subtrees keep their own `properties` until `release_all_retired`, so they are
        // counted too: an apply caught mid-flight must not read as a drop in live nodes.
        for (_, tree) in &self.retiring {
            walk(tree, &mut nodes, &mut properties);
        }
        (self.surfaces.len(), nodes, properties, self.retiring.len())
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
        let RetainedNode { id, kind, rect, style, properties, paint, children } = node;
        for child in children {
            self.retire_child_first(child);
        }
        self.retiring.push((id, RetainedNode { id, kind, rect, style, properties, paint, children: Vec::new() }));
    }
}

/// Admits all four § 6 surface roles as containers with one `child` tree, including a lock tree
/// before the compositor has handed out a surface (ADR-0040 decision 1, ADR-0052 decision 2).
fn ensure_supported_kind(kind: &str) -> Result<(), LayoutError> {
    match kind {
        "panel" | "window" | "popup" | "lock" | "rect" | "row" | "column" | "text" | "icon" | "image" | "button"
        | "list" | "textfield" => Ok(()),
        other => Err(LayoutError::UnsupportedNodeKind(other.to_string())),
    }
}

/// Checks kind and depth before any `resolve_properties` call. Resolution runs Lua (ADR-0044), so
/// checking afterward once ran a self-generating `children` getter 64 times against the 64-level
/// cap, and ran every getter on unsupported kinds before refusing them. Children are checked at
/// `depth + 1`.
fn ensure_node_admissible(kind: &str, depth: u32) -> Result<(), LayoutError> {
    ensure_supported_kind(kind)?;
    // `>=` admits levels 0..63, exactly 64. `>` would admit 65 while claiming 64 and disagree with
    // `MAX_SIGNAL_NESTING_DEPTH`; the reported depth is the 1-based refused level.
    if depth >= MAX_TREE_DEPTH {
        return Err(LayoutError::TreeTooDeep { kind: kind.to_string(), depth: depth + 1, max: MAX_TREE_DEPTH });
    }
    Ok(())
}

/// Selects `child`, `children`, generated list children, or no children. Surface roles share one
/// `child`; `textfield` is a leaf (§ 5.2 item 8). Its callbacks and secure-submit fields remain in
/// `RetainedNode.properties`; the keyboard path reads the latter from the scene while the secret
/// buffer stays on `App` (ADR-0005).
fn children_of(kind: &str, properties: &HashMap<String, Value>) -> Result<Vec<VirtualNode>, LayoutError> {
    match kind {
        "panel" | "window" | "popup" | "lock" => {
            Ok(node::parse_single_child(properties, "child")?.into_iter().collect())
        }
        "rect" | "row" | "column" | "button" => node::parse_children(properties),
        // ADR-0045 decision 3: list children are generated from `source`, not a literal table.
        "list" => node::parse_list_children(properties),
        "text" | "icon" | "image" | "textfield" => Ok(Vec::new()),
        other => unreachable!("ensure_supported_kind already rejected `{other}`"),
    }
}

/// `child = function(output)` on a `panel`/`lock` (ADR-0121) runs per instance and pass, with the
/// output name, before the ordinary child walk. Per-pass calls preserve registry-stable state such
/// as `state("wallpaper_" .. output)`. `window`/`popup` have no output name, so function children
/// are refused rather than called with `""`.
fn build_child_for_output(
    mut properties: HashMap<String, Value>,
    kind: &str,
    output: &str,
) -> Result<HashMap<String, Value>, LayoutError> {
    let Some(Value::Function(builder)) = properties.get("child") else {
        return Ok(properties);
    };
    if !matches!(kind, "panel" | "lock") {
        return Err(node::invalid(
            "child",
            format!(
                "a function child is for a `panel` or `lock`, which have one instance per output to hand it; \
                 a `{kind}` has one instance wherever the compositor places it"
            ),
        ));
    }
    let built = builder.call::<Value>(output).map_err(|e| node::invalid("child", e.to_string()))?;
    match built {
        Value::Table(_) => {
            properties.insert("child".to_string(), built);
        }
        // A nil result maps this output empty, like an absent `child`.
        Value::Nil => {
            properties.remove("child");
        }
        other => {
            return Err(node::invalid(
                "child",
                format!("expected the function to return a node table, got {}", node::preview_for_error(&other)),
            ));
        }
    }
    Ok(properties)
}

/// Resolves only a surface root's `Fill`/`Percent` against available room. `Content` stays `auto`
/// until children are known; descendants use the solver.
fn resolve_non_content(mode: SizeMode, available: f32) -> Option<f32> {
    match mode {
        SizeMode::Content => None,
        SizeMode::Pixels(n) => Some(n),
        SizeMode::Fill => Some(available),
        SizeMode::Percent(p) => Some(available * p),
    }
}

/// Pairs children by identity (ADR-0045 decisions 1-2): an `id` matches only the same `id`, while
/// id-less children match positionally among other id-less children. An id miss is new, never a
/// positional fallback, so it cannot inherit an unrelated `NodeId`/subtree. Unclaimed retained
/// nodes retire child-first. The linear match uses one `HashMap<&str, usize>` per parent; sibling
/// count is unbounded (`1..10000` is legal Lua), and this runs on the Wayland dispatch thread at
/// capability-push cadence (ADR-0044 decision 2).
fn pair_children_by_id_then_position(
    scene: &mut Scene,
    fresh_children: &[VirtualNode],
    old_children: Vec<RetainedNode>,
) -> Result<Vec<Option<RetainedNode>>, LayoutError> {
    let fresh_ids: Vec<Option<String>> =
        fresh_children.iter().map(|c| node::parse_node_id(&c.properties)).collect::<Result<_, _>>()?;

    // Decision 1: reject duplicate sibling ids before touching `old_children`, so failure retires
    // nothing.
    let mut seen: HashSet<&str> = HashSet::with_capacity(fresh_ids.len());
    for id in fresh_ids.iter().flatten() {
        if !seen.insert(id.as_str()) {
            return Err(LayoutError::InvalidProperty {
                property: "id".to_string(),
                detail: format!("duplicate id `{id}` among siblings"),
            });
        }
    }

    // Retained ids were already validated and cannot hold signals, so `.ok().flatten()` safely
    // treats absent and validated-no-id alike.
    let old_ids: Vec<Option<String>> =
        old_children.iter().map(|c| node::parse_node_id(&c.properties).ok().flatten()).collect();
    let mut old_slots: Vec<Option<RetainedNode>> = old_children.into_iter().map(Some).collect();

    let mut retained_by_id: HashMap<&str, usize> = HashMap::with_capacity(old_ids.len());
    for (index, id) in old_ids.iter().enumerate() {
        if let Some(id) = id {
            retained_by_id.insert(id.as_str(), index);
        }
    }

    // Identified subsequence: a miss stays `None`, not a positional slot.
    let mut matched: Vec<Option<RetainedNode>> = Vec::with_capacity(fresh_children.len());
    for fresh_id in &fresh_ids {
        let claimed = fresh_id
            .as_deref()
            .and_then(|id| retained_by_id.get(id).copied())
            .and_then(|index| old_slots[index].take());
        matched.push(claimed);
    }

    // Id-less subsequences zip in order (ADR-0023); identified retained children are excluded.
    let mut unidentified_old = old_ids.iter().enumerate().filter(|(_, id)| id.is_none()).map(|(index, _)| index);
    for (slot, fresh_id) in matched.iter_mut().zip(&fresh_ids) {
        if fresh_id.is_none()
            && let Some(index) = unidentified_old.next()
        {
            *slot = old_slots[index].take();
        }
    }

    // Unclaimed nodes, including vanished ids, retire child-first in their old order.
    for slot in &mut old_slots {
        if let Some(leftover) = slot.take() {
            scene.retire_child_first(leftover);
        }
    }

    Ok(matched)
}

/// The parent's flow axis. Stacking parents have none; each child gets the whole content box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MainAxis {
    Horizontal,
    Vertical,
}

/// Which axis `kind` flows along; a `list` borrows its `direction` from [`flow_kind`].
fn main_axis_of(kind: &str, properties: &HashMap<String, Value>) -> Result<Option<MainAxis>, LayoutError> {
    Ok(match flow_kind(kind, properties)? {
        "row" => Some(MainAxis::Horizontal),
        "column" => Some(MainAxis::Vertical),
        _ => None,
    })
}

/// Leaf sizes taffy cannot derive from style alone. Parsed in [`prepare`] so malformed icon sizes
/// can return [`LayoutError`]; the measure callback returns only `Size<f32>`.
enum Measure {
    /// Shaped extent; `wrap` and `max_lines` change geometry, not only paint.
    Text { content: String, runs: Vec<StyleRun>, font_size: f32, wrap: node::Wrap, max_lines: Option<usize> },
    /// § 5.1's `icon` `size`, the same number on both axes.
    Square(f32),
}

/// One node after identity, resolution and parsing, and before geometry: everything a
/// [`RetainedNode`] needs except the rect, plus the taffy node that rect will come out of.
///
/// The tree of these is what [`prepare`] builds walking the fresh `VirtualNode` tree in
/// declaration order, and what [`finish`] walks again to read the solved geometry back.
struct PreparedNode {
    id: NodeId,
    kind: String,
    style: LayoutStyle,
    properties: HashMap<String, Value>,
    paint: Option<PaintStyle>,
    taffy: taffy::NodeId,
    children: Vec<PreparedNode>,
    /// The retained children of a node that is not `visible` this pass, carried through untouched
    /// (ADR-0124): not rebuilt, not laid out, not retired. `children` is empty whenever this is
    /// not.
    frozen: Vec<RetainedNode>,
}

/// `Content` and `Fill` map to taffy's `auto`; `Fill` gets its meaning from parent flow and
/// [`taffy_style`]'s grow/stretch rules.
fn taffy_dimension(mode: SizeMode) -> taffy::Dimension {
    match mode {
        SizeMode::Content | SizeMode::Fill => taffy::Dimension::auto(),
        SizeMode::Pixels(n) => taffy::Dimension::length(n),
        SizeMode::Percent(p) => taffy::Dimension::percent(p),
    }
}

/// `Align` as an item's alignment inside its parent slot.
fn item_align(align: Align) -> taffy::AlignSelf {
    match align {
        Align::Start => taffy::AlignItems::START,
        Align::Center => taffy::AlignItems::CENTER,
        Align::End => taffy::AlignItems::END,
        Align::Stretch => taffy::AlignItems::STRETCH,
    }
}

/// `Align` as a flow container's packing. `Stretch` remains `Start`: the old pass kept the same
/// main-axis cursor for both, and promoting it to a taffy distribution would change behavior.
fn main_align(align: Align) -> taffy::JustifyContent {
    match align {
        Align::Start | Align::Stretch => taffy::JustifyContent::START,
        Align::Center => taffy::JustifyContent::CENTER,
        Align::End => taffy::JustifyContent::END,
    }
}

/// One node's taffy style, combining its container and item roles. `parent_axis` decides whether a
/// `Fill` shares a flow remainder or takes the whole slot; `None` means stacking or surface root.
fn taffy_style(
    kind: &str,
    properties: &HashMap<String, Value>,
    style: &LayoutStyle,
    parent_axis: Option<MainAxis>,
) -> Result<taffy::Style, LayoutError> {
    // Invisible nodes get no size, position, or spacing gap. The old pass resolved their geometry
    // before declining to place them; all readers already filter on `visible`.
    if !style.visible {
        return Ok(taffy::Style { display: taffy::Display::None, ..taffy::Style::DEFAULT });
    }

    let mut out = taffy::Style {
        // No shrink: fixed children keep their stated size, even when siblings overflow. Disable
        // taffy's automatic minimum so a `Fill` item can collapse to zero as in the old pass.
        // On a flex cross axis, leave `min_size` as `auto`: taffy 0.14 otherwise adds the
        // container's margin to each child's minimum (`constants.margin` instead of `child.margin`
        // in `determine_flex_base_size`/`determine_container_main_size`). `Some(0) + margin` floors
        // it; `None + margin` does not. The bug measured a panel body at 1521px wide/one line,
        // then drew it 378px wide/two lines, making every card a line short
        // (`a_containers_own_margin_does_not_widen_what_its_children_are_measured_at`).
        flex_shrink: 0.0,
        min_size: match parent_axis {
            Some(MainAxis::Horizontal) => {
                taffy::Size { width: length(0.0), height: taffy::LengthPercentageAuto::auto() }
            }
            Some(MainAxis::Vertical) => taffy::Size { width: taffy::LengthPercentageAuto::auto(), height: length(0.0) },
            None => taffy::Size { width: length(0.0), height: length(0.0) },
        },
        padding: taffy::Rect {
            left: length(style.padding.left),
            right: length(style.padding.right),
            top: length(style.padding.top),
            bottom: length(style.padding.bottom),
        },
        margin: taffy::Rect {
            left: length(style.margin.left),
            right: length(style.margin.right),
            top: length(style.margin.top),
            bottom: length(style.margin.bottom),
        },
        size: taffy::Size { width: taffy_dimension(style.width_mode), height: taffy_dimension(style.height_mode) },
        // The ceiling is taffy's own `max-height`: the node's auto height is measured from its
        // children and then capped, and the children keep the height they were given, which is
        // what leaves `finish`'s `extent_along` a remainder for the scroll offset to be clamped to.
        max_size: taffy::Size {
            width: style.max_width.map_or_else(taffy::LengthPercentageAuto::auto, taffy::LengthPercentageAuto::length),
            height: style
                .max_height
                .map_or_else(taffy::LengthPercentageAuto::auto, taffy::LengthPercentageAuto::length),
        },
        ..taffy::Style::DEFAULT
    };

    // Container half.
    match main_axis_of(kind, properties)? {
        Some(axis) => {
            out.display = taffy::Display::Flex;
            out.flex_direction = match axis {
                MainAxis::Horizontal => taffy::FlexDirection::Row,
                MainAxis::Vertical => taffy::FlexDirection::Column,
            };
            // A flow container packs along its own axis; children control the other axis.
            out.justify_content = Some(main_align(match axis {
                MainAxis::Horizontal => style.align_h,
                MainAxis::Vertical => style.align_v,
            }));
            // Adjacent-child spacing is a flex gap; set both axes because there is one flex line.
            out.gap = taffy::Size { width: length(style.spacing), height: length(style.spacing) };
        }
        // ADR-0023's stacking model is one auto-sized grid cell: children overlap and align
        // independently, while `Content` is their bounding union.
        None => out.display = taffy::Display::Grid,
    }

    // Item half. `Fill` off the parent's flow axis means the whole slot and outranks alignment,
    // matching the old pass's fill-then-align order.
    let fills_h = style.width_mode == SizeMode::Fill && parent_axis != Some(MainAxis::Horizontal);
    let fills_v = style.height_mode == SizeMode::Fill && parent_axis != Some(MainAxis::Vertical);
    let align_h = if fills_h { taffy::AlignItems::STRETCH } else { item_align(style.align_h) };
    let align_v = if fills_v { taffy::AlignItems::STRETCH } else { item_align(style.align_v) };

    // Flex parents govern the main axis with `justify_content`; grid items state both axes.
    let (governed_h, governed_v) = match parent_axis {
        Some(MainAxis::Horizontal) => (None, Some(align_v)),
        Some(MainAxis::Vertical) => (Some(align_h), None),
        None => (Some(align_h), Some(align_v)),
    };

    // Taffy's `align-self: stretch` applies only to an `auto` cross size, so `height = 5` would
    // normally win. The old pass overrode the size, and this engine keeps stretch precedence
    // (`row_child_stretch_alignment_fills_the_cross_axis`); blank the size to make taffy do that.
    if governed_h == Some(taffy::AlignItems::STRETCH) {
        out.size.width = taffy::Dimension::auto();
    }
    if governed_v == Some(taffy::AlignItems::STRETCH) {
        out.size.height = taffy::Dimension::auto();
    }

    match parent_axis {
        // Flex item: parent packs the main axis; cross alignment is local. A zero basis makes
        // main-axis `Fill` share the whole remainder
        // (`two_fill_siblings_split_the_remainder_equally`).
        Some(MainAxis::Horizontal) => {
            out.align_self = Some(align_v);
            if style.width_mode == SizeMode::Fill {
                out.flex_grow = 1.0;
                out.flex_basis = taffy::Dimension::length(0.0);
            }
        }
        Some(MainAxis::Vertical) => {
            out.align_self = Some(align_h);
            if style.height_mode == SizeMode::Fill {
                out.flex_grow = 1.0;
                out.flex_basis = taffy::Dimension::length(0.0);
            }
        }
        // Grid item in the shared cell; both alignments are local.
        None => {
            out.justify_self = Some(align_h);
            out.align_self = Some(align_v);
            out.grid_row = taffy::Line { start: line(1), end: span(1) };
            out.grid_column = taffy::Line { start: line(1), end: span(1) };
        }
    }

    Ok(out)
}

/// Builds one node's [`taffy_style`] and hands back the solver node holding it, with no children
/// attached yet.
///
/// Split out of [`prepare`] purely for the stack: a `taffy::Style` is 552 bytes, and [`prepare`]
/// recurses one frame per tree level with `MAX_TREE_DEPTH` of them allowed, so a `Style` built
/// there is 552 bytes multiplied by the depth cap. Built here, it lives in a frame that returns
/// before the recursion descends.
fn new_solver_node(
    tree: &mut taffy::TaffyTree<Measure>,
    kind: &str,
    properties: &HashMap<String, Value>,
    style: &LayoutStyle,
    parent_axis: Option<MainAxis>,
    measure: Option<Measure>,
) -> Result<taffy::NodeId, LayoutError> {
    let solver_style = taffy_style(kind, properties, style, parent_axis)?;
    match measure {
        Some(measure) => tree.new_leaf_with_context(solver_style, measure),
        None => tree.new_leaf(solver_style),
    }
    .map_err(taffy_failed)
}

/// The first of the two walks: identity, resolution, parsing and tree construction, in the order
/// the config wrote the nodes.
///
/// Everything that can run Lua or fail happens here, depth-first in declaration order, guaranteeing
/// every getter fires exactly once, in source order. The hand-written pass had to work to keep
/// that, recursing into `Fill` children after their siblings and splitting resolution out of the
/// recursion; here there are no rounds, so declaration and recursion order are the same one.
///
/// `properties` and `style` arrive already resolved and parsed, done by the parent:
/// [`taffy_style`] needs a child's `margin` and size modes to build the node, so the parse happens
/// in the parent's loop. `Scene::apply_one_instance` does it for a surface root, which has none.
#[allow(clippy::too_many_arguments)]
fn prepare(
    scene: &mut Scene,
    tree: &mut taffy::TaffyTree<Measure>,
    retained: Option<RetainedNode>,
    kind: &str,
    properties: HashMap<String, Value>,
    style: LayoutStyle,
    parent_axis: Option<MainAxis>,
    lua: &Lua,
    depth: u32,
) -> Result<PreparedNode, LayoutError> {
    // Already run by whoever resolved `properties` (that is the ordering `ensure_node_admissible`
    // exists to enforce), repeated here so this function holds its own preconditions rather than
    // trusting a call site, notably `children_of`'s `unreachable!` arm below.
    ensure_node_admissible(kind, depth)?;

    let id = retained.as_ref().map(|r| r.id);
    let old_children = retained.map(|r| r.children).unwrap_or_default();
    let id = id.unwrap_or_else(|| scene.alloc_id());

    // Before the children, because a `text`'s measurement reads the `content` and `font_size`
    // parsed here rather than parsing them a second time.
    let paint = node::paint_style(kind, &properties)?;
    let measure = match flow_kind(kind, &properties)? {
        // `node::paint_style` gives every `text` a `PaintStyle::Text` and `flow_kind` cannot route
        // another kind here, so the arm is total, the same shape as `children_of`'s
        // `unreachable!`.
        "text" => {
            let Some(PaintStyle::Text { content, runs, font_size, wrap, max_lines, .. }) = paint.as_ref() else {
                unreachable!("a `text` node always carries a `PaintStyle::Text`")
            };
            Some(Measure::Text {
                content: content.clone(),
                runs: runs.clone(),
                font_size: *font_size,
                wrap: *wrap,
                max_lines: *max_lines,
            })
        }
        "icon" => Some(Measure::Square(node::parse_icon_size(&properties)?)),
        // `image` has no intrinsic size, unlike `icon`: knowing a file's own dimensions means
        // decoding it, and this pass has no canvas to decode against and runs on every
        // `Scene::apply`. So an `image` takes the box § 5.1's `width`/`height` give it, measuring
        // nothing without one, the same as an empty `rect`.
        _ => None,
    };

    // Before the children, so their ids attach afterwards, and so the `taffy::Style` behind it is
    // gone from the stack by the time this frame recurses (see `new_solver_node`).
    let taffy_id = new_solver_node(tree, kind, &properties, &style, parent_axis, measure)?;

    // A hidden node's subtree is frozen, not rebuilt (ADR-0124): the children it had keep their
    // ids, properties and last geometry, and none of their signals is read, no `list` item
    // function called, no text measured, until the node is visible again. `taffy_style` already
    // gave the node `Display::None`, so nothing below it could have reached the layout anyway,
    // and `paint`, `hit` and `overlay_input_regions` stop at a hidden node. Before this, a closed
    // picker of fifty tiles was rebuilt on every push of every capability, the clock's included.
    if !style.visible {
        return Ok(PreparedNode {
            id,
            kind: kind.to_string(),
            style,
            properties,
            paint,
            taffy: taffy_id,
            children: Vec::new(),
            frozen: old_children,
        });
    }

    let fresh_children = children_of(kind, &properties)?;
    let matched_candidates = pair_children_by_id_then_position(scene, &fresh_children, old_children)?;
    let own_axis = main_axis_of(kind, &properties)?;

    let mut children = Vec::with_capacity(fresh_children.len());
    for (fresh_child, candidate) in fresh_children.iter().zip(matched_candidates) {
        // Before this child's own getters run, not after: resolving its property map calls back
        // into Lua, and a child the walk is about to refuse must not execute anything on the way
        // to being refused. `depth + 1` is the level this child would occupy, so the error is the
        // same variant, kind and level the recursive call raises (see `ensure_node_admissible`).
        ensure_node_admissible(&fresh_child.kind, depth + 1)?;

        let reusable = match candidate {
            Some(c) if c.kind == fresh_child.kind => Some(c),
            Some(stale) => {
                scene.retire_child_first(stale);
                None
            }
            None => None,
        };

        // This child's one resolve and one parse for this pass, both here rather than inside the
        // recursive call, because the style the call is handed is built from them and a second
        // read of an impure `margin` could answer differently.
        let child_properties = node::resolve_properties(&fresh_child.properties, &fresh_child.kind, lua)?;
        let child_style = LayoutStyle::parse(&child_properties)?;
        children.push(prepare(
            scene,
            tree,
            reusable,
            &fresh_child.kind,
            child_properties,
            child_style,
            own_axis,
            lua,
            depth + 1,
        )?);
    }

    let child_ids: Vec<taffy::NodeId> = children.iter().map(|child| child.taffy).collect();
    tree.set_children(taffy_id, &child_ids).map_err(taffy_failed)?;

    Ok(PreparedNode {
        id,
        kind: kind.to_string(),
        style,
        properties,
        paint,
        taffy: taffy_id,
        children,
        frozen: Vec::new(),
    })
}

/// The second walk: solved geometry back out of the tree and into retained nodes.
///
/// taffy hands out a location relative to the parent's border box, the same frame `layout::paint`
/// and `layout::hit` already accumulate down, so the rect goes across untouched. Two things still
/// this module's own happen here, both needing a size that only exists once the solve is done: a
/// scrolled container shifts its children, and an over-wide `text` is cut to the box it ended up
/// in.
fn finish(
    tree: &taffy::TaffyTree<Measure>,
    prepared: PreparedNode,
    shaping: &ShapingHandle,
) -> Result<RetainedNode, LayoutError> {
    let PreparedNode { id, kind, style, properties, mut paint, taffy: taffy_id, children, frozen } = prepared;
    let layout = tree.layout(taffy_id).map_err(taffy_failed)?;
    let size = LogicalSize { width: layout.size.width, height: layout.size.height };

    // Frozen children come back as they were (see `prepare`): no scroll offset applied again to
    // rects that already carry one, no text refitted to a box that was not laid out.
    if !style.visible {
        return Ok(RetainedNode {
            id,
            kind,
            rect: LogicalRect { x: layout.location.x, y: layout.location.y, width: size.width, height: size.height },
            style,
            properties,
            paint,
            children: frozen,
        });
    }

    let mut children: Vec<RetainedNode> =
        children.into_iter().map(|child| finish(tree, child, shaping)).collect::<Result<_, _>>()?;

    // ADR-0069 decision 4. Subtracted from every child's main coordinate, so a scrolled child sits
    // before the content box and the clip `layout::paint` computes per node cuts it. Summed here
    // rather than read off taffy's `scrollable_overflow_rect`, since the two disagree: CSS
    // scrollable overflow is the union of the children's border boxes, while this engine's
    // `spacing`-and-margin footprint is what `Fill` was sized against, which the tests pin.
    if let Some(axis) = main_axis_of(&kind, &properties)? {
        let padding = style.padding;
        let (content_main, total_main) = match axis {
            MainAxis::Horizontal => (
                (size.width - padding.horizontal()).max(0.0),
                extent_along(&children, MainAxis::Horizontal, style.spacing),
            ),
            MainAxis::Vertical => (
                (size.height - padding.vertical()).max(0.0),
                extent_along(&children, MainAxis::Vertical, style.spacing),
            ),
        };
        let padding_start = match axis {
            MainAxis::Horizontal => padding.left,
            MainAxis::Vertical => padding.top,
        };
        reveal_child(&properties, &children, axis, padding_start, content_main);
        let offset = scroll_offset(&properties, content_main, total_main);
        if offset != 0.0 {
            for child in &mut children {
                match axis {
                    MainAxis::Horizontal => child.rect.x -= offset,
                    MainAxis::Vertical => child.rect.y -= offset,
                }
            }
        }
    }

    // After sizing, because the width it fits into is this node's own, and before the node is
    // built, because what it rewrites is the string the display list will carry.
    fit_text_to_box(&mut paint, (size.width - style.padding.horizontal()).max(0.0), shaping);

    Ok(RetainedNode {
        id,
        kind,
        rect: LogicalRect { x: layout.location.x, y: layout.location.y, width: size.width, height: size.height },
        style,
        properties,
        paint,
        children,
    })
}

/// How much room this container's visible children take along `axis`, margins and gaps included:
/// the number a scroll offset is clamped against. The same footprint the sizing pass used, which
/// keeps a scroll limit and the layout it scrolls in agreement.
fn extent_along(children: &[RetainedNode], axis: MainAxis, spacing: f32) -> f32 {
    let visible: Vec<&RetainedNode> = children.iter().filter(|c| c.style.visible).collect();
    let extents: f32 = visible
        .iter()
        .map(|c| {
            let extent = match axis {
                MainAxis::Horizontal => c.rect.width,
                MainAxis::Vertical => c.rect.height,
            };
            extent + c.style.margin_on(axis)
        })
        .sum();
    extents + spacing * visible.len().saturating_sub(1) as f32
}

/// taffy's own errors are all "you handed me a node id I do not have", which this module cannot do:
/// every id comes from the tree it is used against, and the tree lives no longer than the pass. So
/// this is the `unreachable!` equivalent for a `Result` that has to be handled anyway, reported as
/// a pass failure rather than a panic on the Wayland dispatch thread.
fn taffy_failed(err: taffy::TaffyError) -> LayoutError {
    node::invalid("layout", format!("the layout solver refused a node built by this pass: {err}"))
}

/// Runs taffy's own layout passes over the tree: sizes up, positions down, and the constraints
/// resolved between them. Given a prepared tree and the room its root has, fills in every node's
/// geometry.
///
/// The measure callback is the one place this crate is still asked a geometry question, only for
/// the two kinds whose size is their content: a `text`'s shaped extent and an `icon`'s square.
/// taffy asks each a handful of times per pass rather than once, over a small set of distinct
/// `(text, size, wrap width)` tuples, and `ShapingHandle`'s memo is keyed on exactly that tuple, so
/// every repeat after the first is answered without crossing the channel.
fn solve(
    tree: &mut taffy::TaffyTree<Measure>,
    root: taffy::NodeId,
    available: LogicalSize,
    shaping: &ShapingHandle,
) -> Result<(), LayoutError> {
    let space = taffy::Size {
        width: taffy::AvailableSpace::Definite(available.width),
        height: taffy::AvailableSpace::Definite(available.height),
    };
    tree.compute_layout_with_measure(root, space, |input, _node, context, style| {
        taffy::compute_leaf_layout(
            input,
            style,
            |_, _| 0.0,
            |known, offered| {
                let Some(measure) = context else {
                    return taffy::Size::ZERO;
                };
                match measure {
                    Measure::Square(size) => taffy::Size { width: *size, height: *size },
                    Measure::Text { content, runs, font_size, wrap, max_lines } => {
                        // The wrap boundary: the width this box is already known to have, or the
                        // width on offer when it is not. `MaxContent`/`MinContent` mean taffy is
                        // asking what the string wants rather than offering it a box, and an
                        // unconstrained measurement is the honest answer to that.
                        //
                        // `None` for a node that does not wrap, so it measures the one line it
                        // will paint. Passing the box width regardless is what this used to do,
                        // and it is why a fixed-width `text` reserved three lines of height to
                        // draw one clipped one.
                        let max_width = match wrap {
                            node::Wrap::None => None,
                            node::Wrap::Word => known.width.or(match offered.width {
                                taffy::AvailableSpace::Definite(width) => Some(width),
                                taffy::AvailableSpace::MinContent | taffy::AvailableSpace::MaxContent => None,
                            }),
                        };
                        let line_height = shaping::line_height(*font_size);
                        let shaped = shaping.shape(ShapeRequest {
                            text: content.clone(),
                            font_size: *font_size,
                            line_height,
                            max_width,
                            runs: node::font_runs(runs),
                        });
                        let lines = max_lines.map_or(shaped.lines.len(), |cap| shaped.lines.len().min(cap));
                        taffy::Size { width: shaped.width, height: lines as f32 * line_height }
                    }
                }
            },
        )
    })
    .map_err(taffy_failed)
}

/// The kind whose layout `kind` actually uses. Every kind is itself except `list`, which borrows a
/// `row`'s or a `column`'s arm depending on its `direction` (§ 5.2 item 7).
///
/// A `list` is a repeater, not a third layout: it reconciles children by key, then stacks them,
/// and "stacks them" is a `column` or a `row` and nothing else. Routing to the existing arms keeps
/// a horizontal list identical to a hand-built `row`, rather than a second implementation that
/// agrees with it until it does not.
fn flow_kind<'a>(kind: &'a str, properties: &HashMap<String, Value>) -> Result<&'a str, LayoutError> {
    if kind == "list" { node::parse_list_direction(properties) } else { Ok(kind) }
}

/// The `Signal` behind a node's `scroll` property, or `None` if it declares none.
///
/// Unresolved in the slot because `layout::node::is_structural_property` says so, the same way
/// `hover` arrives (ADR-0062 decision 3, ADR-0069 decision 4). Anything else there (a number, a
/// `state()` signal, a capability) is silently inert rather than an error, matching
/// `layout::hover::hover_signal`: `Signal::scroll_handle` refuses every kind this must not write,
/// so naming the wrong thing gets no scrolling instead of a wheel writing somewhere it should not.
pub(crate) fn scroll_signal_of(node: &ResolvedNode) -> Option<crate::lua::signal::Signal> {
    scroll_signal(&node.properties)
}

/// Which axis this container scrolls along, or `None` when it does not flow at all.
///
/// `pub(crate)` for `wayland::input`'s wheel handler, which has to know whether a node under the
/// pointer takes a horizontal or a vertical wheel before it writes anything. Reads a `list`'s
/// `direction` through [`main_axis_of`], so a horizontal `list` takes a horizontal wheel.
pub(crate) fn scrolling_axis(kind: &str, properties: &HashMap<String, Value>) -> Result<Option<MainAxis>, LayoutError> {
    main_axis_of(kind, properties)
}

fn scroll_signal(properties: &HashMap<String, Value>) -> Option<crate::lua::signal::Signal> {
    let Some(Value::UserData(ud)) = properties.get("scroll") else {
        return None;
    };
    crate::lua::signal::from_userdata(ud)
}

/// Honours a pending `signal:reveal(index)` on this container's scroll signal (ADR-0112): moves the
/// asked offset the least distance that puts the `index`-th visible child's border box inside the
/// viewport, or leaves it alone when the child is already in view. Written quietly, ahead of
/// [`scroll_offset`], which then clamps it like any wheel ask -- so a reveal past the end lands on
/// the end, and a reveal of a child that does not exist changes nothing. Children are still at
/// their unscrolled positions here, which is what makes `rect` minus the leading padding the
/// child's place in the content.
fn reveal_child(
    properties: &HashMap<String, Value>,
    children: &[RetainedNode],
    axis: MainAxis,
    padding_start: f32,
    content_main: f32,
) {
    let Some(signal) = scroll_signal(properties) else {
        return;
    };
    let Some(index) = signal.take_reveal() else {
        return;
    };
    let Some(child) = children.iter().filter(|c| c.style.visible).nth(index - 1) else {
        return;
    };
    let (start, extent) = match axis {
        MainAxis::Horizontal => (child.rect.x - padding_start, child.rect.width),
        MainAxis::Vertical => (child.rect.y - padding_start, child.rect.height),
    };
    let asked = signal.scroll_offset().unwrap_or(0.0);
    let wanted = if start < asked {
        start
    } else if start + extent > asked + content_main {
        start + extent - content_main
    } else {
        return;
    };
    if let Some(handle) = signal.scroll_handle() {
        handle.set_quiet(Value::Number(f64::from(wanted)));
    }
}

/// How far this container is scrolled along its main axis, clamped to what there is to scroll, and
/// written back so the signal holds the offset actually used (ADR-0069 decision 4).
///
/// `content_main` is the viewport and `total_main` the content, both already computed by the
/// caller for its own alignment arithmetic, so no new parameter is threaded through the recursion.
///
/// A container with nothing to scroll returns 0 rather than erroring, so a `Content`-sized column
/// (content and viewport the same number by construction) is a no-op, the same answer `Fill` gives
/// in a `Content` parent for the same reason: no remainder (decision 5).
fn scroll_offset(properties: &HashMap<String, Value>, content_main: f32, total_main: f32) -> f32 {
    let Some(signal) = scroll_signal(properties) else {
        return 0.0;
    };
    let Some(asked) = signal.scroll_offset() else {
        return 0.0;
    };
    let limit = (total_main - content_main).max(0.0);
    let used = asked.clamp(0.0, limit);
    if used != asked
        && let Some(handle) = signal.scroll_handle()
    {
        // Quiet: this number is derived from the geometry of the pass that is running, so marking
        // the scene dirty would schedule another pass to observe what this one already used.
        handle.set_quiet(Value::Number(f64::from(used)));
    }
    used
}

/// Rewrites a `text`'s content to what its box can actually show, which is the one thing paint
/// cannot work out for itself.
///
/// Runs here rather than in `layout::paint`: the box width isn't known until this node has been
/// sized, and the shaping worker isn't reachable from a display-list build, which is pure by
/// design. Does nothing for a run that neither wraps nor elides, and nothing on a `Content`-sized
/// node under `elide` alone, whose box came from measuring this same string and so always fits it.
fn fit_text_to_box(paint: &mut Option<PaintStyle>, content_width: f32, shaping: &ShapingHandle) {
    let Some(PaintStyle::Text { content, runs, font_size, elide, wrap, max_lines, .. }) = paint.as_mut() else {
        return;
    };
    if content.is_empty() || content_width <= 0.0 {
        return;
    }
    // The output is taken off the builder before the borrow of `content` ends, which is what lets
    // the same two fields be overwritten below.
    let fitted: Option<(String, Vec<StyleRun>)> = match wrap {
        // The measured-width check is the fast path, not politeness: most strings fit, and
        // skipping the binary search below is the difference on a list of them.
        node::Wrap::None => {
            if *elide == node::Elide::End && measured_width(content, runs, *font_size, shaping) > content_width {
                let mut fitted = Fitted::new(content, runs);
                let cut = elide_cut(content, runs, 0..content.len(), *font_size, content_width, shaping);
                fitted.push_source(0..cut);
                fitted.push_ellipsis(cut);
                Some((fitted.text, fitted.runs))
            } else {
                None
            }
        }
        node::Wrap::Word => {
            let fitted = wrapped_to_fit(content, runs, *font_size, *elide, *max_lines, content_width, shaping);
            Some((fitted.text, fitted.runs))
        }
    };
    if let Some((text, styled)) = fitted {
        *content = text;
        *runs = styled;
    }
}

/// A `text`'s content being rebuilt to fit its box, with its styled runs following it (ADR-0104).
///
/// Every rewrite here (joined lines, flattened remainder, ellipsis) used to edit `content` alone.
/// Runs are byte ranges into that string, so this is the one place that appends source slices and
/// re-bases the runs overlapping each slice.
struct Fitted<'s> {
    source: &'s str,
    source_runs: &'s [StyleRun],
    text: String,
    runs: Vec<StyleRun>,
}

impl<'s> Fitted<'s> {
    fn new(source: &'s str, source_runs: &'s [StyleRun]) -> Self {
        Self { source, source_runs, text: String::new(), runs: Vec::new() }
    }

    /// Appends `source[range]` and the parts of any run that fall inside it, re-based. Newlines in
    /// the slice become spaces when `flatten` is set: a remainder being collapsed onto an elided
    /// last line must not carry the paragraph breaks it spanned, and a space is the same width in
    /// bytes, so the runs need no adjustment for it.
    fn push_source_flattened(&mut self, range: Range<usize>, flatten: bool) {
        let at = self.text.len();
        let slice = &self.source[range.clone()];
        if flatten {
            self.text.extend(slice.chars().map(|c| if c == '\n' { ' ' } else { c }));
        } else {
            self.text.push_str(slice);
        }
        for run in self.source_runs {
            let start = run.range.start.max(range.start);
            let end = run.range.end.min(range.end);
            if start < end {
                self.runs.push(StyleRun { range: at + (start - range.start)..at + (end - range.start), ..run.clone() });
            }
        }
    }

    fn push_source(&mut self, range: Range<usize>) {
        self.push_source_flattened(range, false);
    }

    /// A plain separator, part of no run.
    fn push_plain(&mut self, text: &str) {
        self.text.push_str(text);
    }

    /// The ellipsis, in the style of the character it replaced (the run covering `cut`, if any,
    /// else the one just before it): a truncated bold sentence ends in a bold ellipsis.
    fn push_ellipsis(&mut self, cut: usize) {
        let at = self.text.len();
        self.text.push('\u{2026}');
        let style = self
            .source_runs
            .iter()
            .find(|run| run.range.contains(&cut))
            .or_else(|| self.source_runs.iter().rev().find(|run| run.range.end == cut && cut > 0));
        if let Some(run) = style {
            self.runs.push(StyleRun { range: at..self.text.len(), ..run.clone() });
        }
    }
}

/// `content` broken to `content_width` and capped to `max_lines`, joined by `\n` for
/// `text::atlas::TextPainter::draw_text` to walk, with its runs re-based to the result.
///
/// The cap and `elide` compose, which is the notification-body case: keep the lines allowed, and
/// if an ellipsis was asked for, rebuild the last one out of everything that did not fit so it
/// reads as truncated rather than as a sentence that happens to stop.
///
/// That remainder is the source from the last kept line's start to the end, with its paragraph
/// breaks flattened to spaces. It used to be the dropped lines' texts joined back with spaces; the
/// range form is what lets the runs follow, and differs only in keeping the source's own
/// whitespace at the breaks.
fn wrapped_to_fit<'s>(
    content: &'s str,
    runs: &'s [StyleRun],
    font_size: f32,
    elide: node::Elide,
    max_lines: Option<usize>,
    content_width: f32,
    shaping: &ShapingHandle,
) -> Fitted<'s> {
    let shaped = shaping.shape(ShapeRequest {
        text: content.to_string(),
        font_size,
        line_height: shaping::line_height(font_size),
        max_width: Some(content_width),
        runs: node::font_runs(runs),
    });
    let mut fitted = Fitted::new(content, runs);
    let Some(cap) = max_lines.filter(|cap| *cap < shaped.line_ranges.len()) else {
        for (index, range) in shaped.line_ranges.iter().enumerate() {
            if index > 0 {
                fitted.push_plain("\n");
            }
            fitted.push_source(range.clone());
        }
        return fitted;
    };

    // `cap` is at least 1: `parse_max_lines` maps 0 to no cap at all, so a `Some` cap standing
    // below a nonzero line count always leaves a line to rewrite.
    for range in &shaped.line_ranges[..cap - 1] {
        fitted.push_source(range.clone());
        fitted.push_plain("\n");
    }
    let last = &shaped.line_ranges[cap - 1];
    if elide == node::Elide::End {
        let rest = last.start..shaped.line_ranges.last().map_or(last.end, |range| range.end);
        let cut = elide_cut(content, runs, rest.clone(), font_size, content_width, shaping);
        fitted.push_source_flattened(rest.start..cut, true);
        fitted.push_ellipsis(cut);
    } else {
        fitted.push_source(last.clone());
    }
    fitted
}

/// One string's unconstrained width, the question `elide` is a search over.
fn measured_width(text: &str, runs: &[StyleRun], font_size: f32, shaping: &ShapingHandle) -> f32 {
    shaping
        .shape(ShapeRequest {
            text: text.to_string(),
            font_size,
            line_height: shaping::line_height(font_size),
            max_width: None,
            runs: node::font_runs(runs),
        })
        .width
}

/// Where to cut `region` of `text` so that what precedes the cut, plus an ellipsis, still fits
/// `width`: a byte offset within `region`, at a character boundary. `region.start` -- the ellipsis
/// alone -- is always admissible and is the honest answer for a box too narrow for one character.
///
/// Always ellipsizes, even for a `text` already narrow enough: the callers that want "leave it
/// alone if it fits" ask [`measured_width`] first, and the one that doesn't is truncating a
/// remainder, where the ellipsis is the whole point of the call. Each candidate is measured with
/// the runs it would carry, so a bold prefix is not cut where a regular one would fit.
///
/// ponytail: cuts at a character boundary rather than a grapheme cluster, the real ceiling: an
/// emoji with a skin-tone modifier can lose the modifier and change what it draws. Nothing in this
/// shell's own strings does that yet; window titles arriving from outside it eventually will.
/// Upgrade path: a `unicode-segmentation` pass over grapheme boundaries.
fn elide_cut(
    text: &str,
    runs: &[StyleRun],
    region: Range<usize>,
    font_size: f32,
    width: f32,
    shaping: &ShapingHandle,
) -> usize {
    let slice = &text[region.clone()];
    if slice.is_empty() {
        return region.start;
    }
    // Byte offsets a prefix may be cut at, so the search never lands inside a codepoint. The
    // last entry is the start of the final character, the longest prefix worth trying: appending
    // an ellipsis to the whole string is never narrower than the string.
    let cuts: Vec<usize> = slice.char_indices().map(|(index, _)| region.start + index).collect();
    let fits = |cut: usize| {
        let mut candidate = Fitted::new(text, runs);
        candidate.push_source_flattened(region.start..cut, true);
        candidate.push_ellipsis(cut);
        measured_width(&candidate.text, &candidate.runs, font_size, shaping) <= width
    };
    // Largest index whose prefix plus an ellipsis fits; zero is always admissible.
    let (mut low, mut high) = (0usize, cuts.len() - 1);
    while low < high {
        // Rounded up, so `mid` is always above `low` and the loop cannot stall; `high` is only ever
        // assigned `mid - 1`, and `mid` is at least 1 whenever this body runs.
        let mid = low + (high - low).div_ceil(2);
        if fits(cuts[mid]) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    cuts[low]
}

/// § 4's input-region scan: what this surface draws and what it can click, as surface-local
/// physical rects (ADR-0038 decision 5, ADR-0109). Pure; the `wl_region`/
/// `wl_surface::set_input_region` push it feeds lives in `crate::wayland::App::apply_input_region`,
/// the only place a Wayland object exists to push to.
///
/// A visible node claims its whole box when it is *solid*: it paints something (a box with a
/// background or a border, or any text, icon, image or field), or it is a `button` with a pointer
/// handler (`on_click`, `on_drag`, `on_wheel`), which is invisible by design but still pressable
/// (the panel host's click-outside catcher). Transparent containers claim nothing and are walked
/// into, so a full-surface `column` holding two cards yields the cards. Everything else is
/// click-through and, under focus-follows-mouse, focus-through; the popup's empty space below its
/// cards therefore takes neither clicks nor keyboard focus.
///
/// Applies to any surface whose visible content is smaller than the surface itself: empty space
/// (clicks pass through) with nothing visible, a no-op for a tightly-sized bar whose child fills
/// it.
pub fn overlay_input_regions(surface_root: &ResolvedNode, scale: f32) -> Vec<PhysicalRect> {
    let mut regions = Vec::new();
    for child in &surface_root.children {
        collect_input_regions(child, 0.0, 0.0, scale, &mut regions);
    }
    regions
}

fn collect_input_regions(node: &ResolvedNode, origin_x: f32, origin_y: f32, scale: f32, out: &mut Vec<PhysicalRect>) {
    if !node.visible {
        return;
    }
    let rect = LogicalRect { x: origin_x + node.rect.x, y: origin_y + node.rect.y, ..node.rect };
    if takes_input_as_a_box(node) {
        out.push(snap_to_physical(rect, scale));
        return;
    }
    for child in &node.children {
        collect_input_regions(child, rect.x, rect.y, scale, out);
    }
}

/// [`overlay_input_regions`]'s "solid" test. A `background` of `#00000000` counts: the IDL says it
/// draws a transparent rectangle where an absent one draws nothing, and a config that wrote it
/// asked for a box.
fn takes_input_as_a_box(node: &ResolvedNode) -> bool {
    let paints = match &node.paint {
        Some(PaintStyle::Box { background, widths, .. }) => {
            background.is_some() || [widths.top, widths.right, widths.bottom, widths.left].iter().any(|w| *w > 0.0)
        }
        Some(_) => true,
        None => false,
    };
    paints
        || (node.kind == "button"
            && ["on_click", "on_drag", "on_wheel"]
                .iter()
                .any(|handler| matches!(node.properties.get(*handler), Some(Value::Function(_)))))
}

#[cfg(test)]
mod flow_kind_tests {
    use super::*;

    fn props(lua: &mlua::Lua, direction: Option<&str>) -> HashMap<String, Value> {
        let mut properties = HashMap::new();
        if let Some(direction) = direction {
            properties.insert("direction".to_string(), Value::String(lua.create_string(direction).unwrap()));
        }
        properties
    }

    #[test]
    fn a_list_lays_out_as_a_column_unless_it_says_otherwise() {
        // The default is what every config written before `direction` existed relies on.
        let lua = mlua::Lua::new();
        assert_eq!(flow_kind("list", &props(&lua, None)).unwrap(), "column");
        assert_eq!(flow_kind("list", &props(&lua, Some("Vertical"))).unwrap(), "column");
    }

    #[test]
    fn a_horizontal_list_lays_out_as_a_row() {
        let lua = mlua::Lua::new();
        assert_eq!(flow_kind("list", &props(&lua, Some("Horizontal"))).unwrap(), "row");
    }

    #[test]
    fn direction_on_anything_that_is_not_a_list_is_ignored_rather_than_obeyed() {
        // `row` and `column` already say which way they go in their own name, so a `direction` on
        // one is a config confusing itself, not a second way to spell the kind.
        let lua = mlua::Lua::new();
        assert_eq!(flow_kind("column", &props(&lua, Some("Horizontal"))).unwrap(), "column");
        assert_eq!(flow_kind("row", &props(&lua, Some("Vertical"))).unwrap(), "row");
    }

    #[test]
    fn an_unknown_direction_is_refused_by_name() {
        let lua = mlua::Lua::new();
        let err = flow_kind("list", &props(&lua, Some("sideways"))).unwrap_err().to_string();
        assert!(err.contains("sideways"), "the message has to name what was written: {err}");
        assert!(err.contains("Horizontal"), "and what was expected: {err}");
    }
}

#[cfg(test)]
pub(super) mod tests {
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

    pub(super) fn full() -> LogicalSize {
        LogicalSize { width: 1000.0, height: 500.0 }
    }

    /// The single-output shorthand every fixture below uses: one instance per declared surface,
    /// against one output named `"TEST"`, so a fixture declaring `id = "bar"` reads back as
    /// `scene.surface("bar@TEST")`. Deliberately not `layout::instance::expand_instances` -- that
    /// function takes `SurfaceSpec`s, whose `panel` arm requires a `layer`, and these fixtures
    /// test layout rather than topology; `expand_instances` has its own direct tests in
    /// `layout::instance`.
    pub(super) fn apply_at(
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

    /// Two outputs, `"LEFT"` and `"RIGHT"`, for the per-output child fixtures (ADR-0121).
    fn apply_on_two_outputs(
        scene: &mut Scene,
        surface: &VirtualNode,
        shaping: &ShapingHandle,
        lua: &Lua,
    ) -> Result<(), LayoutError> {
        let declared_id = node::parse_surface_id(&surface.properties).expect("the fixture declares an `id`");
        let instances: Vec<SurfaceInstance> = ["LEFT", "RIGHT"]
            .into_iter()
            .map(|output| SurfaceInstance {
                instance_id: format!("{declared_id}@{output}"),
                declared_id: declared_id.clone(),
                output: output.to_string(),
                available: full(),
            })
            .collect();
        scene.apply(std::slice::from_ref(surface), &instances, shaping, lua)
    }

    #[test]
    fn a_function_child_is_built_once_per_output_with_that_outputs_name() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"return panel {
                    id = "wall",
                    child = function(output)
                        return rect { width = output == "LEFT" and 10 or 20, height = 5 }
                    end,
                }"#,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        apply_on_two_outputs(&mut scene, &surface, &shaping, &lua).unwrap();

        assert_eq!(scene.surface("wall@LEFT").unwrap().children[0].rect.width, 10.0);
        assert_eq!(scene.surface("wall@RIGHT").unwrap().children[0].rect.width, 20.0);
    }

    #[test]
    fn a_function_child_returning_nil_maps_the_instance_empty() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"return panel {
                    id = "wall",
                    child = function(output)
                        if output == "LEFT" then return rect { width = 10, height = 5 } end
                    end,
                }"#,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        apply_on_two_outputs(&mut scene, &surface, &shaping, &lua).unwrap();

        assert_eq!(scene.surface("wall@LEFT").unwrap().children.len(), 1);
        assert!(scene.surface("wall@RIGHT").unwrap().children.is_empty());
    }

    #[test]
    fn a_function_child_on_a_window_is_refused_and_a_non_node_return_names_child() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table =
            lua.load(r#"return window { id = "w", child = function() return rect {} end }"#).eval().unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err().to_string();
        assert!(err.contains("child") && err.contains("window"), "{err}");

        let table: mlua::Table =
            lua.load(r#"return panel { id = "p", child = function() return 4 end }"#).eval().unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err().to_string();
        assert!(err.contains("child") && err.contains("node table"), "{err}");
    }

    #[test]
    fn a_signal_valued_width_resolves_to_its_current_value_in_the_resolved_node() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Integer(40), crate::lua::signal::DirtyFlag::new()).0;
        lua.globals().set("w", signal).unwrap();
        let table: mlua::Table =
            lua.load(r#"return panel { id = "bar", child = rect { width = w, height = 20 } }"#).eval().unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        let child = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(child.rect.width, 40.0, "a Signal-valued width must resolve at layout time");
    }

    #[test]
    fn a_state_signal_in_a_property_resolves_at_layout_time_and_a_set_between_applies_moves_it() {
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
    fn a_hidden_subtree_is_frozen_rather_than_rebuilt_and_thaws_with_its_ids() {
        // The closed picker case (ADR-0124): while `visible` is false no item function runs and
        // the retained children stay, ids included, so showing it again pairs against them.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                built = 0
                return panel { id = "bar", child = column { visible = state("open", true),
                    children = { list { source = state("items", { "a", "b" }), itemfn = function(name)
                        built = built + 1
                        return rect { width = 10, height = 10 }
                    end } } } }"#,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let built = || lua.globals().get::<i64>("built").unwrap();

        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let shown = scene.surface("bar@TEST").unwrap();
        let ids_before: Vec<NodeId> = shown.children[0].children[0].children.iter().map(|c| c.id).collect();
        assert_eq!(ids_before.len(), 2);
        assert_eq!(built(), 2);

        lua.load(r#"state("open", true):set(false)"#).exec().unwrap();
        apply_at(&mut scene, std::slice::from_ref(&surface), full(), &shaping, &lua).unwrap();
        let hidden = scene.surface("bar@TEST").unwrap();
        assert!(!hidden.children[0].visible);
        let frozen: Vec<NodeId> = hidden.children[0].children[0].children.iter().map(|c| c.id).collect();
        assert_eq!(frozen, ids_before, "the hidden subtree keeps what it had");
        assert_eq!(built(), 2, "no item function ran for a hidden list");
        assert!(scene.retiring_ids().is_empty(), "nothing was retired");

        lua.load(r#"state("open", true):set(true)"#).exec().unwrap();
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let thawed = scene.surface("bar@TEST").unwrap();
        let ids_after: Vec<NodeId> = thawed.children[0].children[0].children.iter().map(|c| c.id).collect();
        assert_eq!(ids_after, ids_before, "showing it again pairs the fresh items with the frozen nodes");
        assert_eq!(built(), 4);
        assert_eq!(thawed.children[0].children[0].children[1].rect.y, 10.0, "and lays them out again");
    }

    #[test]
    fn a_pixels_sized_rect_resolves_to_its_explicit_size() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(r#"panel { id = "bar", child = rect { width = 40, height = 20 } }"#);
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
        let (_lua, surface) =
            surface_from(r#"panel { id = "bar", width = 1000, height = 500, child = rect { width = "50%" } }"#);
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let child = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(child.rect.width, 500.0);
    }

    #[test]
    fn row_intrinsic_width_sums_children_plus_spacing_gaps() {
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
        assert_eq!(row.children[1].rect.x, 15.0, "10 (first child) + 5 (its margin.right) = 15, not overlapping at 10");
    }

    #[test]
    fn stretching_a_child_that_itself_has_children_repositions_its_descendants_too() {
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

    /// A `Stretch` child of a `Content`-sized row: the solver closes this, not anything written
    /// here (ADR-0023).
    ///
    /// Same shape as the test above, minus the row's `height = "Fill"`, which is the whole
    /// difference: the row is now `Content`-sized, so its own height is not known until after its
    /// children resolve. The hand-written pass could not pre-force a `Stretch` child's size in that
    /// case, so it patched the child's own `rect` afterwards and left the child's descendants
    /// positioned against the pre-stretch height -- the grandchild centred in 6 rather than in 50.
    /// ADR-0023's upgrade path (g) named "a second constraint pass" as the fix. That is what a
    /// solver is.
    #[test]
    fn a_stretch_child_of_a_content_sized_row_repositions_its_descendants_too() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", width = 100, height = 50, child = row { children = {
                rect { width = 20, height = 50 },
                rect { width = 20, align_v = "Stretch", children = {
                    rect { width = 6, height = 6, align_h = "Center", align_v = "Center" },
                } },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.rect.height, 50.0, "the row measures itself from the fixed sibling");
        let stretched = &row.children[1];
        assert_eq!(stretched.rect.height, 50.0, "and the stretched sibling takes that whole height");
        assert_eq!(
            stretched.children[0].rect.y,
            (50.0 - 6.0) / 2.0,
            "centered against the stretched height, which the old stacking model did not do"
        );
    }

    /// The one thing the solver swap narrows, pinned so it stays a decision rather than a surprise.
    ///
    /// An invisible node leaves the layout entirely, so its whole subtree resolves to zero geometry
    /// instead of being sized and then declined a position. Nothing outside this module can tell:
    /// `layout::paint`, `layout::hit` and `overlay_input_regions` all filter on `visible` before
    /// they read a rect, and the two readers that do look at a hidden node -- `layout::hover`, and
    /// `wayland::surface`'s `panel_spec` re-derive -- read `properties`, which is still here in
    /// full. What was always specified is the part that still holds: a hidden child reserves no
    /// space and no `spacing` gap, which the two tests above this pin.
    #[test]
    fn an_invisible_subtree_resolves_to_no_geometry_at_all() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", width = 100, height = 50, child = rect { width = 40, height = 30,
                visible = false, children = { rect { width = 10, height = 10 } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let hidden = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!((hidden.rect.width, hidden.rect.height), (0.0, 0.0), "the hidden node itself");
        // A subtree hidden from the start was never built (ADR-0124): there is nothing under it
        // to have geometry until it is shown.
        assert!(hidden.children.is_empty(), "and nothing under it yet");
        assert_eq!(
            hidden.properties.get("width").map(|w| w.to_string().unwrap()),
            Some("40".to_string()),
            "its properties survive in full, which is what `panel_spec` and `hover` read"
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
        assert_eq!(row.rect.width, 10.0, "the invisible child must not widen the row or add a spacing gap");
        assert_eq!(
            row.children[1].rect.x, 0.0,
            "the visible child packs at the start as if the hidden one weren't there"
        );
    }

    /// The measured defect this pass fixes. `width = "Fill"` used to resolve against the parent's
    /// whole content width, per child, with no knowledge of siblings: in a 600px row a `Fill` child
    /// took 600 and its fixed sibling was then placed at x=600, outside the row it belonged to.
    #[test]
    fn a_fill_child_takes_only_the_room_its_siblings_leave() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { width = 600, height = 40, children = {
                rect { width = "Fill", height = 10 },
                rect { width = 100, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.width, 500.0, "the fill child takes 600 less its sibling's 100");
        assert_eq!(row.children[1].rect.x, 500.0, "and its sibling lands inside the row, not past its edge");
    }

    #[test]
    fn two_fill_siblings_split_the_remainder_equally() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { width = 600, height = 40, children = {
                rect { width = "Fill", height = 10 },
                rect { width = "Fill", height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!((row.children[0].rect.width, row.children[1].rect.width), (300.0, 300.0));
        assert_eq!(row.children[1].rect.x, 300.0);
    }

    /// A share is a footprint, and the positioning advances by a footprint including
    /// margin. Forcing the share as the child's *size* instead would push every later sibling out
    /// by exactly the margin.
    #[test]
    fn a_fill_childs_margin_comes_out_of_its_own_share() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { width = 600, height = 40, children = {
                rect { width = "Fill", height = 10, margin = 25 },
                rect { width = 100, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.width, 450.0, "500 of footprint less 25 of margin on each side");
        assert_eq!(row.children[1].rect.x, 500.0, "so the sibling still starts one footprint in");
    }

    /// The mirror of `a_fill_childs_margin_comes_out_of_its_own_share`, and the case that test
    /// missed: the margin on the *sibling* rather than on the `Fill` child. The positioning
    /// advances its cursor by a footprint that includes margin, so a remainder that counts only the
    /// sibling's box hands the `Fill` child exactly that margin too much and pushes it off the end.
    #[test]
    fn a_fixed_siblings_margin_is_counted_against_the_remainder() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { width = 600, height = 40, children = {
                rect { width = 100, height = 10, margin = 20 },
                rect { width = "Fill", height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        let fill = &row.children[1];
        assert_eq!(fill.rect.width, 460.0, "600 less the sibling's 100 box and its 40 of margin");
        assert_eq!(
            fill.rect.x + fill.rect.width,
            row.rect.width,
            "and the fill child ends exactly at the row's edge, not past it"
        );
    }

    #[test]
    fn spacing_is_reserved_before_a_fill_child_is_sized() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { width = 600, height = 40, spacing = 20, children = {
                rect { width = "Fill", height = 10 },
                rect { width = 100, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.width, 480.0, "600 less the sibling's 100 and the one 20px gap");
        assert_eq!(row.children[1].rect.x, 500.0);
    }

    #[test]
    fn a_column_fills_its_main_axis_the_same_way_a_row_does() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = column { width = 200, height = 600, children = {
                rect { height = "Fill", width = 10 },
                rect { height = 100, width = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let column = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(column.children[0].rect.height, 500.0);
        assert_eq!(column.children[1].rect.y, 500.0);
    }

    /// Clamped rather than negative, and the overflow stays visible. This is flexbox without
    /// `flex-shrink`: the engine will not silently shrink a size the config stated in pixels to
    /// make a `Fill` sibling fit.
    #[test]
    fn fixed_children_that_already_overflow_collapse_a_fill_sibling_to_nothing() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { width = 100, height = 40, children = {
                rect { width = "Fill", height = 10 },
                rect { width = 300, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.width, 0.0);
        assert_eq!(row.children[1].rect.width, 300.0, "the stated size is kept, not shrunk to fit");
    }

    /// An invisible sibling takes no space when positioned, so it must reserve none here
    /// either -- otherwise hiding a node would shrink the one beside it.
    #[test]
    fn an_invisible_sibling_reserves_nothing_from_a_fill_childs_share() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        for hidden in [
            r#"rect { width = 100, height = 10, visible = false }"#,
            r#"rect { width = "Fill", height = 10, visible = false }"#,
        ] {
            let mut scene_for_case = std::mem::replace(&mut scene, Scene::new());
            let (_lua, surface) = surface_from(&format!(
                r#"panel {{ id = "bar", child = row {{ width = 600, height = 40, children = {{
                    rect {{ width = "Fill", height = 10 }}, {hidden},
                }} }} }}"#
            ));
            apply_at(&mut scene_for_case, &[surface], full(), &shaping, &_lua).unwrap();
            let row = &scene_for_case.surface("bar@TEST").unwrap().children[0];
            assert_eq!(row.children[0].rect.width, 600.0, "the visible fill child takes the whole row");
        }
    }

    /// Correct before this pass and pinned so it stays that way: a row's *cross* axis hands every
    /// child the row's full height, because on that axis there is nothing to share.
    #[test]
    fn fill_on_a_rows_cross_axis_is_still_the_whole_row() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { width = 600, height = 200, children = {
                rect { width = 100, height = "Fill" },
                rect { width = 100, height = 50 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.height, 200.0);
    }

    /// Also correct before this pass and pinned: a stacking parent has no main axis, its children
    /// may overlap by design (ADR-0023), and `Fill` there means the whole box.
    #[test]
    fn fill_under_a_stacking_parent_is_still_the_whole_box() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = rect { width = 600, height = 200, children = {
                rect { width = "Fill", height = 10 },
                rect { width = "Fill", height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let stack = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!((stack.children[0].rect.width, stack.children[1].rect.width), (600.0, 600.0));
        assert_eq!(stack.children[1].rect.x, 0.0, "stacked, not flowed");
    }

    /// A percentage still resolves against the parent, not against the remainder, which is what a
    /// CSS percentage width does. Deliberately left alone by this pass: changing it would be a
    /// second behaviour change riding along inside a bug fix.
    #[test]
    fn a_percentage_sibling_still_resolves_against_the_parent() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { width = 600, height = 40, children = {
                rect { width = "Fill", height = 10 },
                rect { width = "50%", height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[1].rect.width, 300.0, "half of the parent, not half of what is left");
        assert_eq!(row.children[0].rect.width, 300.0, "and the fill child takes what that leaves");
    }

    /// Unchanged, and deliberate: a row that states no width has no remainder to divide, so a
    /// `Fill` child of it resolves to zero. See ADR-0077's own note on item 10
    /// for the one-pass reasoning behind it and the upgrade path.
    #[test]
    fn a_fill_child_of_a_content_sized_row_still_resolves_to_zero() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { height = 40, children = {
                rect { width = "Fill", height = 10 },
                rect { width = 100, height = 10 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.width, 0.0);
    }

    /// ADR-0069. Scrolling moves children within a viewport the clip already cuts them to.
    fn scrolled(lua_src: &str, offset: f32) -> (mlua::Lua, Vec<f32>, f32) {
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua.load(lua_src).eval().unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let signal: mlua::AnyUserData = lua.load(r#"return scroll("s")"#).eval().unwrap();
        let signal = crate::lua::signal::from_userdata(&signal).unwrap();
        signal.scroll_handle().unwrap().set_changed(mlua::Value::Number(f64::from(offset)));

        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let container = &scene.surface("bar@TEST").unwrap().children[0];
        let ys = container.children.iter().map(|c| c.rect.y).collect();
        (lua, ys, signal.scroll_offset().unwrap())
    }

    const SCROLLED_COLUMN: &str = r#"panel { id = "bar", child = column { width = 100, height = 100, scroll = scroll("s"), children = {
        rect { width = 10, height = 100 }, rect { width = 10, height = 100 }, rect { width = 10, height = 100 },
    } } }"#;

    fn revealed(index: usize, offset: f32) -> (Vec<f32>, f32) {
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua.load(SCROLLED_COLUMN).eval().unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let signal: mlua::AnyUserData = lua.load(r#"return scroll("s")"#).eval().unwrap();
        let signal = crate::lua::signal::from_userdata(&signal).unwrap();
        signal.scroll_handle().unwrap().set_changed(mlua::Value::Number(f64::from(offset)));
        assert!(signal.request_reveal(index));

        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let container = &scene.surface("bar@TEST").unwrap().children[0];
        let ys = container.children.iter().map(|c| c.rect.y).collect();
        assert!(signal.take_reveal().is_none(), "the pass consumed the ask");
        (ys, signal.scroll_offset().unwrap())
    }

    /// ADR-0112: `signal:reveal(index)` moves the least distance that shows the child, and only
    /// when it is out of view -- so a keyboard selection walking down a list scrolls it a row at a
    /// time and a selection already on screen leaves the wheel's offset alone.
    #[test]
    fn a_reveal_scrolls_the_least_distance_that_shows_the_child() {
        let (ys, used) = revealed(3, 0.0);
        assert_eq!(used, 200.0, "the third 100px child in a 100px viewport: its bottom lands on the viewport's");
        assert_eq!(ys, vec![-200.0, -100.0, 0.0]);

        let (_, used) = revealed(1, 150.0);
        assert_eq!(used, 0.0, "revealing upward puts the child's top at the viewport's top");

        let (_, used) = revealed(2, 100.0);
        assert_eq!(used, 100.0, "a child already in view moves nothing");

        let (_, used) = revealed(9, 20.0);
        assert_eq!(used, 20.0, "a child that does not exist reveals nothing");
    }

    #[test]
    fn a_scroll_offset_moves_children_up_within_the_viewport() {
        let (_lua, ys, used) = scrolled(SCROLLED_COLUMN, 120.0);
        assert_eq!(ys, vec![-120.0, -20.0, 80.0], "every child shifts by the offset, first one out of the box");
        assert_eq!(used, 120.0, "an in-range offset is used as asked");
    }

    #[test]
    fn an_unscrolled_container_places_children_exactly_as_before() {
        let (_lua, ys, _) = scrolled(SCROLLED_COLUMN, 0.0);
        assert_eq!(ys, vec![0.0, 100.0, 200.0]);
    }

    /// The bound is content minus viewport: 300 of children in a 100 box leaves 200 to scroll.
    #[test]
    fn an_offset_past_the_end_is_clamped_and_written_back() {
        let (_lua, ys, used) = scrolled(SCROLLED_COLUMN, 5_000.0);
        assert_eq!(used, 200.0, "the signal holds what was used, not what the wheel asked for");
        assert_eq!(ys, vec![-200.0, -100.0, 0.0], "so the last child sits at the top and nothing scrolls past it");
    }

    #[test]
    fn a_negative_offset_is_clamped_to_the_top() {
        let (_lua, ys, used) = scrolled(SCROLLED_COLUMN, -50.0);
        assert_eq!(used, 0.0);
        assert_eq!(ys[0], 0.0);
    }

    /// Decision 5: a container whose content and viewport are the same number by construction.
    #[test]
    fn a_content_sized_container_has_nothing_to_scroll() {
        let (_lua, ys, used) = scrolled(
            r#"panel { id = "bar", child = column { width = 100, scroll = scroll("s"), children = {
                rect { width = 10, height = 100 }, rect { width = 10, height = 100 },
            } } }"#,
            80.0,
        );
        assert_eq!(used, 0.0, "no remainder, so the offset is clamped away rather than erroring");
        assert_eq!(ys, vec![0.0, 100.0]);
    }

    /// `max_height` is what makes a content-sized container scrollable: below the cap it is exactly
    /// its children, at the cap it stops and the rest is remainder.
    #[test]
    fn a_max_height_caps_a_content_sized_column_and_leaves_the_rest_to_scroll() {
        let capped = r#"panel { id = "bar", child = column { width = 100, max_height = 150, scroll = scroll("s"), children = {
            rect { width = 10, height = 100 }, rect { width = 10, height = 100 }, rect { width = 10, height = 100 },
        } } }"#;
        let (lua, ys, used) = scrolled(capped, 5_000.0);
        assert_eq!(used, 150.0, "300 of children in a box capped at 150 leaves 150 to scroll");
        assert_eq!(ys, vec![-150.0, -50.0, 50.0]);
        drop(lua);

        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = column { width = 100, max_height = 150, children = {
                rect { width = 10, height = 40 }, rect { width = 10, height = 40 },
            } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let column = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(column.rect.height, 80.0, "under the cap the box is the content, as with no cap at all");
    }

    /// Spacing counts toward the content extent, because a gap advances the cursor by
    /// it. A bound computed without it would let the list scroll one gap short of its end.
    #[test]
    fn spacing_counts_toward_what_there_is_to_scroll() {
        let (_lua, _ys, used) = scrolled(
            r#"panel { id = "bar", child = column { width = 100, height = 100, spacing = 10, scroll = scroll("s"), children = {
                rect { width = 10, height = 100 }, rect { width = 10, height = 100 },
            } } }"#,
            9_999.0,
        );
        assert_eq!(used, 110.0, "200 of children plus one 10px gap, less the 100 viewport");
    }

    #[test]
    fn a_row_scrolls_horizontally() {
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"return panel { id = "bar", child = row { width = 100, height = 50, scroll = scroll("s"), children = {
                    rect { width = 100, height = 10 }, rect { width = 100, height = 10 },
                } } }"#,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let signal: mlua::AnyUserData = lua.load(r#"return scroll("s")"#).eval().unwrap();
        let signal = crate::lua::signal::from_userdata(&signal).unwrap();
        signal.scroll_handle().unwrap().set_changed(mlua::Value::Number(60.0));
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.x, -60.0, "a row takes the offset on x, not y");
        assert_eq!(row.children[0].rect.y, 0.0);
    }

    /// A wheel must not be able to write a capability snapshot. `scroll_handle` refuses every kind
    /// but its own, so binding the wrong signal scrolls nothing instead.
    /// Alignment and scrolling cannot both be in play; this pins why rather than trusting it.
    ///
    /// `spare` is `(content - total).max(0)` and the scroll limit is `(total - content).max(0)`, so
    /// one is zero whenever the other is not. Content that underfills its box aligns and cannot
    /// scroll; overflowing content has no spare to align with. A `Center` column with a scroll
    /// offset is therefore still centred, not centred-then-shifted.
    #[test]
    fn alignment_and_scrolling_are_mutually_exclusive_by_construction() {
        let (_lua, ys, used) = scrolled(
            r#"panel { id = "bar", child = column { width = 100, height = 300, align_v = "Center", scroll = scroll("s"), children = {
                rect { width = 10, height = 50 }, rect { width = 10, height = 50 },
            } } }"#,
            999.0,
        );
        assert_eq!(used, 0.0, "100 of content in a 300 box leaves nothing to scroll");
        assert_eq!(ys, vec![100.0, 150.0], "so the pair stays centred rather than being dragged off the top");
    }

    #[test]
    fn a_scroll_property_naming_something_that_is_not_a_scroll_signal_is_inert() {
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"return panel { id = "bar", child = column { width = 100, height = 100, scroll = state("s", 120), children = {
                    rect { width = 10, height = 100 }, rect { width = 10, height = 100 },
                } } }"#,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let column = &scene.surface("bar@TEST").unwrap().children[0];
        let ys: Vec<f32> = column.children.iter().map(|c| c.rect.y).collect();
        assert_eq!(ys, vec![0.0, 100.0], "a `state()` signal holding 120 scrolls nothing");
    }

    /// Found live against `dev-config`: a content-sized `column` with 8px of padding reported the
    /// bare height of its one child, and reported the same height with 50px of padding. Padding
    /// insets the box children are laid out in (they are offset by exactly this
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
        assert_eq!(column.children[0].rect.x, 10.0);
        assert_eq!(column.children[0].rect.y, 8.0);
    }

    /// The surface root resolves through the same `unwrap_or`, and a popup sized to its contents
    /// is the case this actually bites.
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
        let (_lua, surface) = surface_from(r#"panel { id = "bar", child = text { content = "Oblisk" } }"#);
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let text = &scene.surface("bar@TEST").unwrap().children[0];
        assert!(text.rect.width > 0.0);
        assert_eq!(text.rect.height, 12.0 * 1.2, "default font_size 12 * the 1.2 line-height multiplier");
    }

    #[test]
    fn an_unsupported_top_level_kind_is_rejected() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
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

    /// The coverage ADR-0068 widened, pinned so it stays deliberate. `layout::paint::build_node`
    /// returned before any parser on an invisible node, so this config used to boot fine and fail
    /// only once something made the node visible.
    /// ADR-0068's rule applied to the new property: a bad value fails the apply rather than being
    /// clamped or defaulted, so `opacity = 50` meaning percent is heard about immediately.
    fn drawn_text(scene: &Scene) -> String {
        drawn_text_and_runs(scene).0
    }

    fn drawn_text_and_runs(scene: &Scene) -> (String, Vec<StyleRun>) {
        fn find(node: &ResolvedNode) -> Option<(String, Vec<StyleRun>)> {
            if let Some(PaintStyle::Text { content, runs, .. }) = &node.paint {
                return Some((content.clone(), runs.clone()));
            }
            node.children.iter().find_map(find)
        }
        find(&scene.surface("bar@TEST").unwrap()).expect("expected a text node")
    }

    // ---- styled runs following a wrap or an elide (ADR-0104) ----

    fn styled(lua_src: &str) -> (String, Vec<StyleRun>) {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(lua_src);
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        drawn_text_and_runs(&scene)
    }

    /// A run that spans a wrap break is split across the two lines it lands on, and every range
    /// still slices the *fitted* string to the text it styled.
    #[test]
    fn a_styled_run_follows_its_text_across_a_wrap() {
        let (content, runs) = styled(
            r##"panel { id = "bar", child = text { width = 90, font_size = 14, wrap = "Word", content = {
                { text = "plain " }, { text = "bold words that wrap", bold = true }, { text = " tail" },
            } } }"##,
        );
        assert!(content.contains('\n'), "the string must have wrapped for this to test anything: {content:?}");
        let styled_text: String = runs.iter().map(|run| &content[run.range.clone()]).collect::<Vec<_>>().join("|");
        // The run's own words, in order, with the break's newline now outside them.
        let rejoined = styled_text.replace('|', " ").replace("  ", " ");
        assert_eq!(rejoined.trim(), "bold words that wrap".trim_end(), "runs: {styled_text:?} in {content:?}");
        assert!(runs.iter().all(|run| run.bold));
        assert!(runs.iter().all(|run| !content[run.range.clone()].contains('\n')), "a run never spans a break");
    }

    #[test]
    fn an_elided_styled_text_ends_in_an_ellipsis_of_the_same_style() {
        let (content, runs) = styled(
            r##"panel { id = "bar", child = text { width = 60, font_size = 14, elide = "End", content = {
                { text = "Alice: ", bold = true, color = "#ff0000" }, { text = "a long message that will not fit" },
            } } }"##,
        );
        assert!(content.ends_with('\u{2026}'));
        // "Alice: " is 7 bytes; if the cut fell inside it the ellipsis inherits its style.
        let cut = content.len() - '\u{2026}'.len_utf8();
        let last = runs.last().expect("the bold prefix survives at least in part");
        if cut <= 7 {
            assert_eq!(last.range.end, content.len(), "the ellipsis is inside the bold run");
            assert!(last.bold);
        } else {
            assert_eq!(&content[runs[0].range.clone()], "Alice: ");
        }
    }

    #[test]
    fn a_plain_string_content_carries_no_runs() {
        let (content, runs) = styled(r##"panel { id = "bar", child = text { width = 60, content = "hello" } }"##);
        assert_eq!((content.as_str(), runs.len()), ("hello", 0));
    }

    fn elided(lua_src: &str) -> String {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(lua_src);
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        drawn_text(&scene)
    }

    const LONG: &str = "a window title far too long for the box it was given";

    /// The height half of the same round trip, and the behaviour change wrapping brought with it:
    /// a fixed-width `text` that does not ask to wrap now *measures* the one line it paints.
    /// Before, it measured every line cosmic-text would have broken the string onto and painted
    /// one clipped run into a box several times too tall.
    fn text_box(lua_src: &str) -> (String, f32) {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(lua_src);
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        let node = &scene.surface("bar@TEST").unwrap().children[0];
        (drawn_text(&scene), node.rect.height)
    }

    #[test]
    fn a_narrow_text_that_does_not_wrap_is_one_line_tall() {
        let (drawn, height) =
            text_box(&format!(r#"panel {{ id = "bar", child = text {{ width = 80, content = "{LONG}" }} }}"#));
        assert_eq!(height, 12.0 * 1.2, "an unwrapped run occupies one line however long it is");
        assert!(!drawn.contains('\n'), "nothing broke it: {drawn:?}");
    }

    #[test]
    fn a_narrow_text_that_wraps_is_broken_into_lines_and_measured_at_their_height() {
        let (drawn, height) = text_box(&format!(
            r#"panel {{ id = "bar", child = text {{ width = 80, content = "{LONG}", wrap = "Word" }} }}"#
        ));
        let lines: Vec<&str> = drawn.lines().collect();
        assert!(lines.len() > 1, "80px cannot hold {LONG:?} on one line, got {drawn:?}");
        assert_eq!(height, lines.len() as f32 * 12.0 * 1.2, "the box has to be as tall as the lines it holds");
        // Whitespace is where the breaks landed, so the words survive and only the gaps moved.
        assert_eq!(drawn.split_whitespace().collect::<Vec<_>>(), LONG.split_whitespace().collect::<Vec<_>>());
    }

    #[test]
    fn max_lines_caps_both_what_is_drawn_and_the_height_reserved_for_it() {
        let (drawn, height) = text_box(&format!(
            r#"panel {{ id = "bar", child = text {{ width = 80, content = "{LONG}", wrap = "Word", max_lines = 2 }} }}"#
        ));
        assert_eq!(drawn.lines().count(), 2, "got {drawn:?}");
        assert_eq!(height, 2.0 * 12.0 * 1.2, "a capped run reserves the lines it keeps, not the ones it dropped");
    }

    /// The notification-body case: fill the lines allowed, then say the rest was dropped. The
    /// ellipsis has to land on the last kept line and nowhere else.
    #[test]
    fn a_capped_wrap_that_elides_finishes_its_last_line_with_an_ellipsis() {
        let (drawn, _) = text_box(&format!(
            r#"panel {{ id = "bar", child = text {{ width = 80, content = "{LONG}", wrap = "Word", max_lines = 2, elide = "End" }} }}"#
        ));
        let lines: Vec<&str> = drawn.lines().collect();
        assert_eq!(lines.len(), 2, "got {drawn:?}");
        assert!(lines[1].ends_with('\u{2026}'), "the last kept line must be ellipsized: {drawn:?}");
        assert!(!lines[0].ends_with('\u{2026}'), "no earlier line may be: {drawn:?}");
    }

    /// A cap the text never reaches changes nothing, so a config can set `max_lines`
    /// unconditionally and let the content decide.
    #[test]
    fn a_max_lines_above_the_line_count_leaves_the_run_alone() {
        let (drawn, _) = text_box(&format!(
            r#"panel {{ id = "bar", child = text {{ width = 80, content = "{LONG}", wrap = "Word", max_lines = 40, elide = "End" }} }}"#
        ));
        assert!(!drawn.contains('\u{2026}'), "nothing was dropped, so nothing should say it was: {drawn:?}");
        assert_eq!(drawn.split_whitespace().collect::<Vec<_>>(), LONG.split_whitespace().collect::<Vec<_>>());
    }

    /// `max_lines = 0` is the uncapped spelling a `Bound` needs, since a signal cannot produce
    /// "absent". It has to mean the same thing as leaving the property off.
    #[test]
    fn a_max_lines_of_zero_is_no_cap_at_all() {
        let uncapped =
            format!(r#"panel {{ id = "bar", child = text {{ width = 80, content = "{LONG}", wrap = "Word" }} }}"#);
        let zero = format!(
            r#"panel {{ id = "bar", child = text {{ width = 80, content = "{LONG}", wrap = "Word", max_lines = 0 }} }}"#
        );
        assert_eq!(text_box(&zero), text_box(&uncapped));
    }

    #[test]
    fn a_text_too_wide_for_its_box_is_cut_short_and_finished_with_an_ellipsis() {
        let drawn = elided(&format!(
            r#"panel {{ id = "bar", child = text {{ width = 80, content = "{LONG}", elide = "End" }} }}"#
        ));
        assert!(drawn.ends_with('\u{2026}'), "must end with an ellipsis: {drawn:?}");
        assert!(drawn.chars().count() < LONG.chars().count(), "must be shorter than the original: {drawn:?}");
        assert!(LONG.starts_with(drawn.trim_end_matches('\u{2026}')), "must be a prefix of the original: {drawn:?}");
    }

    #[test]
    fn a_text_that_already_fits_is_left_exactly_as_written() {
        let drawn = elided(r#"panel { id = "bar", child = text { width = 600, content = "short", elide = "End" } }"#);
        assert_eq!(drawn, "short", "an ellipsis on a string that fits would be a lie about the content");
    }

    /// The default. Without it the clip cuts mid-glyph, which is what every `text` did before this.
    #[test]
    fn a_text_that_does_not_ask_to_elide_keeps_its_whole_string() {
        let drawn = elided(&format!(r#"panel {{ id = "bar", child = text {{ width = 80, content = "{LONG}" }} }}"#));
        assert_eq!(drawn, LONG);
    }

    /// A `Content`-sized box came from measuring this same string, so it fits by construction.
    #[test]
    fn a_content_sized_text_never_elides_itself() {
        let drawn = elided(&format!(r#"panel {{ id = "bar", child = text {{ content = "{LONG}", elide = "End" }} }}"#));
        assert_eq!(drawn, LONG);
    }

    /// The degenerate end of the search: a box too narrow for even one character leaves the
    /// ellipsis alone rather than panicking on an empty prefix or returning the whole string.
    #[test]
    fn a_box_too_narrow_for_one_character_draws_only_the_ellipsis() {
        let drawn = elided(&format!(
            r#"panel {{ id = "bar", child = text {{ width = 1, content = "{LONG}", elide = "End" }} }}"#
        ));
        assert_eq!(drawn, "\u{2026}");
    }

    /// Cut at character boundaries, so a multi-byte codepoint is kept or dropped whole rather than
    /// sliced into invalid UTF-8. Panics inside the search if this ever regresses.
    #[test]
    fn a_multibyte_string_is_cut_at_character_boundaries() {
        let drawn = elided(
            r#"panel { id = "bar", child = text { width = 40, content = "ααααααααααααααααααααααααα", elide = "End" } }"#,
        );
        assert!(drawn.ends_with('\u{2026}'));
        assert!(drawn.trim_end_matches('\u{2026}').chars().all(|c| c == 'α'), "no partial codepoints: {drawn:?}");
    }

    #[test]
    fn an_unknown_elide_fails_the_pass_naming_the_property() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) =
            surface_from(r#"panel { id = "bar", child = text { content = "hi", elide = "Middle" } }"#);
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "elide"), "got {err:?}");
    }

    #[test]
    fn an_opacity_outside_zero_to_one_fails_the_pass() {
        let shaping = ShapingHandle::spawn();
        for bad in ["50", "-0.5", "1.5", r#""half""#] {
            let mut scene = Scene::new();
            let (_lua, surface) =
                surface_from(&format!(r#"panel {{ id = "bar", child = rect {{ opacity = {bad} }} }}"#));
            let err = apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap_err();
            assert!(
                matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "opacity"),
                "`opacity = {bad}` must be refused by name, got {err:?}"
            );
        }
    }

    #[test]
    fn an_unknown_cursor_name_fails_the_pass() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(r#"panel { id = "bar", child = rect { cursor = "hand" } }"#);
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "cursor"), "got {err:?}");
    }

    #[test]
    fn an_absent_opacity_is_fully_opaque() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(r#"panel { id = "bar", child = rect { width = 10, height = 10 } }"#);
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        assert_eq!(scene.surface("bar@TEST").unwrap().children[0].opacity, 1.0);
    }

    #[test]
    fn a_malformed_paint_property_on_an_invisible_node_still_fails_the_pass() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(r#"panel { id = "bar", child = rect { visible = false, background = 5 } }"#);
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap_err();
        assert!(matches!(err, LayoutError::InvalidProperty { property, .. } if property == "background"));
    }

    #[test]
    fn image_is_a_supported_leaf_with_no_intrinsic_size_and_icon_still_has_one() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { children = { image { source = "/tmp/w.png", fit = "contain" }, icon { name = "audio-volume-high", size = 24 } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();

        let row = &scene.surface("bar@TEST").unwrap().children[0];
        let image = &row.children[0];
        assert_eq!(image.kind, "image");
        assert!(image.children.is_empty(), "image is a leaf, never a container");
        assert_eq!((image.rect.width, image.rect.height), (0.0, 0.0));

        let icon = &row.children[1];
        assert_eq!((icon.rect.width, icon.rect.height), (24.0, 24.0));
    }

    #[test]
    fn an_image_given_a_box_takes_that_box() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", width = "Fill", height = "Fill", child = image { source = "/tmp/w.png", width = "Fill", height = "Fill" } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();

        let image = &scene.surface("bar@TEST").unwrap().children[0];
        assert!(
            image.rect.width > 0.0 && image.rect.height > 0.0,
            "a Fill image should take the panel, got {:?}",
            image.rect
        );
    }

    #[test]
    fn textfield_is_a_supported_leaf_kind_carrying_its_properties_unvalidated() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"panel { id = "bar", child = row { children = { textfield { mask_character = "*", secure_submit = { capability = "polkit", action = "authenticate" } } } } }"#,
        );
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();

        let field = &scene.surface("bar@TEST").unwrap().children[0].children[0];
        assert_eq!(field.kind, "textfield");
        assert!(field.children.is_empty(), "textfield is a leaf, never a container");
        assert_eq!(field.properties.get("mask_character").unwrap().as_string().unwrap().to_string_lossy(), "*");
    }

    #[test]
    fn one_declared_surface_resolves_one_tree_per_instance_each_against_its_own_size() {
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

        assert!(
            scene.surface("bar@TEST").is_none(),
            "no instance means no output to resolve against, which is not an error"
        );
    }

    #[test]
    fn an_instance_naming_a_surface_the_evaluation_did_not_declare_is_a_caller_bug() {
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
    fn a_veto_from_admit_takes_the_same_rollback_road_a_failed_walk_takes() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, v1) = surface_from(r#"panel { id = "bar", width = 10, height = 10 }"#);
        apply_at(&mut scene, &[v1], full(), &shaping, &_lua1).unwrap();
        let next_id_before = scene.next_id;

        let (_lua2, v2) = surface_from(r#"panel { id = "bar", width = 99, height = 99 }"#);
        let instances = vec![SurfaceInstance {
            instance_id: "bar@TEST".to_string(),
            declared_id: "bar".to_string(),
            output: "TEST".to_string(),
            available: full(),
        }];
        let err = scene
            .apply_admitting(&[v2], &instances, &shaping, &_lua2, |_| {
                Err(node::invalid("child", "the finished scene is not admissible"))
            })
            .unwrap_err();

        assert!(err.to_string().contains("not admissible"), "the veto's own message must reach the caller: {err}");
        assert_eq!(
            scene.surface("bar@TEST").unwrap().rect.width,
            10.0,
            "a vetoed apply must leave the prior tree on screen"
        );
        assert_eq!(scene.next_id, next_id_before, "and must not leak the ids the vetoed walk allocated");
    }

    #[test]
    fn a_failed_apply_leaves_the_scene_exactly_as_it_was() {
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
        let (_lua1, surface_v1) = surface_from(r#"panel { id = "bar", child = rect { width = 10, height = 10 } }"#);
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let first_id = {
            let key = "bar@TEST";
            scene.surfaces.get(key).unwrap().children[0].id
        };

        let (_lua2, surface_v2) = surface_from(r#"panel { id = "bar", child = rect { width = 99, height = 99 } }"#);
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let second_id = scene.surfaces.get("bar@TEST").unwrap().children[0].id;

        assert_eq!(first_id, second_id, "same kind at the same position must reuse the retained node's identity");
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
        let (_lua1, surface_v1) = surface_from(r#"panel { id = "bar", child = rect { width = 10, height = 10 } }"#);
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        assert!(scene.retiring_ids().is_empty());

        let (_lua2, surface_v2) = surface_from(r#"panel { id = "bar", child = text { content = "hi" } }"#);
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();

        assert_eq!(scene.surface("bar@TEST").unwrap().children[0].kind, "text");
        assert_eq!(scene.retiring_ids().len(), 1, "the replaced rect must be retired, not dropped");
    }

    #[test]
    fn a_shrinking_child_list_retires_the_removed_tail() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = { rect { width = 1, height = 1 }, rect { width = 2, height = 2 } } } }"#,
        );
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();

        let (_lua2, surface_v2) =
            surface_from(r#"panel { id = "bar", child = row { children = { rect { width = 1, height = 1 } } } }"#);
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

        let (_lua2, surface_v2) = surface_from(r#"panel { id = "bar", child = row { children = {} } }"#);
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();

        let order = scene.retiring_ids();
        let inner_pos = order.iter().position(|id| *id == inner_id).unwrap();
        let outer_pos = order.iter().position(|id| *id == outer_id).unwrap();
        assert!(inner_pos < outer_pos, "the child must be retired before its parent");
    }

    #[test]
    fn release_removes_exactly_one_retiring_entry_and_is_false_on_an_unknown_id() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(r#"panel { id = "bar", child = rect { width = 1, height = 1 } }"#);
        apply_at(&mut scene, &[surface_v1], full(), &shaping, &_lua1).unwrap();
        let (_lua2, surface_v2) = surface_from(r#"panel { id = "bar", child = text { content = "x" } }"#);
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();

        let id = scene.retiring_ids()[0];
        assert!(scene.release(id));
        assert!(scene.retiring_ids().is_empty());
        assert!(!scene.release(id), "releasing an already-released id must return false");
        assert!(!scene.release(NodeId(9999)), "releasing an id that never existed must return false");
    }

    #[test]
    fn a_self_referential_literal_tree_is_rejected_with_a_layout_error() {
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
        let deepest = MAX_TREE_DEPTH as usize;
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();

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

        assert_eq!(
            row.children[0].id, original_id,
            "position 0 still reuses the retained identity, since matching stays positional"
        );
        assert_ne!(
            row.children[1].id, original_id,
            "position 1 is a fresh allocation, not a reused identity -- matching today's rule with no ids present"
        );
    }

    #[test]
    fn a_mixed_child_list_keeps_identified_node_ids_across_applies_and_the_rest_only_by_position() {
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
        assert_eq!(
            row.children.len(),
            2,
            "two columns, each with its own 'inner' child, must not collide across parents"
        );
        assert_eq!(row.children[0].children[0].rect.width, 1.0);
        assert_eq!(row.children[1].children[0].rect.width, 2.0);
    }

    #[test]
    fn a_signal_valued_id_is_rejected() {
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
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"panel { id = "bar", child = row { children = { rect { id = "x", width = 1, height = 1 } } } }"#,
        );
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
        let inner_pos =
            order.iter().position(|id| *id == inner_id).expect("the vanished subtree's child must be retired");
        let outer_pos = order.iter().position(|id| *id == outer_id).expect("the vanished node must be retired");
        assert!(inner_pos < outer_pos, "retirement stays child-first even with a same-length fresh list");
    }

    #[test]
    fn many_identified_siblings_reconcile_in_reversed_order_without_a_quadratic_scan() {
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
        let before: Vec<NodeId> =
            scene.surfaces.get("bar@TEST").unwrap().children[0].children.iter().map(|c| c.id).collect();

        let mut v2_children = String::new();
        v2_children.push_str("rect { id = \"fresh\", width = 1, height = 1 },\n");
        for i in (1..N).rev() {
            v2_children.push_str(&format!("rect {{ id = \"n{i}\", width = 1, height = 1 }},\n"));
        }
        let (_lua2, surface_v2) =
            surface_from(&format!("panel {{ id = \"bar\", child = row {{ children = {{ {v2_children} }} }} }}"));
        apply_at(&mut scene, &[surface_v2], full(), &shaping, &_lua2).unwrap();
        let after: Vec<NodeId> =
            scene.surfaces.get("bar@TEST").unwrap().children[0].children.iter().map(|c| c.id).collect();

        assert_eq!(after.len(), N);
        for (slot, i) in (1..N).rev().enumerate() {
            assert_eq!(after[slot + 1], before[i], "n{i} must keep its NodeId across the reversal");
        }
        assert!(!before.contains(&after[0]), "the never-seen `fresh` id must allocate rather than inherit n0's node");
        assert!(scene.retiring_ids().contains(&before[0]), "n0 left the config, so it is retired");
    }

    /// The three tests below share one shape: a `margin` that is a `computed` signal counting its
    /// own reads into a Lua global, so what a single `Scene::apply` does with that property is
    /// observable from the config's side.
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

    /// The shape resolving-once does not close: a plain Lua table with an `__index`, no `Signal`
    /// anywhere. Every metamethod-aware
    /// `Table::get` used to re-run the metamethod, so `margin` was parsed four separate times per
    /// child per pass and the four answers were free to differ.
    fn surface_with_an_index_counting_margin(lua: &mlua::Lua) -> VirtualNode {
        register_node_constructors(lua).unwrap();
        crate::lua::signal::register(lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                indexes = 0
                local m = setmetatable({}, { __index = function(_, key)
                    indexes = indexes + 1
                    if key == "left" then return indexes end
                    return 0
                end })
                return panel { id = "bar", child = row { children = {
                    rect { width = 10, height = 10, margin = m },
                } } }
                "#,
            )
            .eval()
            .unwrap();
        deserialize_lua_table(&table).unwrap()
    }

    /// One parse, four keys. It was 16 invocations before `LayoutStyle`: the parent's child loop,
    /// both sizing folds and the positioning pass, each reading `top`, `right`,
    /// `bottom` and `left` off the same table.
    #[test]
    fn a_margin_table_is_read_exactly_once_per_node_per_pass() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        let surface = surface_with_an_index_counting_margin(&lua);

        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        assert_eq!(
            lua.globals().get::<u32>("indexes").unwrap(),
            4,
            "one `parse_edge_insets` over four keys, not one per reader"
        );
    }

    /// The defect that count caused, pinned directly. An `__index` answering `left` with a fresh
    /// number each read made the sizing pass and the positioning pass disagree: measured before
    /// this fix, a row measured itself 18 wide and then placed its 10-wide child spanning 16..26,
    /// eight pixels outside the parent it had just been sized to fit.
    #[test]
    fn an_index_metamethod_cannot_make_the_two_passes_disagree() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        let surface = surface_with_an_index_counting_margin(&lua);

        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        let row = &scene.surface("bar@TEST").unwrap().children[0];
        let child = &row.children[0];
        assert_eq!(
            child.rect.x + child.rect.width,
            row.rect.width,
            "the margin the row was measured with must be the margin its child was positioned with: \
             the child spans {}..{} inside a {}-wide row",
            child.rect.x,
            child.rect.x + child.rect.width,
            row.rect.width
        );
    }

    #[test]
    fn an_impure_margin_closure_positions_a_child_inside_the_size_its_parent_was_measured_at() {
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

    /// The guarantee most likely to be lost by a later edit: every getter fires exactly once, in
    /// the order the config wrote it. An impure closure like
    /// this one is what can observe it.
    ///
    /// This used to read `abBA`, and the difference is worth keeping in view. The `Fill` child is
    /// declared first, and the hand-written pass recursed into it *last* so it could be sized from
    /// what its siblings left, which forced resolution out of the recursion to keep the two
    /// siblings in source order and left the grandchildren interleaved the other way. The solver
    /// does its own sizing, so there is one walk again and it goes in declaration order: `aAbB`,
    /// which is the tree read top to bottom. Same guarantee, and now it is the obvious one.
    #[test]
    fn every_getter_fires_exactly_once_in_the_order_the_config_wrote_it() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                order = ""
                local function mark(name, value)
                    return computed({}, function() order = order .. name; return value end)
                end
                return panel { id = "bar", child = row { width = 600, height = 40, children = {
                    rect { width = "Fill", height = 10, margin = mark("a", 0),
                        children = { rect { width = 1, height = 1, margin = mark("A", 0) } } },
                    rect { width = 100, height = 10, margin = mark("b", 0),
                        children = { rect { width = 1, height = 1, margin = mark("B", 0) } } },
                } } }
                "#,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();

        // Four characters for four getters is the "exactly once" half. Their order is the other
        // half: depth-first, declaration order, which is the order the Lua above reads.
        assert_eq!(
            lua.globals().get::<String>("order").unwrap(),
            "aAbB",
            "every getter fires exactly once, in the order the config wrote it"
        );
        let row = &scene.surface("bar@TEST").unwrap().children[0];
        assert_eq!(row.children[0].rect.width, 500.0, "and the fill child was still sized from the remainder");
    }

    #[test]
    fn a_second_apply_resolves_the_property_again_rather_than_reusing_the_first_passes_answer() {
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
        assert!(!lua.globals().get::<bool>("ran").unwrap(), "a rejected child's property getters must not run");
    }

    /// A `rect` with a background, the way the region scan sees one.
    fn solid_paint() -> Option<PaintStyle> {
        let lua = mlua::Lua::new();
        let mut properties = HashMap::new();
        properties.insert("background".to_string(), Value::String(lua.create_string("#112233").unwrap()));
        node::paint_style("rect", &properties).unwrap()
    }

    fn region_node(
        id: u64,
        kind: &str,
        rect: (f32, f32, f32, f32),
        paint: Option<PaintStyle>,
        children: Vec<ResolvedNode>,
    ) -> ResolvedNode {
        ResolvedNode {
            id: NodeId::test(id),
            kind: kind.to_string(),
            rect: LogicalRect { x: rect.0, y: rect.1, width: rect.2, height: rect.3 },
            visible: true,
            opacity: 1.0,
            properties: HashMap::new(),
            paint,
            children,
        }
    }

    /// ADR-0109: a transparent container is walked into; a solid child claims its box; a `button`
    /// with a handler claims its box with nothing painted; a transparent leaf claims nothing.
    #[test]
    fn overlay_input_regions_come_from_what_is_drawn_and_what_is_clickable() {
        let lua = mlua::Lua::new();
        let card_a = region_node(1, "rect", (0.0, 0.0, 100.0, 40.0), solid_paint(), Vec::new());
        let card_b = region_node(2, "rect", (0.0, 50.0, 100.0, 40.0), solid_paint(), Vec::new());
        let column = region_node(3, "column", (10.0, 10.0, 100.0, 500.0), None, vec![card_a, card_b]);
        let root = region_node(4, "panel", (0.0, 0.0, 120.0, 520.0), None, vec![column]);
        assert_eq!(
            overlay_input_regions(&root, 1.0),
            [PhysicalRect { x0: 10, y0: 10, x1: 110, y1: 50 }, PhysicalRect { x0: 10, y0: 60, x1: 110, y1: 100 }],
            "the cards, at their surface-local positions, and not the column"
        );

        let mut catcher = region_node(5, "button", (0.0, 0.0, 120.0, 520.0), None, Vec::new());
        catcher
            .properties
            .insert("on_click".to_string(), Value::Function(lua.create_function(|_, ()| Ok(())).unwrap()));
        let root = region_node(6, "panel", (0.0, 0.0, 120.0, 520.0), None, vec![catcher]);
        assert_eq!(overlay_input_regions(&root, 1.0), [PhysicalRect { x0: 0, y0: 0, x1: 120, y1: 520 }]);

        let idle_button = region_node(7, "button", (0.0, 0.0, 120.0, 520.0), None, Vec::new());
        let root = region_node(8, "panel", (0.0, 0.0, 120.0, 520.0), None, vec![idle_button]);
        assert!(overlay_input_regions(&root, 1.0).is_empty(), "a button with no handler is as transparent as a rect");
    }

    #[test]
    fn overlay_input_regions_includes_only_visible_direct_children() {
        let visible_child = ResolvedNode {
            id: NodeId::test(102),
            kind: "rect".to_string(),
            rect: LogicalRect { x: 0.0, y: 0.0, width: 10.0, height: 10.0 },
            visible: true,
            opacity: 1.0,
            properties: HashMap::new(),
            paint: solid_paint(),
            children: Vec::new(),
        };
        let hidden_child = ResolvedNode {
            id: NodeId::test(103),
            kind: "rect".to_string(),
            rect: LogicalRect { x: 20.0, y: 20.0, width: 10.0, height: 10.0 },
            visible: false,
            opacity: 1.0,
            properties: HashMap::new(),
            paint: None,
            children: Vec::new(),
        };
        let root = ResolvedNode {
            id: NodeId::test(104),
            kind: "panel".to_string(),
            rect: LogicalRect { x: 0.0, y: 0.0, width: 100.0, height: 100.0 },
            visible: true,
            opacity: 1.0,
            properties: HashMap::new(),
            paint: None,
            children: vec![visible_child, hidden_child],
        };

        let regions = overlay_input_regions(&root, 1.0);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0], PhysicalRect { x0: 0, y0: 0, x1: 10, y1: 10 });
    }

    #[test]
    fn a_surface_with_nothing_visible_in_it_claims_no_input_at_all() {
        let hidden_child = ResolvedNode {
            id: NodeId::test(105),
            kind: "rect".to_string(),
            rect: LogicalRect { x: 0.0, y: 0.0, width: 100.0, height: 100.0 },
            visible: false,
            opacity: 1.0,
            properties: HashMap::new(),
            paint: None,
            children: Vec::new(),
        };
        let mut root = ResolvedNode {
            id: NodeId::test(106),
            kind: "panel".to_string(),
            rect: LogicalRect { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0 },
            visible: true,
            opacity: 1.0,
            properties: HashMap::new(),
            paint: None,
            children: vec![hidden_child],
        };
        assert!(overlay_input_regions(&root, 1.0).is_empty());

        root.children.clear();
        assert!(overlay_input_regions(&root, 1.0).is_empty());
    }

    #[test]
    fn a_child_that_fills_its_surface_claims_the_whole_surface() {
        let root = ResolvedNode {
            id: NodeId::test(107),
            kind: "panel".to_string(),
            rect: LogicalRect { x: 0.0, y: 0.0, width: 1920.0, height: 32.0 },
            visible: true,
            opacity: 1.0,
            properties: HashMap::new(),
            paint: None,
            children: vec![ResolvedNode {
                id: NodeId::test(120),
                kind: "row".to_string(),
                rect: LogicalRect { x: 0.0, y: 0.0, width: 1920.0, height: 32.0 },
                visible: true,
                opacity: 1.0,
                properties: HashMap::new(),
                paint: solid_paint(),
                children: Vec::new(),
            }],
        };

        assert_eq!(overlay_input_regions(&root, 1.0), [PhysicalRect { x0: 0, y0: 0, x1: 1920, y1: 32 }]);
    }

    #[test]
    fn a_list_resolves_one_child_per_source_element_in_source_order() {
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
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let itemfn = r#"function(item) return rect { width = item, height = 1 } end"#;
        let (_lua1, surface_v1) =
            surface_from(&format!(r#"panel {{ id = "bar", child = list {{ source = {{ 1 }}, itemfn = {itemfn} }} }}"#));
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
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) =
            surface_from(r#"panel { id = "bar", child = list { itemfn = function(item) return rect {} end } }"#);
        let err = apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap_err();
        assert!(matches!(&err, LayoutError::InvalidProperty { property, .. } if property == "source"), "{err:?}");
    }

    #[test]
    fn a_list_source_that_is_not_a_table_is_a_layout_error_naming_source() {
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
        let (_lua, surface) = surface_from(r#"panel { id = "bar", child = list { source = { 1 }, itemfn = "nope" } }"#);
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
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"{ kind = "window", id = "settings", width = 400, child = rect { width = "Fill", height = "Fill" } }"#,
        );
        apply_at(&mut scene, &[surface], LogicalSize { width: 1920.0, height: 1168.0 }, &shaping, &lua).unwrap();

        let root = scene.surface("settings@TEST").unwrap();
        assert_eq!(
            (root.rect.width, root.rect.height),
            (400.0, 1168.0),
            "the written width stands; the unwritten height fills"
        );
    }

    #[test]
    fn a_popup_root_needs_no_forcing_because_section_6_3_requires_both_of_its_sizes() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"{ kind = "popup", id = "menu", parent = "bar", width = 200, height = 120,
                 anchor_rect = { x = 0, y = 0, width = 86, height = 24 },
                 child = rect { width = "Fill", height = "Fill" } }"#,
        );
        apply_at(&mut scene, &[surface], LogicalSize { width: 200.0, height: 120.0 }, &shaping, &lua).unwrap();

        let root = scene.surface("menu@TEST").unwrap();
        assert_eq!((root.rect.width, root.rect.height), (200.0, 120.0));
        assert_eq!((root.children[0].rect.width, root.children[0].rect.height), (200.0, 120.0));
    }

    #[test]
    fn a_lock_is_a_supported_kind_carrying_a_single_child_and_stretching_it_like_a_panel() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(
            r#"{ kind = "lock", id = "screen-lock",
                 child = rect { align_h = "Stretch", align_v = "Stretch" } }"#,
        );
        apply_at(&mut scene, &[surface], LogicalSize { width: 1920.0, height: 1080.0 }, &shaping, &lua).unwrap();

        let root = scene.surface("screen-lock@TEST").unwrap();
        assert_eq!(root.kind, "lock");
        assert_eq!(root.children.len(), 1, "§ 6.4's `child`, read through the same `parse_single_child` a panel's is");
        assert_eq!((root.children[0].rect.width, root.children[0].rect.height), (1920.0, 1080.0));
    }

    #[test]
    fn an_unsized_lock_root_is_its_output_so_a_fill_child_covers_the_locked_screen() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) =
            surface_from(r#"{ kind = "lock", id = "screen-lock", child = rect { width = "Fill", height = "Fill" } }"#);
        apply_at(&mut scene, &[surface], LogicalSize { width: 2560.0, height: 1440.0 }, &shaping, &lua).unwrap();

        let root = scene.surface("screen-lock@TEST").unwrap();
        assert_eq!((root.rect.width, root.rect.height), (2560.0, 1440.0));
        assert_eq!((root.children[0].rect.width, root.children[0].rect.height), (2560.0, 1440.0));
    }

    #[test]
    fn an_unsupported_top_level_kind_is_still_rejected() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(r#"{ kind = "dialog", id = "s", child = rect {} }"#);
        assert!(matches!(
            apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap_err(),
            LayoutError::UnsupportedNodeKind(k) if k == "dialog"
        ));
    }

    /// taffy 0.14 adds a flex container's own margin to its children's minimum cross size when it
    /// measures them (see `taffy_style`'s `min_size`). The card here is the panel host's: a column
    /// with a left margin of most of the output, holding a body that wraps at the card's width.
    /// Its height has to be the wrapped body's, whatever the margin.
    #[test]
    fn a_containers_own_margin_does_not_widen_what_its_children_are_measured_at() {
        let long = "have a look at this: https://github.com/anasgets111/oblisk-shell/pull/12 and tell me what you think about it all";
        let heights = |margin: u32| {
            let src = format!(
                r#"panel {{ id = "bar", child = column {{ width = "Fill", height = "Fill", children = {{
                column {{ width = 392, margin = {{ left = {margin}, top = 4 }}, padding = {{ top = 7, right = 7, bottom = 7, left = 7 }}, children = {{
                    column {{ width = "Fill", children = {{
                        text {{ content = "Anas", font_size = 14 }},
                        text {{ content = "{long}", font_size = 12, wrap = "Word", width = "Fill" }},
                    }} }},
                }} }},
            }} }} }}"#
            );
            let mut scene = Scene::new();
            let shaping = ShapingHandle::spawn();
            let (_lua, surface) = surface_from(&src);
            apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
            let card = &scene.surface("bar@TEST").unwrap().children[0].children[0];
            let inner = &card.children[0];
            (card.rect.height, inner.rect.height, inner.children[0].rect.height + inner.children[1].rect.height)
        };
        let (card, inner, lines) = heights(1521);
        assert_eq!(inner, lines, "the column is as tall as its two texts, one of them wrapped");
        assert_eq!(card, inner + 14.0, "and the card is that plus its padding");
        assert_eq!((card, inner), (heights(0).0, heights(0).1), "the margin moves the card, it does not resize it");
    }
}

#[cfg(test)]
mod pass_budget_tests {
    use super::tests::{apply_at, full};
    use super::*;
    use crate::lua::nodes::{deserialize_lua_table, register_node_constructors};
    use crate::text::shaping::ShapingHandle;

    /// The config here contains no `Signal` at all:
    /// a plain table with an `__index` that never returns is Lua the pass runs outside any signal
    /// evaluation, so ADR-0021's per-getter cap never covered it. Item 5 measured this exact shape
    /// at 26.10 seconds returning `Ok(())`.
    ///
    /// Slow on purpose, and the only test here that is: what it pins is a wall-clock bound, so it
    /// has to spend it. Roughly `LAYOUT_PASS_CAP`.
    #[test]
    fn a_runaway_index_metamethod_fails_the_pass_instead_of_hanging_it() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();
        let table: mlua::Table = lua
            .load(
                r#"
                local m = setmetatable({}, { __index = function() while true do end end })
                return panel { id = "bar", child = row { children = {
                    rect { width = 10, height = 10, margin = m },
                } } }
                "#,
            )
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        let started = std::time::Instant::now();
        let outcome = apply_at(&mut scene, &[surface], full(), &shaping, &lua);
        let elapsed = started.elapsed();

        assert!(
            matches!(outcome, Err(LayoutError::PassBudgetExceeded)),
            "an unbounded metamethod must blame the budget, not whichever property it was reading: {outcome:?}"
        );
        assert!(elapsed < std::time::Duration::from_secs(20), "must be bounded, took {elapsed:?}");
    }

    /// The failure rolls back like every other one (`CONTEXT.md`, Rollback). A pass refused
    /// halfway must not leave the scene holding a partly-resolved tree, or the next repaint draws
    /// it.
    #[test]
    fn a_pass_refused_by_the_budget_leaves_the_scene_as_it_was() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).unwrap();

        let good: mlua::Table =
            lua.load(r#"return panel { id = "bar", child = rect { width = 40, height = 10 } }"#).eval().unwrap();
        apply_at(&mut scene, &[deserialize_lua_table(&good).unwrap()], full(), &shaping, &lua).unwrap();
        let before = scene.surface("bar@TEST").unwrap().children[0].rect.width;

        let runaway: mlua::Table = lua
            .load(
                r#"
                local m = setmetatable({}, { __index = function() while true do end end })
                return panel { id = "bar", child = rect { width = 99, height = 10, margin = m } }
                "#,
            )
            .eval()
            .unwrap();
        let outcome = apply_at(&mut scene, &[deserialize_lua_table(&runaway).unwrap()], full(), &shaping, &lua);

        assert!(matches!(outcome, Err(LayoutError::PassBudgetExceeded)), "{outcome:?}");
        assert_eq!(scene.surface("bar@TEST").unwrap().children[0].rect.width, before, "the good tree survives");
    }
}
