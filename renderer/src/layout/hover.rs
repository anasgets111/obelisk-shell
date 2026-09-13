//! Which `hover` signals a pointer position turns on, and which it turns off (ADR-0062).
//!
//! Pure: this decides what a hover means without a live `wl_pointer` or `wl_surface`.

use mlua::{Function, Value};

use super::hit::{self, LogicalPoint};
use super::scene::ResolvedNode;
use crate::lua::signal::{self, Signal};
use crate::text::snap::LogicalRect;

/// One node's answer: its hover signal, whether the pointer is on it, and where it is.
///
/// `rect` is `Some` only when `hovered`, and it is the node's *absolute* rect in its surface's
/// logical coordinates -- the same space `on_click` hands a config (ADR-0050 decision 3), so a
/// tooltip `popup` binding `anchor_rect` to it lands over the node the way a dropdown lands over
/// the button that opened it. Leaving it `None` on the way out is deliberate: the rect signal keeps
/// the last place the pointer was, so `anchor_rect` stays a valid non-zero rect (§ 6 refuses a
/// zero one) while the popup is closing.
pub struct HoverWrite {
    pub signal: Signal,
    pub hovered: bool,
    pub rect: Option<LogicalRect>,
    /// The node's `on_hover`, to be called on the crossing this write reports and not on every
    /// motion event inside the node (ADR-0095). Rides on the same write as the signal because the
    /// signal is what remembers the previous answer: a callback has no memory of its own, and a
    /// `ResolvedNode` has no identity to hang one off.
    pub on_hover: Option<Function>,
}

/// Every `hover` signal declared anywhere in `tree`, paired with whether the pointer is on its
/// node.
///
/// `point` is `None` when the pointer is not in this surface at all -- a `wl_pointer` `Leave`, or a
/// surface the pointer never entered -- which turns every hover in the tree off.
///
/// **The whole tree, not the hit path.** Returning only the nodes under the pointer would say what
/// to turn on and nothing about what to turn off, and the node being left is exactly the one whose
/// tooltip has to close. Invisible nodes are walked for the same reason: a node that went invisible
/// while the pointer was inside it still owns a signal reading true.
///
/// **On the path, not the innermost node** (ADR-0062 decision 5). [`hit::hit_path`] already
/// applies the three rules that matter -- containment gates descent, the topmost child wins, a node
/// that is not visible is not entered -- so a `pill` wrapping a `button` wrapping a `text` reports
/// all three as hovered, which is what a config binding the pill's own signal needs.
///
/// Duplicates are possible and deliberate: two nodes may name one signal, and the caller writes the
/// pairs in order, so the last one wins. That is a config saying two boxes are one hover region,
/// and the answer it gets is "hovered if the deepest-declared of them is", which is the only answer
/// available without the writer knowing what the config meant.
pub fn hover_writes(tree: &ResolvedNode, point: Option<LogicalPoint>) -> Vec<HoverWrite> {
    let path = point.map(|point| hit::hit_path(tree, point)).unwrap_or_default();
    let mut writes = Vec::new();
    collect(tree, &path, &mut writes);
    writes
}

/// Pushes `node`'s hover signal, if any, then recurses. Every node is visited so a hover can turn
/// off after the pointer leaves or the node becomes invisible.
fn collect(node: &ResolvedNode, path: &[&ResolvedNode], writes: &mut Vec<HoverWrite>) {
    if let Some(signal) = hover_signal(node) {
        // Both references index the same tree and `ResolvedNode` has no comparable id. The path
        // is bounded by `scene::MAX_TREE_DEPTH`, so this scan is at most 64 comparisons.
        let depth = path.iter().position(|on_path| std::ptr::eq(*on_path, node));
        // The prefix turns this parent-relative rect into an absolute one (ADR-0050 decision 3).
        let rect = depth.and_then(|depth| hit::absolute_rect(&path[..=depth]));
        let on_hover = match node.properties.get("on_hover") {
            Some(Value::Function(callback)) => Some(callback.clone()),
            _ => None,
        };
        writes.push(HoverWrite { signal, hovered: depth.is_some(), rect, on_hover });
    }
    for child in &node.children {
        collect(child, path, writes);
    }
}

/// The unresolved `Signal` handle behind `hover` (ADR-0062 decision 3). Other values, including a
/// bare boolean or `state()` signal, are inert because `Signal::hover_handle` refuses them. The
/// Wayland writer deliberately does not turn these unsupported values into config errors.
fn hover_signal(node: &ResolvedNode) -> Option<Signal> {
    let Some(Value::UserData(ud)) = node.properties.get("hover") else {
        return None;
    };
    signal::from_userdata(ud)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::signal::DirtyFlag;
    use crate::text::snap::LogicalRect;
    use mlua::Lua;
    use std::collections::HashMap;

    fn hover_userdata(lua: &Lua) -> (Signal, Value) {
        let (over, _rect) = Signal::new_hover(DirtyFlag::new(), Value::Nil);
        let ud = lua.create_userdata(over.clone()).unwrap();
        (over, Value::UserData(ud))
    }

    fn node(rect: (f32, f32, f32, f32), hover: Option<Value>, children: Vec<ResolvedNode>) -> ResolvedNode {
        node_with(rect, hover, None, children)
    }

    fn node_with(
        rect: (f32, f32, f32, f32),
        hover: Option<Value>,
        on_hover: Option<Value>,
        children: Vec<ResolvedNode>,
    ) -> ResolvedNode {
        let mut properties = HashMap::new();
        if let Some(hover) = hover {
            properties.insert("hover".to_string(), hover);
        }
        if let Some(on_hover) = on_hover {
            properties.insert("on_hover".to_string(), on_hover);
        }
        ResolvedNode {
            displayed_source: None,
            dissolve: None,
            tweens: Vec::new(),
            leaving: false,
            blur: false,
            transform: crate::layout::node::Transform::default(),
            margin: crate::layout::node::EdgeInsets::default(),
            id: crate::layout::scene::NodeId::test(0),
            kind: "row".to_string(),
            rect: LogicalRect { x: rect.0, y: rect.1, width: rect.2, height: rect.3 },
            visible: true,
            opacity: 1.0,
            properties,
            paint: None,
            children,
        }
    }

    fn at(x: f32, y: f32) -> Option<LogicalPoint> {
        Some(LogicalPoint { x, y })
    }

    fn answers(writes: &[HoverWrite]) -> Vec<bool> {
        writes.iter().map(|write| write.hovered).collect()
    }

    #[test]
    fn a_tree_with_no_hover_property_asks_for_no_writes() {
        let tree = node((0.0, 0.0, 100.0, 20.0), None, vec![node((0.0, 0.0, 50.0, 20.0), None, vec![])]);
        assert!(hover_writes(&tree, at(10.0, 10.0)).is_empty());
    }

    #[test]
    fn the_node_under_the_pointer_is_hovered_and_its_sibling_is_not() {
        let lua = Lua::new();
        let (_left_signal, left) = hover_userdata(&lua);
        let (_right_signal, right) = hover_userdata(&lua);
        let tree = node(
            (0.0, 0.0, 100.0, 20.0),
            None,
            vec![node((0.0, 0.0, 50.0, 20.0), Some(left), vec![]), node((50.0, 0.0, 50.0, 20.0), Some(right), vec![])],
        );

        assert_eq!(answers(&hover_writes(&tree, at(10.0, 10.0))), vec![true, false]);
        assert_eq!(answers(&hover_writes(&tree, at(60.0, 10.0))), vec![false, true]);
    }

    #[test]
    fn an_ancestor_of_the_node_under_the_pointer_is_hovered_too() {
        // ADR-0062 decision 5: a pill is a `row` wrapping a `button` wrapping a `text`, and the
        // signal a config binds hangs off the outermost of the three. Innermost-only hover would report false for all of them.
        let lua = Lua::new();
        let (_outer_signal, outer) = hover_userdata(&lua);
        let (_inner_signal, inner) = hover_userdata(&lua);
        let tree = node((0.0, 0.0, 100.0, 20.0), Some(outer), vec![node((10.0, 5.0, 30.0, 10.0), Some(inner), vec![])]);

        assert_eq!(answers(&hover_writes(&tree, at(20.0, 10.0))), vec![true, true], "both the pill and its button");
        assert_eq!(answers(&hover_writes(&tree, at(80.0, 10.0))), vec![true, false], "the pill alone");
    }

    #[test]
    fn a_pointer_outside_the_surface_turns_every_hover_off() {
        // The `wl_pointer` `Leave` case. Without it a tooltip stays open after the pointer has left
        // the bar entirely, because no `Motion` ever arrives to say otherwise.
        let lua = Lua::new();
        let (_signal, hover) = hover_userdata(&lua);
        let tree = node((0.0, 0.0, 100.0, 20.0), Some(hover), vec![]);

        assert_eq!(answers(&hover_writes(&tree, at(10.0, 10.0))), vec![true]);
        assert_eq!(answers(&hover_writes(&tree, None)), vec![false], "a Leave turns it off");
    }

    #[test]
    fn an_invisible_node_is_still_asked_about_so_its_hover_can_be_turned_off() {
        // The node is walked but never on the hit path, since `hit_path` refuses to enter an
        // invisible node. A tooltip that made its own trigger invisible would otherwise latch on.
        let lua = Lua::new();
        let (_signal, hover) = hover_userdata(&lua);
        let mut tree = node((0.0, 0.0, 100.0, 20.0), Some(hover), vec![]);
        tree.visible = false;

        assert_eq!(answers(&hover_writes(&tree, at(10.0, 10.0))), vec![false]);
    }

    /// `on_hover` rides on the same write as the signal, because the signal is the memory: the
    /// caller fires the callback on the crossing `set_changed` reports and never on a motion event
    /// inside the node (ADR-0095).
    #[test]
    fn a_nodes_on_hover_is_carried_on_its_hover_write() {
        let lua = Lua::new();
        let (_signal, hover) = hover_userdata(&lua);
        let callback = lua.create_function(|_, ()| Ok(())).unwrap();
        let tree = node_with((0.0, 0.0, 100.0, 20.0), Some(hover), Some(Value::Function(callback)), vec![]);

        let writes = hover_writes(&tree, at(10.0, 10.0));
        assert!(writes[0].on_hover.is_some(), "the callback reaches the caller that fires it");
        assert!(writes[0].hovered);

        // On the way out too: a leave is a crossing, and releasing whatever the enter took is the
        // whole reason a config wants the edge rather than the signal.
        let leaving = hover_writes(&tree, None);
        assert!(leaving[0].on_hover.is_some());
        assert!(!leaving[0].hovered);
    }

    #[test]
    fn a_node_with_a_hover_slot_and_no_callback_carries_none() {
        let lua = Lua::new();
        let (_signal, hover) = hover_userdata(&lua);
        let tree = node((0.0, 0.0, 100.0, 20.0), Some(hover), vec![]);
        assert!(hover_writes(&tree, at(10.0, 10.0))[0].on_hover.is_none());
    }

    #[test]
    fn a_hover_property_that_is_not_a_signal_is_inert_rather_than_an_error() {
        let tree = node((0.0, 0.0, 100.0, 20.0), Some(Value::Boolean(true)), vec![]);
        assert!(hover_writes(&tree, at(10.0, 10.0)).is_empty());
    }

    #[test]
    fn a_hovered_node_reports_where_it_is_and_a_left_one_reports_nothing() {
        // The rect is what a tooltip's `anchor_rect` binds to, and it has to be absolute: the node
        // below sits at (10, 5) inside a parent that is itself at (4, 2), so a parent-relative rect
        // would put the tooltip over the wrong part of the bar.
        let lua = Lua::new();
        let (_signal, hover) = hover_userdata(&lua);
        let tree = node((4.0, 2.0, 100.0, 20.0), None, vec![node((10.0, 5.0, 30.0, 10.0), Some(hover), vec![])]);

        let writes = hover_writes(&tree, at(20.0, 10.0));
        assert!(writes[0].hovered);
        let rect = writes[0].rect.expect("a hovered node reports its rect");
        assert_eq!((rect.x, rect.y, rect.width, rect.height), (14.0, 7.0, 30.0, 10.0));

        // None on the way out, so the rect signal keeps the last place the pointer was and
        // `anchor_rect` stays a valid non-zero rect while the popup closes (§ 6).
        let leaving = hover_writes(&tree, None);
        assert!(!leaving[0].hovered);
        assert!(leaving[0].rect.is_none());
    }

    #[test]
    fn the_topmost_of_two_overlapping_siblings_is_the_hovered_one() {
        // `hit_path` asks children in reverse declaration order and stops at the first hit, so the
        // one painted last wins. Hover has to agree with paint, or the highlight lands on the box
        // the user cannot see.
        let lua = Lua::new();
        let (_under_signal, under) = hover_userdata(&lua);
        let (_over_signal, over) = hover_userdata(&lua);
        let tree = node(
            (0.0, 0.0, 100.0, 20.0),
            None,
            vec![node((0.0, 0.0, 100.0, 20.0), Some(under), vec![]), node((0.0, 0.0, 100.0, 20.0), Some(over), vec![])],
        );

        assert_eq!(answers(&hover_writes(&tree, at(10.0, 10.0))), vec![false, true]);
    }
}
