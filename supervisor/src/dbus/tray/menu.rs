//! DBusMenu `GetLayout` reply parsing: recursive `zvariant::Value` walking into a [`MenuItem`] tree
//! (ADR-0031: parsed by hand, recursively, from the raw zvariant Value).
//! Split from `dbus::tray` -- see `dbus/tray/mod.rs` for the module-level doc.

use serde::Serialize;
use zbus::zvariant::Value;

use super::proxies::{DBusMenuProxy, raw_menu_layout_to_value};

/// One node of a DBusMenu layout tree, already resolved into what `tray.items[].menu` needs
/// (docs/oblisk-idl-api-specs.md §2.14).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MenuItem {
    pub id: i32,
    pub menu_type: String,
    pub label: Option<String>,
    pub enabled: bool,
    pub icon_name: Option<String>,
    pub toggle_type: Option<String>,
    pub toggle_state: Option<i32>,
    pub children: Vec<MenuItem>,
}

/// Unwraps a nested D-Bus variant (`Value::Value(Box<Value>)`) down to the real payload --
/// DBusMenu's `av` (array-of-variant) children come back this way, one variant layer per
/// element.
fn unwrap_variant<'a>(value: &'a Value<'_>) -> &'a Value<'a> {
    match value {
        Value::Value(inner) => unwrap_variant(inner),
        other => other,
    }
}

fn value_as_str<'a>(value: &'a Value<'_>) -> Option<&'a str> {
    match unwrap_variant(value) {
        Value::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

fn value_as_bool(value: &Value<'_>) -> Option<bool> {
    match unwrap_variant(value) {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

fn value_as_i32(value: &Value<'_>) -> Option<i32> {
    match unwrap_variant(value) {
        Value::I32(i) => Some(*i),
        _ => None,
    }
}

fn dict_str_key<'a>(key: &'a Value<'_>) -> Option<&'a str> {
    match key {
        Value::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

/// Hard cap on [`parse_menu_node`]'s own recursion depth: a `GetLayout` reply's tree
/// structure is controlled by whichever session-bus peer registered the tray item's `Menu`
/// object, so a deeply nested but legally-encoded reply could otherwise stack-overflow this
/// task via unbounded Rust recursion. Generous for any real, human-authored menu -- DBusMenu
/// trees nested more than a handful of levels deep don't happen in practice.
const MAX_MENU_DEPTH: u32 = 32;

/// Parses one `(ia{sv}av)`-shaped DBusMenu layout node -- `id`, its properties dict, and its
/// `av` children array -- recursively into a [`MenuItem`] tree. `None` on any structural
/// mismatch; a missing/malformed property falls back to its DBusMenu spec default rather
/// than failing the whole node.
///
/// `depth` is this node's own recursion depth (`0` for the tree's root). At
/// [`MAX_MENU_DEPTH`], this node itself still parses normally, but its `children` are
/// truncated to empty instead of recursing further -- logged, since a legitimate app should
/// never hit this.
pub(super) fn parse_menu_node(value: &Value<'_>, depth: u32) -> Option<MenuItem> {
    let structure = match unwrap_variant(value) {
        Value::Structure(structure) => structure,
        _ => return None,
    };
    let [id_field, properties_field, children_field] = structure.fields() else {
        return None;
    };
    let id = value_as_i32(id_field)?;

    let mut menu_type = "standard".to_string();
    let mut label = None;
    let mut enabled = true;
    let mut icon_name = None;
    let mut toggle_type = None;
    let mut toggle_state_raw = None;
    if let Value::Dict(dict) = unwrap_variant(properties_field) {
        for (key, val) in dict.iter() {
            match dict_str_key(key) {
                Some("type") => {
                    if let Some(s) = value_as_str(val) {
                        menu_type = s.to_string();
                    }
                }
                Some("label") => label = value_as_str(val).map(str::to_string),
                Some("enabled") => {
                    if let Some(b) = value_as_bool(val) {
                        enabled = b;
                    }
                }
                Some("icon-name") => icon_name = value_as_str(val).filter(|s| !s.is_empty()).map(str::to_string),
                Some("toggle-type") => toggle_type = value_as_str(val).filter(|s| !s.is_empty()).map(str::to_string),
                Some("toggle-state") => toggle_state_raw = value_as_i32(val),
                _ => {}
            }
        }
    }
    let toggle_state = toggle_type.as_ref().map(|_| toggle_state_raw.unwrap_or(-1));

    let children = if depth >= MAX_MENU_DEPTH {
        eprintln!("tray: GetLayout reply exceeded the maximum menu depth ({MAX_MENU_DEPTH}) at node id {id}; truncating its children");
        Vec::new()
    } else {
        match unwrap_variant(children_field) {
            Value::Array(array) => array.iter().filter_map(|child| parse_menu_node(child, depth + 1)).collect(),
            _ => Vec::new(),
        }
    };

    Some(MenuItem { id, menu_type, label, enabled, icon_name, toggle_type, toggle_state, children })
}

pub(super) async fn fetch_menu_via(menu: &DBusMenuProxy<'static>) -> zbus::Result<Vec<MenuItem>> {
    let (_, raw_root) = menu.get_layout(0, -1, &[]).await?;
    let root_value = raw_menu_layout_to_value(raw_root);
    Ok(parse_menu_node(&root_value, 0).map(|root| root.children).unwrap_or_default())
}


#[cfg(test)]
mod tests {
    use zbus::zvariant::{Array, Dict, Signature, Str, StructureBuilder};

    use super::*;

    // ---- parse_menu_node ----

    fn menu_node_value<'a>(id: i32, properties: Vec<(&'a str, Value<'a>)>, children: Vec<Value<'a>>) -> Value<'a> {
        let mut dict = Dict::new(&Signature::Str, &Signature::Variant);
        for (key, value) in properties {
            dict.append(Value::Str(Str::from(key)), Value::Value(Box::new(value))).expect("dict insert must succeed in this test");
        }
        let mut array = Array::new(&Signature::Variant);
        for child in children {
            array.append(Value::Value(Box::new(child))).expect("array insert must succeed in this test");
        }
        let structure = StructureBuilder::new().add_field(id).append_field(Value::Dict(dict)).append_field(Value::Array(array)).build().expect("well-formed test structure");
        Value::Structure(structure)
    }

    #[test]
    fn parse_menu_node_parses_a_leaf_standard_item() {
        let value = menu_node_value(7, vec![("type", Value::Str(Str::from("standard"))), ("label", Value::Str(Str::from("Quit"))), ("enabled", Value::Bool(true))], vec![]);

        let item = parse_menu_node(&value, 0).expect("must parse a well-formed node");
        assert_eq!(item.id, 7);
        assert_eq!(item.menu_type, "standard");
        assert_eq!(item.label, Some("Quit".to_string()));
        assert!(item.enabled);
        assert_eq!(item.icon_name, None);
        assert_eq!(item.toggle_type, None);
        assert_eq!(item.toggle_state, None);
        assert!(item.children.is_empty());
    }

    #[test]
    fn parse_menu_node_defaults_type_to_standard_and_enabled_to_true_when_absent() {
        let value = menu_node_value(1, vec![], vec![]);
        let item = parse_menu_node(&value, 0).expect("must parse a well-formed node");
        assert_eq!(item.menu_type, "standard");
        assert!(item.enabled);
        assert_eq!(item.label, None);
    }

    #[test]
    fn parse_menu_node_parses_a_separator() {
        let value = menu_node_value(2, vec![("type", Value::Str(Str::from("separator")))], vec![]);
        let item = parse_menu_node(&value, 0).expect("must parse a well-formed node");
        assert_eq!(item.menu_type, "separator");
    }

    #[test]
    fn parse_menu_node_respects_enabled_false() {
        let value = menu_node_value(3, vec![("enabled", Value::Bool(false))], vec![]);
        let item = parse_menu_node(&value, 0).expect("must parse a well-formed node");
        assert!(!item.enabled);
    }

    #[test]
    fn parse_menu_node_parses_toggle_type_and_state() {
        let value = menu_node_value(4, vec![("toggle-type", Value::Str(Str::from("checkmark"))), ("toggle-state", Value::I32(1))], vec![]);
        let item = parse_menu_node(&value, 0).expect("must parse a well-formed node");
        assert_eq!(item.toggle_type, Some("checkmark".to_string()));
        assert_eq!(item.toggle_state, Some(1));
    }

    #[test]
    fn parse_menu_node_defaults_toggle_state_to_negative_one_when_toggle_type_present_but_state_absent() {
        let value = menu_node_value(5, vec![("toggle-type", Value::Str(Str::from("radio")))], vec![]);
        let item = parse_menu_node(&value, 0).expect("must parse a well-formed node");
        assert_eq!(item.toggle_type, Some("radio".to_string()));
        assert_eq!(item.toggle_state, Some(-1));
    }

    #[test]
    fn parse_menu_node_parses_icon_name() {
        let value = menu_node_value(6, vec![("icon-name", Value::Str(Str::from("edit-cut")))], vec![]);
        let item = parse_menu_node(&value, 0).expect("must parse a well-formed node");
        assert_eq!(item.icon_name, Some("edit-cut".to_string()));
    }

    #[test]
    fn parse_menu_node_recurses_into_children() {
        let child_a = menu_node_value(11, vec![("label", Value::Str(Str::from("Copy")))], vec![]);
        let child_b = menu_node_value(12, vec![("label", Value::Str(Str::from("Paste")))], vec![]);
        let root = menu_node_value(0, vec![], vec![child_a, child_b]);

        let item = parse_menu_node(&root, 0).expect("must parse a well-formed node");
        assert_eq!(item.children.len(), 2);
        assert_eq!(item.children[0].id, 11);
        assert_eq!(item.children[0].label, Some("Copy".to_string()));
        assert_eq!(item.children[1].id, 12);
        assert_eq!(item.children[1].label, Some("Paste".to_string()));
    }

    #[test]
    fn parse_menu_node_recurses_multiple_levels_deep() {
        let grandchild = menu_node_value(21, vec![("label", Value::Str(Str::from("Deep")))], vec![]);
        let child = menu_node_value(11, vec![("label", Value::Str(Str::from("Submenu")))], vec![grandchild]);
        let root = menu_node_value(0, vec![], vec![child]);

        let item = parse_menu_node(&root, 0).expect("must parse a well-formed node");
        assert_eq!(item.children[0].children[0].id, 21);
        assert_eq!(item.children[0].children[0].label, Some("Deep".to_string()));
    }

    #[test]
    fn parse_menu_node_rejects_a_non_structure_value() {
        assert_eq!(parse_menu_node(&Value::I32(42), 0), None);
    }

    #[test]
    fn parse_menu_node_truncates_at_the_depth_cap_without_panicking_or_overflowing() {
        // A chain well deeper than MAX_MENU_DEPTH: a malicious (or just buggy) GetLayout reply
        // could hand this parser exactly this shape, and unbounded recursion through it is a
        // stack-overflow DoS any session-bus peer registering a tray item's Menu object could
        // otherwise trigger.
        fn deep_chain(remaining: u32, id: i32) -> Value<'static> {
            if remaining == 0 {
                menu_node_value(id, vec![], vec![])
            } else {
                menu_node_value(id, vec![], vec![deep_chain(remaining - 1, id + 1)])
            }
        }

        let root = deep_chain(MAX_MENU_DEPTH + 20, 0);

        // Must complete (no panic, no stack overflow) and return a real, if truncated, tree.
        let item = parse_menu_node(&root, 0).expect("the root node itself must still parse");

        let mut current = &item;
        let mut depth = 0;
        while !current.children.is_empty() {
            current = &current.children[0];
            depth += 1;
        }
        assert_eq!(depth, MAX_MENU_DEPTH, "parsing must truncate children exactly at the depth cap, not keep recursing into the deeper levels the raw tree actually has");
    }

}
