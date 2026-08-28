//! The one-pass resolve algorithm and the retained-scene transaction
//! (`docs/oblisk-layout-engine-geometry.md` § 3-5, `CONTEXT.md`'s "Retained scene" entries).
//!
//! `resolve_and_reconcile` is the single recursive walk doing all three of § 3's passes
//! (constraint down, size up, position down) per node in one recursion, matching the spec's
//! literal "single... pass" language, while simultaneously matching fresh nodes to retained ones
//! by position (§ 4) and retiring removed subtrees child-first (`CONTEXT.md`, Lease).

use std::collections::HashMap;

use mlua::{Lua, Value};

use crate::layout::node::{self, Align, EdgeInsets, LayoutError, SizeMode};
use crate::lua::nodes::VirtualNode;
use crate::text::shaping::{ShapeRequest, ShapingHandle};
use crate::text::snap::{LogicalRect, PhysicalRect, snap_to_physical};

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LogicalSize {
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(u64);

/// The public, ID-less output of one node's resolution: geometry plus a full passthrough of its
/// raw properties for a future paint stage. Reused by `Scene::surface` and by
/// `overlay_input_regions`.
#[derive(Debug, Clone)]
pub struct ResolvedNode {
    pub kind: String,
    pub rect: LogicalRect,
    pub visible: bool,
    pub properties: HashMap<String, Value>,
    pub children: Vec<ResolvedNode>,
}

/// One retained node: `ResolvedNode`'s geometry plus the `NodeId` identity that lets the next
/// `Scene::apply` decide whether to reuse it or retire it.
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

/// The persistent node tree for one generation (`CONTEXT.md`, Retained scene), keyed per surface
/// by that surface's own `id` property (§ 6.1) -- surfaces are identified, not ordered, unlike
/// everything below a surface's root, which reconciles positionally (§ 4).
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

    /// Reconciles every entry in `fresh_surfaces` into the retained scene, resolving geometry
    /// against `available` (the placeholder output size -- see docs/adr/0023 item 6). A surface
    /// id present in the retained scene but absent from `fresh_surfaces` this cycle is left
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
    /// O(nodes), not O(Lua heap). Item 5 is the real fix: resolving every property once up front
    /// means the fallible work finishes before any of `next_id`/`retiring`/`surfaces` is touched,
    /// and a walk that cannot fail partway needs no snapshot at all.
    pub fn apply(
        &mut self,
        fresh_surfaces: &[VirtualNode],
        available: LogicalSize,
        shaping: &ShapingHandle,
        lua: &Lua,
    ) -> Result<(), LayoutError> {
        let next_id_snapshot = self.next_id;
        let retiring_snapshot_len = self.retiring.len();
        let surfaces_snapshot = self.surfaces.clone();

        for fresh in fresh_surfaces {
            if let Err(err) = self.apply_one_surface(fresh, available, shaping, lua) {
                self.surfaces = surfaces_snapshot;
                self.next_id = next_id_snapshot;
                self.retiring.truncate(retiring_snapshot_len);
                return Err(err);
            }
        }
        Ok(())
    }

    /// One surface's worth of `apply`'s loop body, split out so `apply` can wrap it in a single
    /// early-return-on-error site instead of duplicating the rollback at every `?`.
    fn apply_one_surface(
        &mut self,
        fresh: &VirtualNode,
        available: LogicalSize,
        shaping: &ShapingHandle,
        lua: &Lua,
    ) -> Result<(), LayoutError> {
        let key = node::parse_surface_id(&fresh.properties)?;
        let existing = self.surfaces.remove(&key);
        let reconciled = resolve_and_reconcile(self, existing, fresh, available, shaping, None, None, lua)?;
        self.surfaces.insert(key, reconciled);
        Ok(())
    }

    pub fn surface(&self, id: &str) -> Option<ResolvedNode> {
        self.surfaces.get(id).map(RetainedNode::to_resolved)
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

fn ensure_supported_kind(kind: &str) -> Result<(), LayoutError> {
    match kind {
        "surface" | "rect" | "row" | "column" | "text" | "icon" | "button" | "textfield" => Ok(()),
        other => Err(LayoutError::UnsupportedNodeKind(other.to_string())),
    }
}

/// Dispatches to the right raw property (`child` for `surface`, `children` for the container
/// kinds, none for leaves) -- only called after [`ensure_supported_kind`] already validated
/// `node.kind`, so the fallback arm is unreachable, not a silent default.
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
fn children_of(node: &VirtualNode, lua: &Lua) -> Result<Vec<VirtualNode>, LayoutError> {
    match node.kind.as_str() {
        "surface" => Ok(node::parse_single_child(&node.properties, "child", lua)?
            .into_iter()
            .collect()),
        "rect" | "row" | "column" | "button" => node::parse_children(&node.properties, lua),
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
    lua: &Lua,
) -> Result<(Option<f32>, Option<f32>), LayoutError> {
    match parent_kind {
        "row" => {
            let cross = node::parse_align(child_properties, "align_v", lua)?;
            let forced_h = (cross == Align::Stretch && own_height_known.is_some()).then_some(margined_budget.height);
            Ok((None, forced_h))
        }
        "column" => {
            let cross = node::parse_align(child_properties, "align_h", lua)?;
            let forced_w = (cross == Align::Stretch && own_width_known.is_some()).then_some(margined_budget.width);
            Ok((forced_w, None))
        }
        "rect" | "button" | "surface" => {
            let align_h = node::parse_align(child_properties, "align_h", lua)?;
            let align_v = node::parse_align(child_properties, "align_v", lua)?;
            let forced_w = (align_h == Align::Stretch && own_width_known.is_some()).then_some(margined_budget.width);
            let forced_h = (align_v == Align::Stretch && own_height_known.is_some()).then_some(margined_budget.height);
            Ok((forced_w, forced_h))
        }
        _ => Ok((None, None)),
    }
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
// Eight parameters, but each is load-bearing for this single recursive pass (§ 3's "single...
// pass", see the module doc comment); splitting them into a struct would just be a bag carrying
// the same eight fields through the same one caller.
#[allow(clippy::too_many_arguments)]
fn resolve_and_reconcile(
    scene: &mut Scene,
    retained: Option<RetainedNode>,
    fresh: &VirtualNode,
    available: LogicalSize,
    shaping: &ShapingHandle,
    forced_width: Option<f32>,
    forced_height: Option<f32>,
    lua: &Lua,
) -> Result<RetainedNode, LayoutError> {
    ensure_supported_kind(&fresh.kind)?;

    let id = retained.as_ref().map(|r| r.id);
    let old_children = retained.map(|r| r.children).unwrap_or_default();
    let id = id.unwrap_or_else(|| scene.alloc_id());

    let padding = node::parse_edge_insets(&fresh.properties, "padding", lua)?;
    let visible = node::parse_visible(&fresh.properties, lua)?;
    let width_mode = node::parse_size_mode(&fresh.properties, "width", lua)?;
    let height_mode = node::parse_size_mode(&fresh.properties, "height", lua)?;

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

    let fresh_children = children_of(fresh, lua)?;
    let mut old_iter = old_children.into_iter();
    let mut new_children = Vec::with_capacity(fresh_children.len());
    for fresh_child in &fresh_children {
        let candidate = old_iter.next();
        let reusable = match candidate {
            Some(c) if c.kind == fresh_child.kind => Some(c),
            Some(stale) => {
                scene.retire_child_first(stale);
                None
            }
            None => None,
        };

        // § 3.1: a child's own margin comes out of the same available-space budget its parent's
        // padding already inset -- subtracted here, per child, since each child can carry a
        // different margin.
        let child_margin = node::parse_edge_insets(&fresh_child.properties, "margin", lua)?;
        let margined_budget = LogicalSize {
            width: (child_budget.width - child_margin.left - child_margin.right).max(0.0),
            height: (child_budget.height - child_margin.top - child_margin.bottom).max(0.0),
        };
        let (child_forced_width, child_forced_height) = stretch_forced_size(
            &fresh.kind,
            &fresh_child.properties,
            own_width_known,
            own_height_known,
            margined_budget,
            lua,
        )?;

        new_children.push(resolve_and_reconcile(
            scene,
            reusable,
            fresh_child,
            margined_budget,
            shaping,
            child_forced_width,
            child_forced_height,
            lua,
        )?);
    }
    // Fresh list shorter than the retained one: everything left over was removed this cycle.
    for leftover in old_iter {
        scene.retire_child_first(leftover);
    }

    let intrinsic = intrinsic_content_size(fresh, &new_children, text_wrap_width, shaping, lua)?;
    let own_width = own_width_known.unwrap_or(intrinsic.width);
    let own_height = own_height_known.unwrap_or(intrinsic.height);
    let size = LogicalSize {
        width: own_width,
        height: own_height,
    };

    position_children(fresh, &mut new_children, size, padding, own_width_known, own_height_known, lua)?;

    Ok(RetainedNode {
        id,
        kind: fresh.kind.clone(),
        rect: LogicalRect {
            x: 0.0,
            y: 0.0,
            width: size.width,
            height: size.height,
        },
        visible,
        properties: fresh.properties.clone(),
        children: new_children,
    })
}

/// § 3.2's bottom-up size resolution, per kind. An invisible child contributes nothing here (see
/// `resolve_and_reconcile`'s doc comment on the collapse-vs-reserve-space choice) -- filtered out
/// before the row/column sum and the stacking union.
fn intrinsic_content_size(
    node: &VirtualNode,
    children: &[RetainedNode],
    text_wrap_width: f32,
    shaping: &ShapingHandle,
    lua: &Lua,
) -> Result<LogicalSize, LayoutError> {
    match node.kind.as_str() {
        "text" => {
            let content = node::parse_content(&node.properties, lua)?;
            let font_size = node::parse_font_size(&node.properties, lua)?;
            // A wrap width of 0 means nothing is known yet (a Content-sized text inside a
            // Content-sized ancestor with no room resolved so far) -- treat that as unconstrained
            // rather than forcing every word onto its own line.
            let max_width = (text_wrap_width > 0.0).then_some(text_wrap_width);
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
            let size = node::parse_icon_size(&node.properties, lua)?;
            Ok(LogicalSize {
                width: size,
                height: size,
            })
        }
        "rect" if children.is_empty() => Ok(LogicalSize::default()),
        "row" => {
            let spacing = node::parse_spacing(&node.properties, lua)?;
            let visible: Vec<&RetainedNode> = children.iter().filter(|c| c.visible).collect();
            // § 3.1: a child's margin travels with it -- its footprint on the row's main axis is
            // its own width plus its margin, matching `position_children`'s identical footprint
            // math (the two must agree, or a child would be sized to fit but then overlap or
            // leave a gap once positioned).
            let width = visible
                .iter()
                .map(|c| Ok(c.rect.width + node::parse_edge_insets(&c.properties, "margin", lua)?.horizontal()))
                .collect::<Result<Vec<f32>, LayoutError>>()?
                .into_iter()
                .sum::<f32>()
                + spacing * visible.len().saturating_sub(1) as f32;
            let height = visible
                .iter()
                .map(|c| Ok(c.rect.height + node::parse_edge_insets(&c.properties, "margin", lua)?.vertical()))
                .collect::<Result<Vec<f32>, LayoutError>>()?
                .into_iter()
                .fold(0.0_f32, f32::max);
            Ok(LogicalSize { width, height })
        }
        "column" => {
            let spacing = node::parse_spacing(&node.properties, lua)?;
            let visible: Vec<&RetainedNode> = children.iter().filter(|c| c.visible).collect();
            let height = visible
                .iter()
                .map(|c| Ok(c.rect.height + node::parse_edge_insets(&c.properties, "margin", lua)?.vertical()))
                .collect::<Result<Vec<f32>, LayoutError>>()?
                .into_iter()
                .sum::<f32>()
                + spacing * visible.len().saturating_sub(1) as f32;
            let width = visible
                .iter()
                .map(|c| Ok(c.rect.width + node::parse_edge_insets(&c.properties, "margin", lua)?.horizontal()))
                .collect::<Result<Vec<f32>, LayoutError>>()?
                .into_iter()
                .fold(0.0_f32, f32::max);
            Ok(LogicalSize { width, height })
        }
        // Stacking model (rect-with-children, button, surface): § 3.2 gives no formula for a
        // container that isn't row/column -- docs/adr/0023 item 4 documents this as this phase's
        // own interpretation. Content size is the bounding union over independently-positioned
        // children, each inflated by its own margin.
        _ => {
            let visible: Vec<&RetainedNode> = children.iter().filter(|c| c.visible).collect();
            let width = visible
                .iter()
                .map(|c| Ok(c.rect.width + node::parse_edge_insets(&c.properties, "margin", lua)?.horizontal()))
                .collect::<Result<Vec<f32>, LayoutError>>()?
                .into_iter()
                .fold(0.0_f32, f32::max);
            let height = visible
                .iter()
                .map(|c| Ok(c.rect.height + node::parse_edge_insets(&c.properties, "margin", lua)?.vertical()))
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
    node: &VirtualNode,
    children: &mut [RetainedNode],
    size: LogicalSize,
    padding: EdgeInsets,
    own_width_known: Option<f32>,
    own_height_known: Option<f32>,
    lua: &Lua,
) -> Result<(), LayoutError> {
    let content_x = padding.left;
    let content_y = padding.top;
    let content_width = (size.width - padding.left - padding.right).max(0.0);
    let content_height = (size.height - padding.top - padding.bottom).max(0.0);

    let margins: Vec<EdgeInsets> = children
        .iter()
        .map(|c| node::parse_edge_insets(&c.properties, "margin", lua))
        .collect::<Result<_, _>>()?;

    match node.kind.as_str() {
        "row" => {
            let spacing = node::parse_spacing(&node.properties, lua)?;
            let main_align = node::parse_align(&node.properties, "align_h", lua)?;
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
                let cross_align = node::parse_align(&children[i].properties, "align_v", lua)?;
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
        "column" => {
            let spacing = node::parse_spacing(&node.properties, lua)?;
            let main_align = node::parse_align(&node.properties, "align_v", lua)?;
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
                let cross_align = node::parse_align(&children[i].properties, "align_h", lua)?;
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
        // item 4.
        _ => {
            for (i, child) in children.iter_mut().enumerate() {
                if !child.visible {
                    continue;
                }
                let align_h = node::parse_align(&child.properties, "align_h", lua)?;
                let align_v = node::parse_align(&child.properties, "align_v", lua)?;
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

/// § 5.1's overlay input-region scan: `overlay_root`'s direct, visible children, projected to
/// physical pixels. Pure -- see docs/adr/0023 item 5 for why the real `wl_region`/
/// `wl_surface::set_input_region` push isn't wired here.
///
/// ponytail: no production caller yet. The thread boundary that used to make one impossible is
/// gone (docs/adr/0039 put the `Scene` and the `wl_surface`s on the same thread), so this is now
/// one direct call away; making it is build-steps.md Phase 20's job, not the thread move's.
/// Exercised by this module's own tests only.
#[allow(dead_code)]
pub fn overlay_input_regions(overlay_root: &ResolvedNode, scale: f32) -> Vec<PhysicalRect> {
    overlay_root
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

    #[test]
    fn a_signal_valued_width_resolves_to_its_current_value_in_the_resolved_node() {
        // The Scene::apply seam (ADR-0044 decision 1): a Signal in a geometry slot must reach
        // the ResolvedNode's rect, not error the whole apply.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        register_node_constructors(&lua).unwrap();
        crate::lua::signal::register(&lua).unwrap();
        let signal = crate::lua::signal::Signal::new_live(Value::Integer(40)).0;
        lua.globals().set("w", signal).unwrap();
        let table: mlua::Table = lua
            .load(r#"return surface { id = "bar", child = rect { width = w, height = 20 } }"#)
            .eval()
            .unwrap();
        let surface = deserialize_lua_table(&table).unwrap();

        scene.apply(&[surface], full(), &shaping, &lua).unwrap();

        let child = &scene.surface("bar").unwrap().children[0];
        assert_eq!(child.rect.width, 40.0, "a Signal-valued width must resolve at layout time");
    }

    #[test]
    fn a_pixels_sized_rect_resolves_to_its_explicit_size() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) =
            surface_from(r#"surface { id = "bar", child = rect { width = 40, height = 20 } }"#);
        scene.apply(&[surface], full(), &shaping, &_lua).unwrap();
        let root = scene.surface("bar").unwrap();
        let child = &root.children[0];
        assert_eq!(child.rect.width, 40.0);
        assert_eq!(child.rect.height, 20.0);
    }

    #[test]
    fn a_childless_rect_with_no_explicit_size_resolves_to_zero() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(r#"surface { id = "bar", child = rect {} }"#);
        scene.apply(&[surface], full(), &shaping, &_lua).unwrap();
        let child = &scene.surface("bar").unwrap().children[0];
        assert_eq!(child.rect.width, 0.0);
        assert_eq!(child.rect.height, 0.0);
    }

    #[test]
    fn a_fill_child_takes_its_parents_available_bounds() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"surface { id = "bar", width = 1000, height = 500, child = rect { width = "Fill", height = "Fill" } }"#,
        );
        scene.apply(&[surface], full(), &shaping, &_lua).unwrap();
        let child = &scene.surface("bar").unwrap().children[0];
        assert_eq!(child.rect.width, 1000.0);
        assert_eq!(child.rect.height, 500.0);
    }

    #[test]
    fn a_percent_child_scales_against_its_parents_available_bounds() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"surface { id = "bar", width = 1000, height = 500, child = rect { width = "50%" } }"#,
        );
        scene.apply(&[surface], full(), &shaping, &_lua).unwrap();
        let child = &scene.surface("bar").unwrap().children[0];
        assert_eq!(child.rect.width, 500.0);
    }

    #[test]
    fn row_intrinsic_width_sums_children_plus_spacing_gaps() {
        // Hand-computed: two 10-wide children + 1 gap of 5 = 25, independent of the resolve code.
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"surface { id = "bar", child = row { spacing = 5, children = { rect { width = 10, height = 8 }, rect { width = 10, height = 4 } } } }"#,
        );
        scene.apply(&[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar").unwrap().children[0];
        assert_eq!(row.rect.width, 25.0, "10 + 10 + 5 spacing");
        assert_eq!(row.rect.height, 8.0, "max of children's heights");
    }

    #[test]
    fn column_intrinsic_height_sums_children_plus_spacing_gaps() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"surface { id = "bar", child = column { spacing = 3, children = { rect { width = 6, height = 10 }, rect { width = 9, height = 10 } } } }"#,
        );
        scene.apply(&[surface], full(), &shaping, &_lua).unwrap();
        let column = &scene.surface("bar").unwrap().children[0];
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
            r#"surface { id = "bar", child = row { children = { rect { width = 10, height = 10, margin = { left = 4, right = 4 } } } } }"#,
        );
        scene.apply(&[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar").unwrap().children[0];
        assert_eq!(row.rect.width, 18.0, "10 + 4 + 4 margin");
        assert_eq!(row.children[0].rect.x, 4.0, "the child's own margin.left offsets it inward");
    }

    #[test]
    fn a_margined_row_child_pushes_its_sibling_apart_instead_of_overlapping() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"surface { id = "bar", child = row { children = {
                rect { width = 10, height = 10, margin = { right = 5 } },
                rect { width = 10, height = 10 },
            } } }"#,
        );
        scene.apply(&[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar").unwrap().children[0];
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
            r#"surface { id = "bar", width = 100, height = 50, child = row { height = "Fill", children = {
                rect { width = 20, align_v = "Stretch", children = {
                    rect { width = 6, height = 6, align_h = "Center", align_v = "Center" },
                } },
            } } }"#,
        );
        scene.apply(&[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar").unwrap().children[0];
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
            r#"surface { id = "bar", child = row { children = { rect { width = 10, height = 10 }, rect { width = 10, height = 10 } } } }"#,
        );
        scene.apply(&[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar").unwrap().children[0];
        assert_eq!(row.children[0].rect.x, 0.0);
        assert_eq!(row.children[1].rect.x, 10.0);
    }

    #[test]
    fn row_end_alignment_packs_children_against_the_far_edge() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"surface { id = "bar", width = 100, height = 20, child = row { width = "Fill", align_h = "End", children = { rect { width = 10, height = 10 } } } }"#,
        );
        scene.apply(&[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar").unwrap().children[0];
        assert_eq!(row.children[0].rect.x, 90.0);
    }

    #[test]
    fn row_child_stretch_alignment_fills_the_cross_axis() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"surface { id = "bar", width = 100, height = 50, child = row { height = "Fill", children = { rect { width = 10, height = 5, align_v = "Stretch" } } } }"#,
        );
        scene.apply(&[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar").unwrap().children[0];
        assert_eq!(row.children[0].rect.height, 50.0);
    }

    #[test]
    fn stacking_container_aligns_each_child_independently_and_they_can_overlap() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"surface { id = "bar", width = 100, height = 100, child = rect { width = "Fill", height = "Fill", children = {
                rect { width = 20, height = 20, align_h = "Start", align_v = "Start" },
                rect { width = 20, height = 20, align_h = "End", align_v = "End" },
            } } }"#,
        );
        scene.apply(&[surface], full(), &shaping, &_lua).unwrap();
        let outer = &scene.surface("bar").unwrap().children[0];
        assert_eq!(outer.children[0].rect.x, 0.0);
        assert_eq!(outer.children[1].rect.x, 80.0);
        assert_eq!(outer.children[1].rect.y, 80.0);
    }

    #[test]
    fn an_invisible_child_does_not_consume_row_space() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"surface { id = "bar", child = row { children = {
                rect { width = 10, height = 10, visible = false },
                rect { width = 10, height = 10 },
            } } }"#,
        );
        scene.apply(&[surface], full(), &shaping, &_lua).unwrap();
        let row = &scene.surface("bar").unwrap().children[0];
        assert_eq!(
            row.rect.width, 10.0,
            "the invisible child must not widen the row or add a spacing gap"
        );
        assert_eq!(
            row.children[1].rect.x, 0.0,
            "the visible child packs at the start as if the hidden one weren't there"
        );
    }

    #[test]
    fn text_content_size_comes_from_a_real_shaping_round_trip() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) =
            surface_from(r#"surface { id = "bar", child = text { content = "Oblisk" } }"#);
        scene.apply(&[surface], full(), &shaping, &_lua).unwrap();
        let text = &scene.surface("bar").unwrap().children[0];
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
        let (_lua, surface) = surface_from(r#"surface { id = "bar", child = list {} }"#);
        let err = scene.apply(&[surface], full(), &shaping, &_lua).unwrap_err();
        assert!(matches!(err, LayoutError::UnsupportedNodeKind(k) if k == "list"));
    }

    #[test]
    fn an_unsupported_kind_nested_inside_children_is_rejected() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) =
            surface_from(r#"surface { id = "bar", child = row { children = { list {} } } }"#);
        let err = scene.apply(&[surface], full(), &shaping, &_lua).unwrap_err();
        assert!(matches!(err, LayoutError::UnsupportedNodeKind(k) if k == "list"));
    }

    #[test]
    fn textfield_is_a_supported_leaf_kind_carrying_its_properties_unvalidated() {
        // build-steps.md Phase 15 item 2 / ADR-0027: `textfield` becomes a valid, parseable
        // scene-node kind here; no GPU painting or `wp-text-input-v3` wiring reads its
        // properties from the scene graph in this slice (see `children_of`'s doc comment).
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua, surface) = surface_from(
            r#"surface { id = "bar", child = row { children = { textfield { mask_character = "*", secure_submit = { capability = "polkit", action = "authenticate" } } } } }"#,
        );
        scene.apply(&[surface], full(), &shaping, &_lua).unwrap();

        let field = &scene.surface("bar").unwrap().children[0].children[0];
        assert_eq!(field.kind, "textfield");
        assert!(field.children.is_empty(), "textfield is a leaf, never a container");
        assert_eq!(
            field.properties.get("mask_character").unwrap().as_string().unwrap().to_string_lossy(),
            "*"
        );
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
            r#"surface { id = "bar", child = row { children = {
                rect { width = 10, height = 10 },
                rect { width = 20, height = 20 },
            } } }"#,
        );
        scene.apply(&[surface_v1], full(), &shaping, &_lua1).unwrap();

        let ids_before: Vec<NodeId> = {
            let root = scene.surfaces.get("bar").unwrap();
            let row = &root.children[0];
            vec![root.id, row.id, row.children[0].id, row.children[1].id]
        };
        let next_id_before = scene.next_id;
        let retiring_before = scene.retiring_ids();

        // A getter that errors on read: `resolve_property` propagates that as `LayoutError`
        // partway through resolving the row's second child's `width`, after the first child (and
        // the row/surface's own properties) already resolved successfully this pass.
        let lua2 = mlua::Lua::new();
        register_node_constructors(&lua2).unwrap();
        crate::lua::signal::register(&lua2).unwrap();
        let table: mlua::Table = lua2
            .load(
                r#"
                local bad_width = computed({}, function() error("boom") end)
                return surface { id = "bar", child = row { children = {
                    rect { width = 10, height = 10 },
                    rect { width = bad_width, height = 20 },
                } } }
                "#,
            )
            .eval()
            .unwrap();
        let surface_v2 = deserialize_lua_table(&table).unwrap();

        let err = scene.apply(&[surface_v2], full(), &shaping, &lua2).unwrap_err();
        assert!(matches!(err, LayoutError::InvalidProperty { property, .. } if property == "width"));

        let root = scene.surfaces.get("bar").unwrap();
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
            surface_from(r#"surface { id = "bar", child = rect { width = 10, height = 10 } }"#);
        scene.apply(&[surface_v1], full(), &shaping, &_lua1).unwrap();
        let first_id = {
            let key = "bar";
            scene.surfaces.get(key).unwrap().children[0].id
        };

        let (_lua2, surface_v2) =
            surface_from(r#"surface { id = "bar", child = rect { width = 99, height = 99 } }"#);
        scene.apply(&[surface_v2], full(), &shaping, &_lua2).unwrap();
        let second_id = scene.surfaces.get("bar").unwrap().children[0].id;

        assert_eq!(
            first_id, second_id,
            "same kind at the same position must reuse the retained node's identity"
        );
        assert_eq!(
            scene.surface("bar").unwrap().children[0].rect.width,
            99.0,
            "but its geometry must still refresh"
        );
    }

    #[test]
    fn a_kind_change_at_the_same_index_replaces_and_retires_the_old_node() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) =
            surface_from(r#"surface { id = "bar", child = rect { width = 10, height = 10 } }"#);
        scene.apply(&[surface_v1], full(), &shaping, &_lua1).unwrap();
        assert!(scene.retiring_ids().is_empty());

        let (_lua2, surface_v2) =
            surface_from(r#"surface { id = "bar", child = text { content = "hi" } }"#);
        scene.apply(&[surface_v2], full(), &shaping, &_lua2).unwrap();

        assert_eq!(scene.surface("bar").unwrap().children[0].kind, "text");
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
            r#"surface { id = "bar", child = row { children = { rect { width = 1, height = 1 }, rect { width = 2, height = 2 } } } }"#,
        );
        scene.apply(&[surface_v1], full(), &shaping, &_lua1).unwrap();

        let (_lua2, surface_v2) = surface_from(
            r#"surface { id = "bar", child = row { children = { rect { width = 1, height = 1 } } } }"#,
        );
        scene.apply(&[surface_v2], full(), &shaping, &_lua2).unwrap();

        assert_eq!(scene.surface("bar").unwrap().children[0].children.len(), 1);
        assert_eq!(scene.retiring_ids().len(), 1);
    }

    #[test]
    fn child_first_teardown_order_visits_children_before_their_parent() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (_lua1, surface_v1) = surface_from(
            r#"surface { id = "bar", child = row { children = { rect { width = 1, height = 1, children = { rect { width = 1, height = 1 } } } } } }"#,
        );
        scene.apply(&[surface_v1], full(), &shaping, &_lua1).unwrap();
        let outer_id = scene.surfaces.get("bar").unwrap().children[0].children[0].id;
        let inner_id = scene.surfaces.get("bar").unwrap().children[0].children[0].children[0].id;

        // Remove the whole subtree by shrinking the row to zero children.
        let (_lua2, surface_v2) =
            surface_from(r#"surface { id = "bar", child = row { children = {} } }"#);
        scene.apply(&[surface_v2], full(), &shaping, &_lua2).unwrap();

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
            surface_from(r#"surface { id = "bar", child = rect { width = 1, height = 1 } }"#);
        scene.apply(&[surface_v1], full(), &shaping, &_lua1).unwrap();
        let (_lua2, surface_v2) =
            surface_from(r#"surface { id = "bar", child = text { content = "x" } }"#);
        scene.apply(&[surface_v2], full(), &shaping, &_lua2).unwrap();

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
            kind: "surface".to_string(),
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
}
