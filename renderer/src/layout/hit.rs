//! Pointer hit-testing: the chain of nodes under one point (docs/adr/0050 decision 1).
//!
//! Pure, and that is the whole reason it is a module rather than three functions inside
//! `crate::wayland`: everything else on the pointer path needs a live `wl_pointer` and a live
//! `wl_surface`, and this is the part that decides what a click means.

use crate::layout::scene::ResolvedNode;
use crate::text::snap::LogicalRect;

/// A point in one surface's logical coordinates -- the space `wl_pointer`'s `position` already
/// arrives in, and the space [`hit_path`] accumulates each node's parent-relative rect into.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogicalPoint {
    pub x: f32,
    pub y: f32,
}

/// Every node containing `point`, root-first and deepest-last; empty if the point misses `root`.
///
/// A path, not a topmost node: the only tree anyone writes is a `button` whose child is a `text`,
/// and the deepest node under the pointer has no `on_click`, so returning it alone would mean no
/// button ever fires (docs/adr/0050 decision 1). Each caller scans the result from the deep end
/// for the kind it wants.
///
/// Three rules, all load-bearing:
///
/// - **Containment gates descent.** A node whose rect does not hold the point is not entered and
///   neither are its children. This makes the hittable region of an overflowing child exactly its
///   intersection with every ancestor -- the same region `paint::paint_node`'s `intersect_scissor`
///   chain draws it in, so hitting and painting agree on overflow without either walk carrying a
///   clip rect.
/// - **Children in reverse.** `paint_node` paints in declaration order, so the last child is on
///   top and is asked first; the first child that yields a hit wins.
/// - **Half-open bounds**, `rect.x <= point.x < rect.x + rect.width`. Two buttons sharing an edge
///   must not both claim it, and a zero-area rect must contain nothing.
///
/// `ResolvedNode::rect` is parent-relative, so the absolute rect of a hit node is only recoverable
/// from the path that reached it -- [`absolute_rect`] does that recovery, which is why the return
/// type is the whole chain rather than a node and its depth.
///
/// No depth bound of its own: `layout::scene::MAX_TREE_DEPTH` refuses a tree deeper than 64 levels
/// at resolve time.
pub fn hit_path(root: &ResolvedNode, point: LogicalPoint) -> Vec<&ResolvedNode> {
    let mut path = Vec::new();
    descend(root, point, 0.0, 0.0, &mut path);
    path
}

/// The absolute (surface-local) rect of `path`'s last node, `None` for an empty path.
///
/// Sums the parent-relative origins the walk descended through, which is the only place that sum
/// still exists once [`hit_path`] has returned bare node references. Callers wanting an
/// intermediate node's rect pass the prefix ending at it (`absolute_rect(&path[..=index])`).
pub fn absolute_rect(path: &[&ResolvedNode]) -> Option<LogicalRect> {
    let last = path.last()?;
    Some(LogicalRect {
        x: path.iter().map(|node| node.rect.x).sum(),
        y: path.iter().map(|node| node.rect.y).sum(),
        width: last.rect.width,
        height: last.rect.height,
    })
}

/// Pushes `node` and its deepest hit descendant onto `path`, returning whether it was entered at
/// all. `origin_x`/`origin_y` is the absolute origin of `node`'s parent, the same running sum
/// `layout::paint::paint_node` carries.
fn descend<'a>(
    node: &'a ResolvedNode,
    point: LogicalPoint,
    origin_x: f32,
    origin_y: f32,
    path: &mut Vec<&'a ResolvedNode>,
) -> bool {
    if !node.visible {
        return false;
    }
    let x = origin_x + node.rect.x;
    let y = origin_y + node.rect.y;
    if !contains(LogicalRect { x, y, ..node.rect }, point) {
        return false;
    }
    path.push(node);
    for child in node.children.iter().rev() {
        if descend(child, point, x, y, path) {
            break;
        }
    }
    true
}

/// Half-open containment: the top-left edges belong to the rect, the bottom-right ones to
/// whatever is past them. A zero-width or zero-height rect contains nothing, since the two
/// comparisons cannot both hold.
fn contains(rect: LogicalRect, point: LogicalPoint) -> bool {
    point.x >= rect.x && point.x < rect.x + rect.width && point.y >= rect.y && point.y < rect.y + rect.height
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn node(kind: &str, (x, y, width, height): (f32, f32, f32, f32), children: Vec<ResolvedNode>) -> ResolvedNode {
        ResolvedNode {
            kind: kind.to_string(),
            rect: LogicalRect { x, y, width, height },
            visible: true,
            properties: HashMap::new(),
            paint: None,
            children,
        }
    }

    fn kinds(path: &[&ResolvedNode]) -> Vec<String> {
        path.iter().map(|node| node.kind.clone()).collect()
    }

    #[test]
    fn a_point_outside_the_root_hits_nothing_at_all() {
        let root = node("panel", (0.0, 0.0, 100.0, 32.0), vec![]);
        assert!(hit_path(&root, LogicalPoint { x: 50.0, y: 40.0 }).is_empty());
        assert!(hit_path(&root, LogicalPoint { x: -1.0, y: 10.0 }).is_empty());
    }

    #[test]
    fn a_point_inside_the_root_but_in_no_child_yields_the_root_alone() {
        let root = node("panel", (0.0, 0.0, 100.0, 32.0), vec![node("button", (10.0, 4.0, 20.0, 24.0), vec![])]);
        let path = hit_path(&root, LogicalPoint { x: 80.0, y: 16.0 });
        assert_eq!(kinds(&path), ["panel"]);
    }

    #[test]
    fn the_deepest_containing_node_is_last_and_the_root_is_first() {
        let root = node(
            "panel",
            (0.0, 0.0, 100.0, 32.0),
            vec![node("row", (0.0, 0.0, 100.0, 32.0), vec![node("button", (10.0, 4.0, 20.0, 24.0), vec![])])],
        );
        let path = hit_path(&root, LogicalPoint { x: 15.0, y: 10.0 });
        assert_eq!(kinds(&path), ["panel", "row", "button"]);
    }

    #[test]
    fn a_childs_rect_is_read_relative_to_its_parent_not_to_the_surface() {
        let root = node(
            "panel",
            (0.0, 0.0, 100.0, 32.0),
            vec![node("row", (40.0, 0.0, 60.0, 32.0), vec![node("button", (10.0, 4.0, 20.0, 24.0), vec![])])],
        );
        assert_eq!(kinds(&hit_path(&root, LogicalPoint { x: 55.0, y: 10.0 })), ["panel", "row", "button"]);
        assert_eq!(kinds(&hit_path(&root, LogicalPoint { x: 15.0, y: 10.0 })), ["panel"]);
    }

    #[test]
    fn absolute_rect_sums_the_origins_the_walk_descended_through() {
        let root = node(
            "panel",
            (0.0, 0.0, 100.0, 32.0),
            vec![node("row", (40.0, 2.0, 60.0, 30.0), vec![node("button", (10.0, 4.0, 20.0, 24.0), vec![])])],
        );
        let path = hit_path(&root, LogicalPoint { x: 55.0, y: 10.0 });
        assert_eq!(absolute_rect(&path), Some(LogicalRect { x: 50.0, y: 6.0, width: 20.0, height: 24.0 }));
        assert_eq!(absolute_rect(&path[..=1]), Some(LogicalRect { x: 40.0, y: 2.0, width: 60.0, height: 30.0 }));
        assert_eq!(absolute_rect(&[]), None);
    }

    #[test]
    fn an_invisible_node_takes_its_whole_visible_subtree_out_of_the_path() {
        let mut hidden = node("row", (0.0, 0.0, 100.0, 32.0), vec![node("button", (10.0, 4.0, 20.0, 24.0), vec![])]);
        hidden.visible = false;
        let root = node("panel", (0.0, 0.0, 100.0, 32.0), vec![hidden]);
        assert_eq!(kinds(&hit_path(&root, LogicalPoint { x: 15.0, y: 10.0 })), ["panel"]);
    }

    #[test]
    fn two_overlapping_siblings_resolve_to_the_later_declared_one() {
        let root = node(
            "panel",
            (0.0, 0.0, 100.0, 32.0),
            vec![node("under", (0.0, 0.0, 50.0, 32.0), vec![]), node("over", (0.0, 0.0, 50.0, 32.0), vec![])],
        );
        assert_eq!(kinds(&hit_path(&root, LogicalPoint { x: 25.0, y: 16.0 })), ["panel", "over"]);
    }

    #[test]
    fn a_shared_edge_belongs_to_exactly_one_of_two_adjacent_rects() {
        let root = node(
            "panel",
            (0.0, 0.0, 100.0, 32.0),
            vec![node("left", (0.0, 0.0, 50.0, 32.0), vec![]), node("right", (50.0, 0.0, 50.0, 32.0), vec![])],
        );
        assert_eq!(kinds(&hit_path(&root, LogicalPoint { x: 49.9, y: 16.0 })), ["panel", "left"]);
        assert_eq!(kinds(&hit_path(&root, LogicalPoint { x: 50.0, y: 16.0 })), ["panel", "right"]);
    }

    #[test]
    fn a_zero_area_rect_contains_nothing_not_even_its_own_origin() {
        let root = node(
            "panel",
            (0.0, 0.0, 100.0, 32.0),
            vec![node("empty", (10.0, 10.0, 0.0, 12.0), vec![]), node("flat", (30.0, 10.0, 12.0, 0.0), vec![])],
        );
        assert_eq!(kinds(&hit_path(&root, LogicalPoint { x: 10.0, y: 10.0 })), ["panel"]);
        assert_eq!(kinds(&hit_path(&root, LogicalPoint { x: 30.0, y: 10.0 })), ["panel"]);
    }

    #[test]
    fn a_text_inside_a_button_still_leaves_the_button_findable_from_the_deep_end() {
        let root = node(
            "panel",
            (0.0, 0.0, 100.0, 32.0),
            vec![node("button", (10.0, 4.0, 40.0, 24.0), vec![node("text", (6.0, 5.0, 28.0, 14.0), vec![])])],
        );
        let path = hit_path(&root, LogicalPoint { x: 20.0, y: 12.0 });
        assert_eq!(kinds(&path), ["panel", "button", "text"]);
        let index = path.iter().rposition(|node| node.kind == "button").expect("the button must be findable");
        assert_eq!(absolute_rect(&path[..=index]), Some(LogicalRect { x: 10.0, y: 4.0, width: 40.0, height: 24.0 }));
    }
}
