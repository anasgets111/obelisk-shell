//! The resolve pass and the retained-scene transaction (`CONTEXT.md`'s "Retained scene" entries).
//!
//! A pass is three steps. [`prepare`] walks the fresh tree in declaration order, resolving each
//! node's properties once, parsing them once, matching it to the retained node it continues
//! (`pair_children_by_id_then_position`: children carrying an `id` pair by that `id` alone,
//! scoped to their parent, and the id-less ones pair by position among themselves, ADR-0023's
//! original rule on a smaller set, amended by ADR-0045), retiring removed subtrees child-first
//! (`CONTEXT.md`, Lease), and building a taffy node for it. [`solve`] hands that tree to taffy,
//! which runs its own layout passes over it. [`finish`] reads the geometry back out.
//!
//! The layout math itself is taffy's: ADR-0077 replaced this module's hand-written one-pass
//! stacking solver (ADR-0023) with it. What is still this module's own is everything a solver
//! has no opinion about: node identity, the lease, the depth cap, the once-per-pass resolution
//! guarantee, the scroll writeback, and text elision. [`taffy_style`] is the seam between the
//! two, and it is the only place that knows what a `row` or a `Fill` means.

use std::collections::{HashMap, HashSet};

use mlua::{Lua, Value};

use crate::layout::instance::SurfaceInstance;
use crate::layout::node::{self, Align, EdgeInsets, LayoutError, PaintStyle, SizeMode};
use crate::lua::nodes::VirtualNode;
use crate::text::shaping::{ShapeRequest, ShapingHandle};
use crate::text::snap::{LogicalRect, PhysicalRect, snap_to_physical};
use taffy::prelude::{length, line, span};

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LogicalSize {
    pub width: f32,
    pub height: f32,
}

/// The recursion bound for [`prepare`]: at most this many node levels are admitted, and the
/// level past it is refused, the same "at most N levels" boundary `lua::signal`'s
/// `MAX_SIGNAL_NESTING_DEPTH` uses. Applies to both a literal cyclic tree (`r.children = { r }`)
/// and a computed `children` signal that manufactures fresh depth on every read: both recurse
/// through this same function, so one counter catches both.
///
/// Exists to turn an abort into a `LayoutError`, not to express a design limit. Sizing it means
/// sizing it *together with* `MAX_SIGNAL_NESTING_DEPTH`, because the two recursions compound: a
/// node level resolves its `children` property, and that resolution can nest signals.
///
/// Measured end to end by running the compound worst case (a tree at this cap whose every level
/// carries a 31-deep `computed` chain on its `margin`) on a thread whose stack is shrunk until the
/// process aborts, which counts every frame: the mlua frames below the deepest node and the
/// error-formatting frames a refusal runs at full depth, not only this module's own recursion.
///
/// On the tightest stack this runs on, a 2 MiB debug test thread (production owns the Lua VM on the
/// process main thread, 8 MiB by default, since `wayland::run` is called from `main`):
///
/// - about 590 KiB for a 64-level tree with no signals in it, near enough 8,960 B per level;
/// - about 1,040 KiB for the same tree with a 31-deep signal nest on every level.
///
/// The nest costs about 450 KiB once rather than per level, because it is popped before the walk
/// descends to the next node. So 64 levels peak at roughly half of a 2 MiB stack, a 1.97x margin.
///
/// 64 rather than fewer, and unchanged by the solver swap (ADR-0077), because that swap
/// *lowered* this: the hand-written pass needed about 1,400 KiB for the same worst case, a 1.44x
/// margin, so the cap is on firmer ground now than it was when it was written. A real `shell.lua`
/// is 10 to 15 levels deep, which leaves 4x headroom above anything a config has asked for.
/// `MAX_SIGNAL_NESTING_DEPTH` keeps its 32 because it is the cap that also bounds dependency
/// chains, the shape a real config is likeliest to grow.
const MAX_TREE_DEPTH: u32 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(u64);

/// Every geometry property one node's layout reads, parsed exactly once per pass.
///
/// A node's properties resolve once, so the `Signal` behind `margin` is read once per node per
/// pass; parsing the resolved value once is the second half of the same guarantee. A resolved
/// property is still an `mlua::Value`, and a `Value::Table` carrying an `__index` answers every
/// metamethod-aware `Table::get` afresh, so parsing `margin` once in the parent's child loop,
/// rather than separately wherever a sizing pass needs it, avoids the answer changing between
/// reads. Measured on the hand-written pass this replaced: 16 `__index` invocations for one
/// child's margin in one apply, and a row that measured itself 18 wide then placed its 10-wide
/// child spanning 16..26, eight pixels outside the parent it had just been sized to fit. No
/// `Signal` involved. The solver reads this struct once too (ADR-0077): [`taffy_style`] is the
/// only thing that touches it, and it runs once per node per pass.
///
/// Parsed by the *parent*, in its child loop, for the same reason the property resolve is: a
/// parent needs a child's `margin` to compute the budget it recurses with, so the one parse has to
/// happen before the recursion, not at the top of it. `Scene::apply_one_instance` does it for a
/// surface root, which has no parent, extending the same once-per-node guarantee one frame
/// further up.
///
/// Every field is parsed for every kind, including the ones that kind ignores: `spacing` on a
/// `text`, `align_h` on a `row`'s child where only `align_v` was ever read. That widens what fails
/// the pass, deliberately and in the direction ADR-0068 already chose: a malformed geometry
/// property is now heard about while applying rather than on the day a config changes the node's
/// kind and the property starts being read. It is the same trade made knowingly for resolution
/// ("a getter that raises fails the apply even for a property nothing currently reads, which is
/// correct").
#[derive(Debug, Clone, Copy, PartialEq)]
struct LayoutStyle {
    margin: EdgeInsets,
    padding: EdgeInsets,
    width_mode: SizeMode,
    height_mode: SizeMode,
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
        Ok(Self {
            margin: node::parse_edge_insets(properties, "margin")?,
            padding: node::parse_edge_insets(properties, "padding")?,
            width_mode: node::parse_size_mode(properties, "width")?,
            height_mode: node::parse_size_mode(properties, "height")?,
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

/// The public, ID-less output of one node's resolution: geometry, this node's parsed paint
/// properties, and a passthrough of the raw property map for the readers that want a Lua value
/// rather than a parsed one (`hover`, `on_close`, `on_dismiss`, and the surface-role specs
/// `wayland::surface::apply_resolved_state` re-derives at configure cadence). Reused by
/// `Scene::surface` and by `overlay_input_regions`.
///
/// `properties` holds **resolved** values, never a `Signal` handle: `node::resolve_properties` ran
/// over this node's raw map exactly once, at the top of the pass that produced this node. So this
/// tree is a snapshot of one pass, which is what makes it safe for a later reader to take a value
/// straight off it without resolving anything itself. The structural keys are
/// the deliberate exception: `node::is_structural_property` copies them through raw on the kinds
/// whose parsers read them, and those parsers reject a `Signal` outright, so no handle reaches
/// here by that route either.
///
/// ponytail: absent and nil are one state in this map. `node::resolve_properties` omits a key whose
/// signal resolved to `Value::Nil` (ADR-0044 decision 1's amendment), so a `background` bound to a
/// capability signal that currently reads `nil` is indistinguishable here from a `background` the
/// config never set. For every property a parser in `layout::node` reads that is exactly right --
/// each has a documented default and the two spellings of "no value here" must agree, which is the
/// argument that rule rests on. For the paint-only properties it is a real difference collapsed:
/// `node::paint_style` applies the parser's documented default to a property the config did bind,
/// at precisely the moments a capability has not answered yet (every `Capability::ALL` global reads
/// `nil` until the first `StateSnapshot` drains, so this is the state at boot, not an edge case).
/// Distinguishing them means a third state in the map, `Value::Nil` retained as "bound but
/// unresolved", and every parser here re-learning to treat it as absent. Still not worth it: the
/// collapse costs one boot frame painted at the default, and the value it is waiting on arrives on
/// the next turn.
#[derive(Debug, Clone)]
pub struct ResolvedNode {
    pub kind: String,
    pub rect: LogicalRect,
    pub visible: bool,
    /// This node's own `opacity`, before any ancestor's is applied. `layout::paint::build_node`
    /// multiplies the chain together as it descends, the same way it intersects a clip, so a panel
    /// fades with everything in it from one property. 1.0 is the default and contributes nothing.
    pub opacity: f32,
    pub properties: HashMap<String, Value>,
    /// This node's paint properties, parsed here rather than by `layout::paint` on every frame
    /// (`node::paint_style`'s module doc comment says why). `None` for a kind that draws nothing.
    pub paint: Option<PaintStyle>,
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
    /// This node's geometry, parsed once for the pass that produced it. `visible` and `opacity`
    /// live in here too, which is why they are no longer separate fields: they were parsed in the
    /// same place and read by the same passes.
    style: LayoutStyle,
    properties: HashMap<String, Value>,
    paint: Option<PaintStyle>,
    children: Vec<RetainedNode>,
}

impl RetainedNode {
    fn to_resolved(&self) -> ResolvedNode {
        ResolvedNode {
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

/// The persistent node tree for one generation (`CONTEXT.md`, Retained scene), keyed per
/// **surface instance** by that instance's `"{id}@{output}"` id (`CONTEXT.md`, Surface instance;
/// `layout::instance`).
///
/// Instance-keyed, not declared-id-keyed: a laptop panel and a 4K external genuinely need two
/// resolved trees, because a surface targeting `monitor = "All"` gets one `zwlr_layer_surface_v1`
/// per output and each is configured to a different size. One tree per declared surface cannot
/// serve both -- whichever output resolved last would decide the geometry the other one painted.
///
/// This *refines* ADR-0045 decision 5 rather than replacing it. The surface `id` is still the
/// reconcile identity: `apply` finds a fresh `VirtualNode` by `node::parse_surface_id` exactly as
/// before. What the key adds is the output half, stable for an instance's whole life -- an
/// instance is created for one output and dies with it -- so the pair is as much an identity as
/// the `id` alone was.
///
/// Everything below a surface's root reconciles through `pair_children_by_id_then_position`: an
/// optional, per-parent-scoped `id` pairs only against the same `id`, and the children carrying
/// none fall back to ADR-0023's original positional rule among themselves, amended by ADR-0045.
#[derive(Default)]
pub struct Scene {
    surfaces: HashMap<String, RetainedNode>,
    /// Removed subtrees, held alive child-first order until [`Scene::release`] finalizes the
    /// drop (`CONTEXT.md`, Lease). Nothing calls `release` in production yet (see ADR-0023).
    /// `Vec`, not a `HashMap`, specifically so insertion order (child-first) is
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
    /// its `instance_id` and resolved against that instance's own `available` size.
    ///
    /// The two arguments answer different questions and neither implies the other. `fresh_surfaces`
    /// is what the config *declared*; `instances` is what the compositor is actually being asked to
    /// map (`layout::instance::expand_instances`). A declared surface with no instance -- a
    /// `monitor` naming an unplugged display -- resolves not at all: there is no output to resolve
    /// it against. The reverse, an instance naming a surface `fresh_surfaces` does not contain, is
    /// a caller bug rather than a config one (the two come from the same evaluation), so it raises
    /// [`LayoutError::InvalidProperty`] rather than being skipped.
    ///
    /// An instance id present in the retained scene but absent from `instances` this cycle is left
    /// untouched: a `surface` disappearing entirely is a topology change (`CONTEXT.md`), handled by
    /// a generation swap, not this in-place apply.
    ///
    /// `admit` is a veto on the *finished* apply, run once after every instance has reconciled and
    /// before this returns `Ok`. Some invariants are not properties of any one node and cannot be
    /// checked as the walk builds one -- `crate::socket`'s lock guard has to ask whether the whole
    /// resolved lock tree is still authenticatable, which is only answerable once the tree exists.
    /// An `Err` from it takes the same road an `Err` from the walk takes: the snapshot below is
    /// restored and the caller keeps the scene it had.
    ///
    /// Rolls back to exactly its pre-call state on `Err` (`CONTEXT.md`, Rollback; `socket.rs`'s
    /// `handle_reevaluate` already assumes this). A Signal getter can fail at any depth inside
    /// [`prepare`], partway through mutating `self.next_id` and `self.retiring`, and
    /// partway through a multi-surface `fresh_surfaces` list, so snapshotting
    /// `surfaces`/`next_id`/`retiring` up front and restoring the snapshot wholesale on any error is
    /// the smallest change that holds the invariant for all three.
    ///
    /// ponytail: the snapshot is taken unconditionally, so every apply deep-clones the retained
    /// tree whether or not anything fails; the dirty flag that gates a capability push turns that
    /// into once per capability push rather than once per config edit. The clone is structural (a
    /// `RetainedNode`'s `mlua::Value` properties are refcount bumps, not deep copies), so it is
    /// O(nodes), not O(Lua heap). Resolving every property up front, before touching any state,
    /// does not remove the clone: a parent must know a child's `margin` before it can hand that
    /// child a budget, so resolution is interleaved with the walk rather than preceding it, and a
    /// Signal getter can still fail at arbitrary depth with `next_id` and `retiring` already
    /// mutated. Removing the snapshot needs the whole fresh tree resolved into a separate tree
    /// first, which is a second traversal and a second allocation to save a clone that is already
    /// O(nodes).
    /// The unguarded spelling, gated `#[cfg(test)]` so it cannot become a production call site.
    /// Threading a `|_| Ok(())` through every fixture would only make the real callers' guard
    /// easier to leave off by accident; gating this on `test` keeps [`Scene::apply_admitting`] the
    /// *only* way production code applies a scene, which is the property `crate::socket`'s lock
    /// veto depends on.
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

        // One budget for the whole pass, not one per getter call. It has to be entered out here
        // rather than inside the walk for both of the things it bounds: the instruction hook
        // stays installed across the gaps between signal evaluations, where a resolved table's
        // `__index` would otherwise run unhooked, and the deadline spans every node so a tree of
        // individually-legal 5ms getters cannot add up to an unbounded pass.
        let budget = match crate::lua::signal::LayoutPassBudget::enter(lua) {
            Ok(budget) => budget,
            Err(err) => return Err(node::invalid("layout", err.to_string())),
        };
        // Every exit reports a blown budget as a blown budget. Whatever error the walk raised on
        // the way out is a symptom of it: the hook interrupts whichever `Table::get` or getter
        // happened to be running, so without this the failure surfaces as an `InvalidProperty`
        // naming an arbitrary property that is not itself wrong.
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
        // A pass that ran over but never tripped the hook (config Lua can catch the hook's error
        // with `pcall`; it cannot catch this) still fails, and rolls back like any other failure.
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
        // Matched by the declared `id`, keyed by the instance id: ADR-0045 decision 5's
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
        // get in the loop below: resolution runs Lua, so a node the walk will refuse must not run
        // any first. Every node below this one is resolved by its own parent, in the child loop
        // that needs its `margin` before it can recurse.
        ensure_node_admissible(&fresh.kind, 0)?;
        let properties = node::resolve_properties(&fresh.properties, &fresh.kind, lua)?;
        // The root's one parse for this pass. Every other node's is done by its parent's child
        // loop; a surface root has no parent, so this is where the once-per-node-per-pass parse
        // guarantee runs out of frames one level up.
        let style = LayoutStyle::parse(&properties)?;
        // **An unsized `window` or `lock` root is its surface.** A `panel` root sizes itself from
        // § 6.1's `width`/`height`; § 6.2 gives a
        // `window` neither, because a toplevel's size is the compositor's, arriving as an
        // `xdg_toplevel` configure `set_instance_size` has already turned into this `available`.
        //
        // Without this the root fell to `parse_size_mode`'s `Content` default, and a
        // `Content`-sized parent hands its children a budget of zero, so `child = column { width =
        // "Fill" }` -- the obvious way to write a window -- resolved to nothing and the toplevel
        // mapped a fully transparent buffer. Measured against niri, which configured the window at
        // its 1920x1168 tile and had a 0x0 tree painted into it.
        //
        // § 6.4's `lock` is the same case and a worse failure: it has no `width` or `height` at all
        // (`layout::node::lock_spec` refuses both), so it can only ever fall to `Content`, and the
        // surface it fills is the whole output. A zero-sized lock root paints a transparent buffer
        // over a locked session, the black screen with no password field ADR-0052 decision 3
        // refuses the lock to avoid.
        //
        // Only the `Content` default is overridden, per axis. A `window` that does write a `width`
        // is writing a property § 6.2 does not define, and the answer is to honour it like any
        // other node's rather than to silently discard it. A `lock` cannot reach that branch,
        // because the spec parser rejected the config before the scene ever saw it.
        let forced = if matches!(fresh.kind.as_str(), "window" | "lock") {
            (
                (style.width_mode == SizeMode::Content).then_some(available.width),
                (style.height_mode == SizeMode::Content).then_some(available.height),
            )
        } else {
            (None, None)
        };

        // One tree per instance, built and dropped inside this call. Nothing carries across a pass:
        // the identity that has to survive one is the `NodeId` on the retained tree, and taffy's own
        // node ids are an implementation detail of the solve. That also means `apply_admitting`'s
        // rollback has nothing extra to undo -- a failed walk drops the tree on the way out.
        let mut tree: taffy::TaffyTree<Measure> = taffy::TaffyTree::new();
        // The geometry this engine computes is fractional (`layout::text::snap` is what turns it
        // into pixels, per surface scale, at paint time), and taffy rounds layouts to whole numbers
        // unless told not to.
        tree.disable_rounding();
        let prepared = prepare(self, &mut tree, existing, &fresh.kind, properties, style, None, lua, 0)?;

        // The root's own size, patched on after the walk because it is the one node whose `Fill`
        // and `Percent` cannot mean what they mean anywhere else: there is no parent to take a
        // share of or a percentage of, so both resolve against the room the surface was configured
        // at. Every node below it takes its size from the solver.
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
    /// (`layout::instance::SurfaceInstance::instance_id`) -- not by the declared `id` a config
    /// writes. `crate::wayland::App::paint_surface` looks a tree up with exactly the id its
    /// `TrackedSurface` carries, which is what makes the two id spaces one (ADR-0038).
    pub fn surface(&self, instance_id: &str) -> Option<ResolvedNode> {
        self.surfaces.get(instance_id).map(RetainedNode::to_resolved)
    }

    /// Finalizes the drop of one retired subtree. Returns `false` if `id` isn't currently
    /// retiring (already released, or never retired).
    ///
    /// ponytail: no production caller yet -- nothing in this codebase owns a per-node GPU
    /// resource to guard, so nothing needs to signal "done consuming this retired subtree." The
    /// mechanism is built ahead of that consumer (ADR-0023), matching
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

    /// Releases every retired subtree at once, in the child-first order [`Self::retire_child_first`]
    /// established. `crate::socket`'s `RendererClient` calls this after each successful
    /// `Scene::apply`, which is what stops the lease bag growing forever now that an apply runs at
    /// capability-push cadence (ADR-0044 decision 2) rather than once per config edit: every
    /// re-resolve that shortens a `children` list retires the tail, and each retired
    /// `RetainedNode` holds a `HashMap<String, mlua::Value>`, so an undrained bag leaks Lua heap
    /// as well as Rust memory in a process meant to live for a whole session.
    ///
    /// Unconditional, and only correct because it is: nothing in this codebase holds a lease
    /// today, so nothing can be mid-teardown when this runs (ADR-0023, the same fact
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
        let RetainedNode { id, kind, rect, style, properties, paint, children } = node;
        for child in children {
            self.retire_child_first(child);
        }
        self.retiring.push((id, RetainedNode { id, kind, rect, style, properties, paint, children: Vec::new() }));
    }
}

/// All four § 6 roles are here per ADR-0040 decision 1. Every one is a surface container: a
/// role for a `wl_surface`, a `child` tree inside it, and no layout model of their own beyond the
/// stacking model ADR-0023 already gives `panel`. `lock` is admitted the same way
/// (ADR-0052 decision 2): this admits a *node kind* into the walk, and a lock screen's tree
/// has to be walked whether or not the compositor has handed out a surface to paint it into.
fn ensure_supported_kind(kind: &str) -> Result<(), LayoutError> {
    match kind {
        "panel" | "window" | "popup" | "lock" | "rect" | "row" | "column" | "text" | "icon" | "image" | "button"
        | "list" | "textfield" => Ok(()),
        other => Err(LayoutError::UnsupportedNodeKind(other.to_string())),
    }
}

/// Everything a node has to pass before *anything* reads its properties: a supported kind, and a
/// level within [`MAX_TREE_DEPTH`]. Called at each site about to run `node::resolve_properties`
/// over a node's raw map -- `Scene::apply_one_instance` for a surface root, [`prepare`]'s child
/// loop for every node below one -- and again at the top of [`prepare`], which is what keeps
/// [`children_of`]'s `unreachable!` arm unreachable for any caller.
///
/// The ordering is the whole reason this is a function rather than two lines inline. Resolution
/// calls back into Lua (ADR-0044 decision 1), so running it before these checks executes a rejected
/// node's `Signal` getters on its behalf: measured, a self-generating `children` signal's body ran
/// 64 times against a 64-level cap, because the parent at the last admitted level resolved the
/// child's whole property map before recursing into the check that refused that child. The kind
/// case is worse: every property getter of an unsupported-kind node ran, arbitrary Lua side
/// effects for a node that never entered the accepted tree.
///
/// `depth` is the level the node being checked would occupy, so a parent at `depth` checks its
/// children at `depth + 1`, the same number the recursive call is handed.
fn ensure_node_admissible(kind: &str, depth: u32) -> Result<(), LayoutError> {
    ensure_supported_kind(kind)?;
    // A cyclic or infinitely-generating tree returns a LayoutError at MAX_TREE_DEPTH's own stack
    // depth, not somewhere further down (see MAX_TREE_DEPTH's doc comment for the measured stack
    // cost this leaves margin against).
    //
    // `>=`, not `>`: `depth` counts levels already entered, root at 0, so this admits levels
    // 0..MAX_TREE_DEPTH-1 -- exactly MAX_TREE_DEPTH of them, which is what the constant and the
    // error message both say. `>` admitted MAX_TREE_DEPTH + 1 levels while claiming
    // MAX_TREE_DEPTH, and disagreed with the signal cap's `>= MAX_SIGNAL_NESTING_DEPTH` about
    // what "maximum depth" means. The reported `depth` is 1-based (the level being refused) so
    // "maximum depth of 64 levels ... got at least 65" reads as the truth.
    if depth >= MAX_TREE_DEPTH {
        return Err(LayoutError::TreeTooDeep { kind: kind.to_string(), depth: depth + 1, max: MAX_TREE_DEPTH });
    }
    Ok(())
}

/// Dispatches to the right raw property (`child` for the four surface roles, `children` for the
/// container kinds, none for leaves) -- only called after [`ensure_supported_kind`] already
/// validated `node.kind`, so the fallback arm is unreachable, not a silent default.
///
/// `window`, `popup` and `lock` share `panel`'s arm because § 6.2, § 6.3 and § 6.4 give each of
/// them exactly one `child`, the same as § 6.1 does: a surface holds one root visual node, and the
/// difference between the four roles is which protocol assigns the surface its role, not what hangs
/// under it. § 6.4 is the shortest case of all -- `child` is one of the only two properties a
/// `lock` has, and `layout::node::lock_spec` takes the other.
///
/// `textfield` (`oblisk-idl-api-specs.md` § 5.2 item 8) is a leaf like `text`/`icon`: it never
/// takes `children`. Its own properties (`mask_character`, `secure_submit`, `on_change`,
/// `on_submit`) ride along unvalidated in `RetainedNode.properties`, same as `button`'s `on_click`.
/// What *does* read `secure_submit` out of the resolved tree is `renderer/src/wayland/mod.rs`'s
/// keyboard path, off the scene graph rather than through it: the buffer and the focus target live
/// on `App`, because ADR-0005 says a masked field's bytes never become an `mlua::Value`.
fn children_of(kind: &str, properties: &HashMap<String, Value>) -> Result<Vec<VirtualNode>, LayoutError> {
    match kind {
        "panel" | "window" | "popup" | "lock" => {
            Ok(node::parse_single_child(properties, "child")?.into_iter().collect())
        }
        "rect" | "row" | "column" | "button" => node::parse_children(properties),
        // ADR-0045 decision 3: a `list`'s children don't exist as a literal Lua table, they're
        // generated from `source`, one per element, which is why this is its own parser rather
        // than a `parse_children` variant.
        "list" => node::parse_list_children(properties),
        "text" | "icon" | "image" | "textfield" => Ok(Vec::new()),
        other => unreachable!("ensure_supported_kind already rejected `{other}`"),
    }
}

/// A surface root's own size, and nothing else's. Every node below one gets its size from the
/// solver, but the root is the one node whose `Fill` and `Percent` have no parent to mean anything
/// against, so both resolve against the room the surface was configured at. `Content` stays
/// unresolved (`None`) until children are known, which is the solver's `auto`.
fn resolve_non_content(mode: SizeMode, available: f32) -> Option<f32> {
    match mode {
        SizeMode::Content => None,
        SizeMode::Pixels(n) => Some(n),
        SizeMode::Fill => Some(available),
        SizeMode::Percent(p) => Some(available * p),
    }
}

/// Pairs `fresh_children` against `old_children` (ADR-0045 decisions 1-2) by partitioning both
/// sides into identified and unidentified subsequences, because an `id` means "this is the same
/// node, and *only* the same node", in both directions:
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
fn pair_children_by_id_then_position(
    scene: &mut Scene,
    fresh_children: &[VirtualNode],
    old_children: Vec<RetainedNode>,
) -> Result<Vec<Option<RetainedNode>>, LayoutError> {
    let fresh_ids: Vec<Option<String>> =
        fresh_children.iter().map(|c| node::parse_node_id(&c.properties)).collect::<Result<_, _>>()?;

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
    let old_ids: Vec<Option<String>> =
        old_children.iter().map(|c| node::parse_node_id(&c.properties).ok().flatten()).collect();
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

/// The axis a parent lays its children out along. Absent for a stacking parent (`rect`, `button`
/// and the four surface roles), whose children each get the whole content box on both axes and so
/// have no remainder to share -- measured, and correct as it stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MainAxis {
    Horizontal,
    Vertical,
}

/// Which axis `kind` flows along, reading a `list`'s `direction` through [`flow_kind`] so a
/// horizontal `list` shares a row's rules rather than a column's.
fn main_axis_of(kind: &str, properties: &HashMap<String, Value>) -> Result<Option<MainAxis>, LayoutError> {
    Ok(match flow_kind(kind, properties)? {
        "row" => Some(MainAxis::Horizontal),
        "column" => Some(MainAxis::Vertical),
        _ => None,
    })
}

/// What the solver cannot work out from a style alone: a leaf sized by its own content.
///
/// Every other kind takes the size its `width`/`height` ask for, or the size its children add up
/// to, and taffy does both. These two do not. Parsed in [`prepare`] rather than in the measure
/// callback below, because a callback returns a `Size<f32>` and has nowhere to put a
/// [`LayoutError`] -- `icon`'s `size` is a property like any other and can be malformed.
enum Measure {
    /// A `text` node's shaped extent, against the wrap width taffy offers.
    Text { content: String, font_size: f32 },
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
}

/// `Content` is the one mode with no answer until the children are known, which is exactly
/// taffy's `auto`. `Fill` is `auto` too, and gets its meaning from the `flex_grow` or the stretch
/// [`taffy_style`] pairs it with: what "fill" means depends on what the parent is doing, and the
/// size property cannot say it on its own.
fn taffy_dimension(mode: SizeMode) -> taffy::Dimension {
    match mode {
        SizeMode::Content | SizeMode::Fill => taffy::Dimension::auto(),
        SizeMode::Pixels(n) => taffy::Dimension::length(n),
        SizeMode::Percent(p) => taffy::Dimension::percent(p),
    }
}

/// `Align` as the alignment of one item inside the slot its parent gives it.
fn item_align(align: Align) -> taffy::AlignSelf {
    match align {
        Align::Start => taffy::AlignItems::START,
        Align::Center => taffy::AlignItems::CENTER,
        Align::End => taffy::AlignItems::END,
        Align::Stretch => taffy::AlignItems::STRETCH,
    }
}

/// `Align` as a flow container's packing of its children along the axis it flows in.
///
/// `Stretch` is not a packing and never was: the hand-written pass ran its main-axis cursor from
/// the same place for `Start` and for `Stretch`, so `Stretch` on a row's own `align_h` has always
/// read as `Start`. Kept, rather than quietly promoted to a distribution taffy could express.
fn main_align(align: Align) -> taffy::JustifyContent {
    match align {
        Align::Start | Align::Stretch => taffy::JustifyContent::START,
        Align::Center => taffy::JustifyContent::CENTER,
        Align::End => taffy::JustifyContent::END,
    }
}

/// One node's taffy style: the container half (how it lays its own children out) and the item half
/// (how its parent lays *it* out), which is one value because that is how taffy's `Style` is
/// shaped.
///
/// `parent_axis` is the axis this node's parent flows along, `None` when the parent stacks
/// (ADR-0023) or when this is a surface root. It is what decides which of this node's
/// two `Fill`s is a share of a remainder and which is the whole slot: filling a row's main axis
/// means splitting what the siblings leave, and filling anything else means taking all of it.
fn taffy_style(
    kind: &str,
    properties: &HashMap<String, Value>,
    style: &LayoutStyle,
    parent_axis: Option<MainAxis>,
) -> Result<taffy::Style, LayoutError> {
    // An invisible node leaves the layout entirely: no size, no position, and no `spacing` gap on
    // either side of it. The first two are a narrowing of what the hand-written pass did, which
    // resolved a hidden subtree's geometry in full and then declined to place it; nothing outside
    // this module ever read that geometry, because `paint`, `hit` and `overlay_input_regions` all
    // filter on `visible` first. The gap is the part that was always specified, and it still holds.
    if !style.visible {
        return Ok(taffy::Style { display: taffy::Display::None, ..taffy::Style::DEFAULT });
    }

    let mut out = taffy::Style {
        // This engine has no shrink concept, and two taffy defaults have to be turned off to keep
        // it that way. `flex_shrink` is the obvious one: a fixed child keeps the width it asked
        // for even when the siblings already overflow the parent, which is the behaviour
        // `fixed_children_that_already_overflow_collapse_a_fill_sibling_to_nothing` pins. The
        // `min_size` is the other half -- taffy's automatic minimum size would floor a `Fill` item
        // at its own content instead of letting it collapse, and every budget the hand-written
        // pass computed was clamped at zero rather than at a content size.
        flex_shrink: 0.0,
        min_size: taffy::Size { width: length(0.0), height: length(0.0) },
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
        ..taffy::Style::DEFAULT
    };

    // The container half.
    match main_axis_of(kind, properties)? {
        Some(axis) => {
            out.display = taffy::Display::Flex;
            out.flex_direction = match axis {
                MainAxis::Horizontal => taffy::FlexDirection::Row,
                MainAxis::Vertical => taffy::FlexDirection::Column,
            };
            // A flow container's own `align_h`/`align_v` packs its children along the axis it
            // flows in. The other axis is each child's own business and is read off the child.
            out.justify_content = Some(main_align(match axis {
                MainAxis::Horizontal => style.align_h,
                MainAxis::Vertical => style.align_v,
            }));
            // `spacing`, between adjacent children only, which is what a flex gap is. Set on both
            // axes because a single flex line only ever spends the one it flows along.
            out.gap = taffy::Size { width: length(style.spacing), height: length(style.spacing) };
        }
        // The stacking model (ADR-0023) is a grid of exactly one cell holding every
        // child: they overlap, each is aligned independently on both axes within the full content
        // box, and the container's own `Content` size is their bounding union. That is what a
        // single auto-sized grid track does, which is why this is a `Display::Grid` rather than an
        // invented third mode. The cell is implicit -- the children pin themselves to line 1 on
        // both axes below, and taffy's auto track sizing does the union.
        None => out.display = taffy::Display::Grid,
    }

    // The item half. `Fill` on an axis this node's parent does not flow along means "the whole
    // slot", and it outranks the alignment the config wrote: the hand-written pass sized such a
    // child to the full slot first and then aligned it inside a box it had already filled, so the
    // alignment could never move it. Stretching says the same thing to taffy in one property.
    let fills_h = style.width_mode == SizeMode::Fill && parent_axis != Some(MainAxis::Horizontal);
    let fills_v = style.height_mode == SizeMode::Fill && parent_axis != Some(MainAxis::Vertical);
    let align_h = if fills_h { taffy::AlignItems::STRETCH } else { item_align(style.align_h) };
    let align_v = if fills_v { taffy::AlignItems::STRETCH } else { item_align(style.align_v) };

    // Which of the two alignments the parent actually applies, per axis. A flex item's main axis is
    // packed by the container's `justify_content`, so the item's own alignment there is not its to
    // state; a grid item in the stacking cell states both.
    let (governed_h, governed_v) = match parent_axis {
        Some(MainAxis::Horizontal) => (None, Some(align_v)),
        Some(MainAxis::Vertical) => (Some(align_h), None),
        None => (Some(align_h), Some(align_v)),
    };

    // The one place this mapping is not plain CSS. `align-self: stretch` applies only to an `auto`
    // cross size, so a child writing both `height = 5` and `align_v = "Stretch"` would keep its 5 --
    // but the hand-written pass overrode the resolved size outright, so this engine has always let
    // the stretch win, and `row_child_stretch_alignment_fills_the_cross_axis` pins it. Blanking the
    // size is how taffy is told the same thing. Kept rather than corrected because swapping the
    // solver is the change being made here; whether a stated size should outrank a stated stretch is
    // a config-facing question, and answering it silently inside a dependency swap is how a shell
    // that laid out correctly yesterday stops doing so today.
    if governed_h == Some(taffy::AlignItems::STRETCH) {
        out.size.width = taffy::Dimension::auto();
    }
    if governed_v == Some(taffy::AlignItems::STRETCH) {
        out.size.height = taffy::Dimension::auto();
    }

    match parent_axis {
        // A flex item: the parent packs it on the main axis, so only the cross alignment is its
        // own, and `Fill` on the main axis is a share of the remainder. A zero basis is what makes
        // the share the whole remainder rather than the remainder on top of a content-sized start,
        // which is the number `two_fill_siblings_split_the_remainder_equally` pins.
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
        // A grid item in the one shared cell. Both alignments are its own, which is the stacking
        // model's "each child positioned per its own `align_h`/`align_v`, independently".
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
/// Split out of [`prepare`] purely for the stack. A `taffy::Style` is 552 bytes, and [`prepare`]
/// recurses one frame per tree level with `MAX_TREE_DEPTH` of them allowed, so a `Style` built
/// there is 552 bytes multiplied by the depth cap. Built here it lives in a frame that returns
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
/// This is where everything that can run Lua or fail happens, and it happens depth-first in
/// declaration order, which is what guarantees every getter fires exactly once, in source order.
/// The hand-written pass had to work to keep that, because it
/// recursed into `Fill` children after their siblings and so had to split resolution out of the
/// recursion; here there are no rounds, so declaration order and recursion order are the same one.
///
/// `properties` and `style` arrive already resolved and parsed, done by the parent for the same
/// reason as before: [`taffy_style`] needs a child's `margin` and its size modes to build the node,
/// so the one parse has to happen in the parent's loop. `Scene::apply_one_instance` does it for a
/// surface root, which has no parent.
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
    // exists to enforce), and repeated here so this function holds its own preconditions rather
    // than trusting a call site -- notably `children_of`'s `unreachable!` arm below.
    ensure_node_admissible(kind, depth)?;

    let id = retained.as_ref().map(|r| r.id);
    let old_children = retained.map(|r| r.children).unwrap_or_default();
    let id = id.unwrap_or_else(|| scene.alloc_id());

    // Before the children, because a `text`'s measurement reads the `content` and `font_size`
    // parsed here rather than parsing them a second time.
    let paint = node::paint_style(kind, &properties)?;
    let measure = match flow_kind(kind, &properties)? {
        // `node::paint_style` gives every `text` a `PaintStyle::Text` and `flow_kind` cannot route
        // another kind here, so the arm is total -- the same shape as `children_of`'s `unreachable!`.
        "text" => {
            let Some(PaintStyle::Text { content, font_size, .. }) = paint.as_ref() else {
                unreachable!("a `text` node always carries a `PaintStyle::Text`")
            };
            Some(Measure::Text { content: content.clone(), font_size: *font_size })
        }
        "icon" => Some(Measure::Square(node::parse_icon_size(&properties)?)),
        // `image` has no intrinsic size, unlike `icon`: knowing a file's own dimensions means
        // decoding it, and this pass has no canvas to decode against and runs on every
        // `Scene::apply`. So an `image` takes the box § 5.1's `width`/`height` give it and measures
        // nothing without one, the same as an empty `rect`.
        _ => None,
    };

    let fresh_children = children_of(kind, &properties)?;
    let matched_candidates = pair_children_by_id_then_position(scene, &fresh_children, old_children)?;
    let own_axis = main_axis_of(kind, &properties)?;
    // Before the children, so their ids can be attached to it afterwards, and so the `taffy::Style`
    // behind it is gone from the stack by the time this frame recurses -- see `new_solver_node`.
    let taffy_id = new_solver_node(tree, kind, &properties, &style, parent_axis, measure)?;

    let mut children = Vec::with_capacity(fresh_children.len());
    for (fresh_child, candidate) in fresh_children.iter().zip(matched_candidates) {
        // Before this child's own getters run, not after: resolving its property map calls back
        // into Lua, and a child the walk is about to refuse must not get to execute anything on the
        // way to being refused. `depth + 1` is the level this child would occupy, so the error is
        // the same variant, kind and level the recursive call raises -- see `ensure_node_admissible`.
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

    Ok(PreparedNode { id, kind: kind.to_string(), style, properties, paint, taffy: taffy_id, children })
}

/// The second walk: solved geometry back out of the tree and into retained nodes.
///
/// taffy hands out a location relative to the parent's border box, which is the same frame
/// `layout::paint` and `layout::hit` already accumulate down, so the rect goes across untouched.
/// The two things that are still this module's own happen here, both because both need a size that
/// only exists once the solve is done: a scrolled container shifts its children, and an over-wide
/// `text` is cut to the box it ended up in.
fn finish(
    tree: &taffy::TaffyTree<Measure>,
    prepared: PreparedNode,
    shaping: &ShapingHandle,
) -> Result<RetainedNode, LayoutError> {
    let PreparedNode { id, kind, style, properties, mut paint, taffy: taffy_id, children } = prepared;
    let layout = tree.layout(taffy_id).map_err(taffy_failed)?;
    let size = LogicalSize { width: layout.size.width, height: layout.size.height };

    let mut children: Vec<RetainedNode> =
        children.into_iter().map(|child| finish(tree, child, shaping)).collect::<Result<_, _>>()?;

    // ADR-0069 decision 4. Subtracted from every child's main coordinate, so a scrolled child
    // sits before the content box and the clip `layout::paint` computes per node cuts it. The
    // extent is summed here rather than read off taffy's `scrollable_overflow_rect` because the
    // two disagree by a margin: CSS scrollable overflow is the union of the children's border
    // boxes, and this engine's `spacing`-and-margin footprint is what `Fill` was sized against and
    // what the tests pin.
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
    elide_to_fit(&mut paint, (size.width - style.padding.horizontal()).max(0.0), shaping);

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

/// How much room this container's visible children take along `axis`, margins and gaps included --
/// the number a scroll offset is clamped against. The same footprint the sizing pass used, which is
/// what keeps a scroll limit and the layout it scrolls in agreement.
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
/// resolved between them. Given a prepared tree and the room its root has, it fills in every
/// node's geometry.
///
/// The measure callback is the one place this crate is still asked a geometry question, and it is
/// asked only about the two kinds whose size is their content: a `text`'s shaped extent and an
/// `icon`'s square. taffy asks each of them a handful of times per pass rather than once, over a
/// small set of distinct `(text, size, wrap width)` tuples, and `ShapingHandle`'s memo is keyed on
/// exactly that tuple, so every repeat after the first is answered without crossing the channel.
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
                    Measure::Text { content, font_size } => {
                        // The wrap boundary: the width this box is already known to have, or the
                        // width on offer when it is not. `MaxContent`/`MinContent` mean taffy is
                        // asking what the string wants rather than offering it a box, and an
                        // unconstrained measurement is the honest answer to that.
                        let max_width = known.width.or(match offered.width {
                            taffy::AvailableSpace::Definite(width) => Some(width),
                            taffy::AvailableSpace::MinContent | taffy::AvailableSpace::MaxContent => None,
                        });
                        let shaped = shaping.shape(ShapeRequest {
                            text: content.clone(),
                            font_size: *font_size,
                            line_height: *font_size * 1.2,
                            max_width,
                        });
                        taffy::Size { width: shaped.width, height: shaped.height }
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
/// A `list` is a repeater, not a third layout: it reconciles children by key and then stacks them,
/// and "stacks them" is a `column` or a `row` and nothing else. Routing to the existing arms is what
/// keeps a horizontal list identical to a hand-built `row` in spacing, margins, alignment and
/// stretch, rather than a second implementation that agrees with it until it does not.
fn flow_kind<'a>(kind: &'a str, properties: &HashMap<String, Value>) -> Result<&'a str, LayoutError> {
    if kind == "list" { node::parse_list_direction(properties) } else { Ok(kind) }
}

/// The `Signal` behind a node's `scroll` property, or `None` if it declares none.
///
/// Unresolved in the slot because `layout::node::is_structural_property` says so, the same way
/// `hover` arrives (ADR-0062 decision 3, ADR-0069 decision 4). Anything else there -- a
/// number, a `state()` signal, a capability -- is silently inert rather than an error, matching
/// `layout::hover::hover_signal`: `Signal::scroll_handle` refuses every kind this must not write,
/// so a config naming the wrong thing gets no scrolling instead of a wheel writing somewhere it
/// should not.
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

/// How far this container is scrolled along its main axis, clamped to what there is to scroll, and
/// written back so the signal holds the offset actually used (ADR-0069 decision 4).
///
/// `content_main` is the viewport and `total_main` the content, both already computed by the caller
/// for its own alignment arithmetic, which is why this needed no new parameter threaded through the
/// layout recursion.
///
/// A container with nothing to scroll returns 0 rather than erroring, so a `Content`-sized column
/// (whose content and viewport are the same number by construction) is a no-op. That is the answer
/// `Fill` gives in a `Content` parent and for the same reason: there is no remainder (decision 5).
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

/// Rewrites an over-wide `text` to the longest prefix that fits, finished with an ellipsis.
///
/// Runs here rather than in `layout::paint` because this is the only place both halves are in
/// reach: the box width is not known until this node has been sized, and the shaping worker is not
/// reachable from a display-list build, which is pure by design.
///
/// Does nothing when the text already fits, and nothing on a `Content`-sized node, whose box came
/// from measuring this same string and therefore always fits it.
///
/// ponytail: a binary search over character prefixes, so roughly ten `ShapingHandle::shape` calls
/// the first time a given string elides in a given box. Every one of them is a cache key
/// (`text::shaping`'s memo), so a string that elided last pass costs nothing this pass, and the
/// search only reruns when the string or the box changes. Before that cache existed this would have
/// been ten blocking round trips per elided node per frame, which is why it is written this way now
/// and would not have been then. Cutting at a character boundary rather than a grapheme cluster is
/// the real ceiling: an emoji with a skin-tone modifier can lose the modifier and change what it
/// draws. Nothing in this shell's own strings does that, and window titles arriving from outside it
/// eventually will, so a `unicode-segmentation` pass over grapheme boundaries is the upgrade path.
fn elide_to_fit(paint: &mut Option<PaintStyle>, content_width: f32, shaping: &ShapingHandle) {
    let Some(PaintStyle::Text { content, font_size, elide: node::Elide::End, .. }) = paint.as_mut() else {
        return;
    };
    if content.is_empty() || content_width <= 0.0 {
        return;
    }
    let measure = |text: &str| {
        shaping
            .shape(ShapeRequest {
                text: text.to_string(),
                font_size: *font_size,
                line_height: *font_size * 1.2,
                max_width: None,
            })
            .width
    };
    if measure(content) <= content_width {
        return;
    }

    // Byte offsets a prefix may be cut at, so the search never lands inside a codepoint. The last
    // entry is the start of the final character, which is the longest prefix worth trying: the whole
    // string is already known not to fit.
    let cuts: Vec<usize> = content.char_indices().map(|(index, _)| index).collect();
    // Largest index into `cuts` whose prefix plus an ellipsis still fits. Zero is always admissible
    // and means the ellipsis alone, the honest answer for a box too narrow for even one character.
    let (mut low, mut high) = (0usize, cuts.len() - 1);
    while low < high {
        // Rounded up, so `mid` is always above `low` and the loop cannot stall; `high` is only ever
        // assigned `mid - 1`, and `mid` is at least 1 whenever this body runs.
        let mid = low + (high - low).div_ceil(2);
        if measure(&format!("{}\u{2026}", &content[..cuts[mid]])) <= content_width {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    *content = format!("{}\u{2026}", &content[..cuts[low]]);
}

/// § 5.1's input-region scan: `surface_root`'s direct, visible children, projected to physical
/// pixels. Pure, and the `wl_region`/`wl_surface::set_input_region` push it feeds lives in
/// `crate::wayland::App::apply_input_region`, which is the only place a Wayland object exists to
/// push it to.
///
/// Per surface since ADR-0038 decision 5. Applies to any surface whose visible content is
/// smaller than the surface itself -- load-bearing for a fullscreen transparent panel, an empty
/// region (so clicks pass straight through) for one with nothing visible in it, and a no-op for a
/// tightly-sized bar whose child fills it.
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
        assert_eq!(
            (hidden.children[0].rect.width, hidden.children[0].rect.height),
            (0.0, 0.0),
            "and everything under it"
        );
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
    /// Alignment and scrolling can never both be in play, and this pins why rather than trusting it.
    ///
    /// `spare` is `(content - total).max(0)` and the scroll limit is `(total - content).max(0)`, so
    /// one is zero whenever the other is not. Content that underfills its box aligns and cannot
    /// scroll; content that overflows scrolls and has no spare to align with. A `Center` column with
    /// a scroll offset set is therefore still centred, not centred-then-shifted.
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
        fn find(node: &ResolvedNode) -> Option<String> {
            if let Some(PaintStyle::Text { content, .. }) = &node.paint {
                return Some(content.clone());
            }
            node.children.iter().find_map(find)
        }
        find(&scene.surface("bar@TEST").unwrap()).expect("expected a text node")
    }

    fn elided(lua_src: &str) -> String {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(lua_src);
        apply_at(&mut scene, &[surface], full(), &shaping, &_lua).unwrap();
        drawn_text(&scene)
    }

    const LONG: &str = "a window title far too long for the box it was given";

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

    #[test]
    fn overlay_input_regions_includes_only_visible_direct_children() {
        let visible_child = ResolvedNode {
            kind: "rect".to_string(),
            rect: LogicalRect { x: 0.0, y: 0.0, width: 10.0, height: 10.0 },
            visible: true,
            opacity: 1.0,
            properties: HashMap::new(),
            paint: None,
            children: Vec::new(),
        };
        let hidden_child = ResolvedNode {
            kind: "rect".to_string(),
            rect: LogicalRect { x: 20.0, y: 20.0, width: 10.0, height: 10.0 },
            visible: false,
            opacity: 1.0,
            properties: HashMap::new(),
            paint: None,
            children: Vec::new(),
        };
        let root = ResolvedNode {
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
            kind: "rect".to_string(),
            rect: LogicalRect { x: 0.0, y: 0.0, width: 100.0, height: 100.0 },
            visible: false,
            opacity: 1.0,
            properties: HashMap::new(),
            paint: None,
            children: Vec::new(),
        };
        let mut root = ResolvedNode {
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
            kind: "panel".to_string(),
            rect: LogicalRect { x: 0.0, y: 0.0, width: 1920.0, height: 32.0 },
            visible: true,
            opacity: 1.0,
            properties: HashMap::new(),
            paint: None,
            children: vec![ResolvedNode {
                kind: "row".to_string(),
                rect: LogicalRect { x: 0.0, y: 0.0, width: 1920.0, height: 32.0 },
                visible: true,
                opacity: 1.0,
                properties: HashMap::new(),
                paint: None,
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
