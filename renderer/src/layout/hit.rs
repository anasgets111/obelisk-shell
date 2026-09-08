//! Pointer hit-testing: the chain of nodes under one point (ADR-0050 decision 1).
//!
//! Pure: unlike the rest of the pointer path, this only decides what a click means. Other
//! pointer-path work owns live `wl_pointer` and `wl_surface` objects.

use cursor_icon::CursorIcon;
use mlua::Value;

use crate::layout::node::{PaintStyle, StyleRun, TextAlign, apply_affine, invert_affine, segments};
use crate::layout::scene::ResolvedNode;
use crate::text::shaping::{self, FontRun, ShapeRequest, ShapingHandle};
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
/// A path, not a topmost node: the supported tree is a `button` whose `text` child has no
/// `on_click`; returning only the deepest node would mean no button ever fires (ADR-0050
/// decision 1).
/// Callers scan from the deep end for the kind they want.
///
/// Three rules, all load-bearing:
///
/// - **Containment gates descent.** A node whose rect does not hold the point is not entered and
///   neither are its children. This makes the hittable region of an overflowing child exactly its
///   intersection with every ancestor -- the same region `paint::run`'s `intersect_scissor`
///   chain draws it in, so hitting and painting agree on overflow without either walk carrying a
///   clip rect.
/// - **Children in reverse.** `run` paints in declaration order, so the last child is on
///   top and is asked first; the first child that yields a hit wins.
/// - **Half-open bounds**, `rect.x <= point.x < rect.x + rect.width`. Two buttons sharing an edge
///   must not both claim it, and a zero-area rect must contain nothing.
///
/// `ResolvedNode::rect` is parent-relative. [`absolute_rect`] recovers a hit node's absolute rect
/// from the path, which is why the return value is the whole chain.
///
/// `layout::scene::MAX_TREE_DEPTH` refuses a tree deeper than 64 levels at resolve time.
pub fn hit_path(root: &ResolvedNode, point: LogicalPoint) -> Vec<&ResolvedNode> {
    let mut path = Vec::new();
    descend(root, point, 0.0, 0.0, &mut path);
    path
}

/// The `href` of the styled run under `point` in a `text` node, or `None` when the point is on
/// plain text, past the end of a line, or below the last one (ADR-0106). `point` is in the node's
/// own coordinates.
///
/// The geometry is paint's, re-derived: `\n` splits the fitted content into lines a `line_height`
/// apart, each line is cut into pieces at its run boundaries, and the pieces are laid left to right
/// from the alignment's anchor. The widths come from the shaping worker rather than femtovg, which
/// is what paint measures with; the two agree to within 2% (`layout::paint`'s divergence tests),
/// which is well inside the slack a press on a word has.
pub fn link_under(node: &ResolvedNode, point: LogicalPoint, shaping: &ShapingHandle) -> Option<String> {
    let Some(PaintStyle::Text { content, runs, font_size, font, align, .. }) = node.paint.as_ref() else {
        return None;
    };
    if runs.iter().all(|run| run.href.is_none()) || point.y < 0.0 {
        return None;
    }
    let line_height = shaping::line_height(*font_size);
    let line_index = (point.y / line_height) as usize;
    let mut line_start = 0usize;
    let line = content.split('\n').enumerate().find_map(|(index, line)| {
        let start = line_start;
        line_start += line.len() + 1;
        (index == line_index).then_some(start..start + line.len())
    })?;
    let measured: Vec<(f32, Option<&StyleRun>)> = segments(line.clone(), runs)
        .into_iter()
        .map(|(range, run)| {
            let rebased = run.filter(|run| run.bold || run.italic).map(|run| FontRun {
                range: 0..range.len(),
                bold: run.bold,
                italic: run.italic,
            });
            let width = shaping
                .shape(ShapeRequest {
                    text: content[range].to_string(),
                    font_size: *font_size,
                    line_height,
                    max_width: None,
                    runs: rebased.into_iter().collect(),
                    font: font.clone(),
                })
                .width;
            (width, run)
        })
        .collect();
    let total: f32 = measured.iter().map(|(width, _)| width).sum();
    let mut x = match align {
        TextAlign::Start => 0.0,
        TextAlign::Center => (node.rect.width - total) / 2.0,
        TextAlign::End => node.rect.width - total,
    };
    for (width, run) in measured {
        if point.x >= x && point.x < x + width {
            return run.and_then(|run| run.href.clone());
        }
        x += width;
    }
    None
}

/// The shape the pointer should take over `path`'s deepest node (ADR-0107). Innermost wins, and
/// at each node an explicit `cursor` property beats what the node is: a `text` with `on_link` over
/// a link's own words is a `pointer`, a `textfield` is `text`, a `button` with an `on_click` is a
/// `pointer`, and nothing else says anything, so the arrow is what is left. The same three
/// questions `wayland::input` asks on a press, asked on motion, so what the cursor promises is what
/// a click does: a `button` with no handler is transparent to both.
///
/// A `cursor` set on an ancestor still loses to a link inside it, since the walk meets the link
/// first. That is the order a card with `cursor = "grab"` and a link in its body wants.
pub fn cursor_under(path: &[&ResolvedNode], point: LogicalPoint, shaping: &ShapingHandle) -> CursorIcon {
    path.iter()
        .enumerate()
        .rev()
        .find_map(|(depth, node)| {
            if let Some(Value::String(name)) = node.properties.get("cursor") {
                // Validated by `node::parse_cursor` when the pass resolved the node, so a name
                // that does not parse here is a bug, not a config error; the arrow is the fallback.
                return Some(name.to_str().ok().and_then(|name| name.parse().ok()).unwrap_or(CursorIcon::Default));
            }
            match node.kind.as_str() {
                "text" if matches!(node.properties.get("on_link"), Some(Value::Function(_))) => {
                    let rect = absolute_rect(&path[..=depth])?;
                    let local = LogicalPoint { x: point.x - rect.x, y: point.y - rect.y };
                    link_under(node, local, shaping).map(|_| CursorIcon::Pointer)
                }
                "textfield" => Some(CursorIcon::Text),
                "button" if matches!(node.properties.get("on_click"), Some(Value::Function(_))) => {
                    Some(CursorIcon::Pointer)
                }
                _ => None,
            }
        })
        .unwrap_or(CursorIcon::Default)
}

/// Whether a node with `id` is still somewhere under `root`, visible or not. The question a held
/// draft asks of the tree it was typed into (ADR-0108): identity, not geometry, so a field that
/// moved is still found and one that was removed is not.
///
/// A leaving node is not found, nor anything under it. The tree has already dropped it and only
/// its exit is still playing (ADR-0150), so a field inside one has nowhere to show a draft and its
/// callbacks belong to a subtree that is gone -- which is the removal this answers `false` for.
pub fn contains_node(root: &ResolvedNode, id: crate::layout::scene::NodeId) -> bool {
    !root.leaving && (root.id == id || root.children.iter().any(|child| contains_node(child, id)))
}

/// The absolute (surface-local) rect of `path`'s last node, `None` for an empty path. It sums the
/// parent-relative origins; pass a prefix to recover an intermediate node's rect.
pub fn absolute_rect(path: &[&ResolvedNode]) -> Option<LogicalRect> {
    let last = path.last()?;
    Some(LogicalRect {
        x: path.iter().map(|node| node.rect.x).sum(),
        y: path.iter().map(|node| node.rect.y).sum(),
        width: last.rect.width,
        height: last.rect.height,
    })
}

fn descend<'a>(
    node: &'a ResolvedNode,
    point: LogicalPoint,
    origin_x: f32,
    origin_y: f32,
    path: &mut Vec<&'a ResolvedNode>,
) -> bool {
    // A leaving node is painted and nothing more (ADR-0150).
    if !node.visible || node.leaving {
        return false;
    }
    let x = origin_x + node.rect.x;
    let y = origin_y + node.rect.y;
    // A transformed node is hit where it is painted: map the pointer back into its untransformed
    // space (ADR-0149), and hand that point down, since children paint under the same matrix.
    // A degenerate matrix (zero scale) paints nothing and takes nothing.
    let point = if node.transform.is_identity() {
        point
    } else {
        let Some(inverse) = invert_affine(node.transform.matrix(LogicalRect { x, y, ..node.rect })) else {
            return false;
        };
        let (px, py) = apply_affine(inverse, point.x, point.y);
        LogicalPoint { x: px, y: py }
    };
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

fn contains(rect: LogicalRect, point: LogicalPoint) -> bool {
    point.x >= rect.x && point.x < rect.x + rect.width && point.y >= rect.y && point.y < rect.y + rect.height
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn node(kind: &str, (x, y, width, height): (f32, f32, f32, f32), children: Vec<ResolvedNode>) -> ResolvedNode {
        ResolvedNode {
            displayed_source: None,
            tweens: Vec::new(),
            leaving: false,
            transform: crate::layout::node::Transform::default(),
            margin: crate::layout::node::EdgeInsets::default(),
            id: crate::layout::scene::NodeId::test(0),
            kind: kind.to_string(),
            rect: LogicalRect { x, y, width, height },
            visible: true,
            opacity: 1.0,
            properties: HashMap::new(),
            paint: None,
            children,
        }
    }

    // ---- cursor_under (ADR-0107) ----

    fn with(mut node: ResolvedNode, lua: &mlua::Lua, key: &str, value: Value) -> ResolvedNode {
        let _ = lua;
        node.properties.insert(key.to_string(), value);
        node
    }

    fn function(lua: &mlua::Lua) -> Value {
        Value::Function(lua.create_function(|_, ()| Ok(())).unwrap())
    }

    /// ADR-0149: a scaled node is hit where it is painted. A 20px button scaled 2x about its
    /// centre covers 10px beyond its laid-out box on every side, and its children are found
    /// through the same inverse.
    #[test]
    fn a_scaled_node_takes_the_pointer_where_it_is_painted() {
        let mut scaled = node("button", (100.0, 100.0, 20.0, 20.0), vec![node("rect", (0.0, 0.0, 10.0, 20.0), vec![])]);
        scaled.transform.scale = (2.0, 2.0);
        let tree = node("panel", (0.0, 0.0, 400.0, 400.0), vec![scaled]);
        // Inside the painted box (90..130), outside the laid-out one (100..120).
        let path = hit_path(&tree, LogicalPoint { x: 92.0, y: 95.0 });
        assert_eq!(path.len(), 3, "the button and its left child, which paints over 90..110");
        assert_eq!(path[2].kind, "rect");
        assert_eq!(hit_path(&tree, LogicalPoint { x: 125.0, y: 105.0 }).len(), 2, "the right half has no child");
        assert_eq!(hit_path(&tree, LogicalPoint { x: 135.0, y: 105.0 }).len(), 1, "past the painted box");
        let mut flat = node("button", (100.0, 100.0, 20.0, 20.0), vec![]);
        flat.transform.scale = (0.0, 1.0);
        let tree = node("panel", (0.0, 0.0, 400.0, 400.0), vec![flat]);
        assert_eq!(hit_path(&tree, LogicalPoint { x: 110.0, y: 110.0 }).len(), 1, "a zero scale takes nothing");
    }

    #[test]
    fn a_button_with_a_handler_is_a_pointer_and_one_without_is_the_arrow() {
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        let handled = node(
            "row",
            (0.0, 0.0, 100.0, 20.0),
            vec![
                with(
                    node("button", (0.0, 0.0, 50.0, 20.0), vec![node("text", (0.0, 0.0, 30.0, 20.0), vec![])]),
                    &lua,
                    "on_click",
                    function(&lua),
                ),
                node("button", (50.0, 0.0, 50.0, 20.0), vec![]),
            ],
        );
        let point = LogicalPoint { x: 10.0, y: 10.0 };
        assert_eq!(cursor_under(&hit_path(&handled, point), point, &shaping), CursorIcon::Pointer);
        let point = LogicalPoint { x: 60.0, y: 10.0 };
        assert_eq!(cursor_under(&hit_path(&handled, point), point, &shaping), CursorIcon::Default);
        assert_eq!(cursor_under(&[], point, &shaping), CursorIcon::Default, "off every node");
    }

    #[test]
    fn an_explicit_cursor_beats_what_the_node_is_and_the_innermost_one_wins() {
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        let disabled = with(
            with(node("button", (0.0, 0.0, 50.0, 20.0), vec![]), &lua, "on_click", function(&lua)),
            &lua,
            "cursor",
            Value::String(lua.create_string("not-allowed").unwrap()),
        );
        let field = node("textfield", (0.0, 0.0, 50.0, 20.0), vec![]);
        let handle = with(
            node("row", (0.0, 0.0, 100.0, 20.0), vec![disabled, node("row", (50.0, 0.0, 50.0, 20.0), vec![field])]),
            &lua,
            "cursor",
            Value::String(lua.create_string("grab").unwrap()),
        );
        let point = LogicalPoint { x: 10.0, y: 10.0 };
        assert_eq!(cursor_under(&hit_path(&handle, point), point, &shaping), CursorIcon::NotAllowed);
        let point = LogicalPoint { x: 60.0, y: 10.0 };
        assert_eq!(
            cursor_under(&hit_path(&handle, point), point, &shaping),
            CursorIcon::Text,
            "a field inside a grab handle is still a field"
        );
    }

    #[test]
    fn a_link_word_is_a_pointer_and_the_plain_word_beside_it_falls_through_to_the_button() {
        let shaping = ShapingHandle::spawn();
        let lua = mlua::Lua::new();
        let text = with(
            styled_text("see this page now", vec![link(4..13, "https://a/")], TextAlign::Start, 300.0),
            &lua,
            "on_link",
            function(&lua),
        );
        let card = node("button", (0.0, 0.0, 300.0, 60.0), vec![text]);
        let plain = LogicalPoint { x: 2.0, y: 5.0 };
        assert_eq!(
            cursor_under(&hit_path(&card, plain), plain, &shaping),
            CursorIcon::Default,
            "plain word, button has no handler"
        );
        let on_link = LogicalPoint { x: width_of(&shaping, "see ") + width_of(&shaping, "this page") / 2.0, y: 5.0 };
        assert_eq!(cursor_under(&hit_path(&card, on_link), on_link, &shaping), CursorIcon::Pointer);
    }

    // ---- link_under (ADR-0106) ----

    fn styled_text(content: &str, runs: Vec<StyleRun>, align: TextAlign, width: f32) -> ResolvedNode {
        let mut node = node("text", (0.0, 0.0, width, 60.0), Vec::new());
        node.paint = Some(PaintStyle::Text {
            content: content.to_string(),
            runs,
            font_size: 14.0,
            font: None,
            color: crate::layout::node::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
            align,
            elide: crate::layout::node::Elide::None,
            wrap: crate::layout::node::Wrap::None,
            max_lines: None,
        });
        node
    }

    fn link(range: std::ops::Range<usize>, href: &str) -> StyleRun {
        StyleRun { range, bold: false, italic: false, underline: true, color: None, href: Some(href.to_string()) }
    }

    fn width_of(shaping: &ShapingHandle, text: &str) -> f32 {
        shaping
            .shape(ShapeRequest {
                text: text.to_string(),
                font_size: 14.0,
                line_height: shaping::line_height(14.0),
                max_width: None,
                runs: Vec::new(),
                font: None,
            })
            .width
    }

    #[test]
    fn the_run_under_the_point_is_found_by_its_measured_width() {
        let shaping = ShapingHandle::spawn();
        let text = "see this page now";
        let node = styled_text(text, vec![link(4..13, "https://a/")], TextAlign::Start, 300.0);
        let before = width_of(&shaping, "see ");
        let link_width = width_of(&shaping, "this page");
        assert_eq!(link_under(&node, LogicalPoint { x: 2.0, y: 5.0 }, &shaping), None, "on 'see'");
        assert_eq!(
            link_under(&node, LogicalPoint { x: before + link_width / 2.0, y: 5.0 }, &shaping),
            Some("https://a/".to_string())
        );
        assert_eq!(
            link_under(&node, LogicalPoint { x: before + link_width + 4.0, y: 5.0 }, &shaping),
            None,
            "on 'now'"
        );
        assert_eq!(link_under(&node, LogicalPoint { x: 299.0, y: 5.0 }, &shaping), None, "past the end of the line");
    }

    #[test]
    fn a_link_on_the_second_line_is_found_at_its_line_and_not_the_first() {
        let shaping = ShapingHandle::spawn();
        let text = "first line\nsee this";
        let node = styled_text(text, vec![link(15..19, "https://b/")], TextAlign::Start, 300.0);
        let x = width_of(&shaping, "see ") + width_of(&shaping, "this") / 2.0;
        let step = shaping::line_height(14.0);
        assert_eq!(link_under(&node, LogicalPoint { x, y: step / 2.0 }, &shaping), None, "first line, plain");
        assert_eq!(link_under(&node, LogicalPoint { x, y: step * 1.5 }, &shaping), Some("https://b/".to_string()));
        assert_eq!(link_under(&node, LogicalPoint { x, y: step * 2.5 }, &shaping), None, "below the last line");
    }

    /// Centred and right-aligned lines start where paint starts them, not at zero.
    #[test]
    fn alignment_moves_where_the_line_starts() {
        let shaping = ShapingHandle::spawn();
        let text = "go";
        let node = styled_text(text, vec![link(0..2, "https://c/")], TextAlign::End, 200.0);
        let w = width_of(&shaping, "go");
        assert_eq!(link_under(&node, LogicalPoint { x: 1.0, y: 5.0 }, &shaping), None, "left edge is empty under End");
        assert_eq!(
            link_under(&node, LogicalPoint { x: 200.0 - w / 2.0, y: 5.0 }, &shaping),
            Some("https://c/".to_string())
        );
    }

    #[test]
    fn a_text_with_no_href_anywhere_answers_nothing_without_measuring() {
        let shaping = ShapingHandle::spawn();
        let plain = styled_text("hello", Vec::new(), TextAlign::Start, 100.0);
        assert_eq!(link_under(&plain, LogicalPoint { x: 3.0, y: 3.0 }, &shaping), None);
        let bold_only = styled_text(
            "hello",
            vec![StyleRun { range: 0..5, bold: true, italic: false, underline: false, color: None, href: None }],
            TextAlign::Start,
            100.0,
        );
        assert_eq!(link_under(&bold_only, LogicalPoint { x: 3.0, y: 3.0 }, &shaping), None);
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
