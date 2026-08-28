//! Node constructors (`oblisk-idl-api-specs.md` § 5.2/§ 6.1) and `VirtualNode`, the loader's
//! shallow, unvalidated table-to-Rust conversion (`docs/oblisk-tdd-test-harness.md` § 4.1 names
//! both this type and `renderer/tests/test_lua_marshalling.rs`).
//!
//! ponytail: `deserialize_lua_table` is shallow on purpose -- it reads `kind` and copies every
//! other key as-is into `properties`, never recursing into a nested `children`/`child` table. A
//! node's own properties (including its raw, unconverted `child`/`children` value) are exactly
//! what build-steps.md Phase 10 calls "the loader's output... not the final in-memory node
//! itself"; walking into it is Phase 12's retained-scene reconciliation, not this module's job.
//! Field-level schema validation against § 5.2's table (e.g. rejecting a `width` that's neither
//! an integer nor `"Fill"`) is the same deferral: Phase 12's layout engine is the actual consumer
//! that needs typed, validated properties, so validating them here would be built ahead of its
//! only real caller.

use std::collections::HashMap;

use mlua::{Lua, Table, Value};

/// § 5.2's eight geometric nodes plus the three top-level surface roles a config declares:
/// § 6.1's `panel`, § 6.2's `window`, § 6.3's `popup` (ADR-0040: these are surface *roles*;
/// "surface" is the umbrella term covering all four).
///
/// § 6.4's `lock` is the fourth role and deliberately absent. A lock surface's lifetime is the
/// lock's, not the config's (docs/adr/0042), so it is not something `shell.lua` returns and giving
/// it a constructor here would say it is -- see `crate::lua::require_surface`, which rejects one at
/// the root with that reason.
const NODE_KINDS: [&str; 11] =
    ["rect", "row", "column", "text", "icon", "button", "list", "textfield", "panel", "window", "popup"];

/// A Lua node table, tagged with its constructor's `kind` and carrying every other prop
/// untouched. Not the final in-memory scene node -- see the module doc comment.
#[derive(Debug, Clone)]
pub struct VirtualNode {
    pub kind: String,
    pub properties: HashMap<String, Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum DeserializeError {
    #[error(transparent)]
    Lua(#[from] mlua::Error),
    #[error("node table has no `kind` field")]
    MissingKind,
    #[error("node table's `kind` field is not a string")]
    KindNotAString,
}

/// Registers every [`NODE_KINDS`] entry as Lua-callable sugar: each takes the props table Lua
/// passed and tags it with `kind`, matching `docs/oblisk-tdd-test-harness.md` § 4.1's own worked
/// example ("Echo table structure back to Rust"). One loop over the array rather than a list
/// spelled out again here, so adding a role is one edit.
pub fn register_node_constructors(lua: &Lua) -> mlua::Result<()> {
    for kind in NODE_KINDS {
        lua.globals().set(
            kind,
            lua.create_function(move |_, props: Table| {
                props.set("kind", kind)?;
                Ok(props)
            })?,
        )?;
    }
    Ok(())
}

/// Converts one Lua node table into a [`VirtualNode`]: pulls out `kind`, copies every other
/// key-value pair into `properties` as-is. Does not recurse into `children`/`child`.
pub fn deserialize_lua_table(table: &Table) -> Result<VirtualNode, DeserializeError> {
    let kind = match table.get::<Value>("kind")? {
        Value::String(s) => s.to_string_lossy(),
        Value::Nil => return Err(DeserializeError::MissingKind),
        _ => return Err(DeserializeError::KindNotAString),
    };

    let mut properties = HashMap::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair?;
        if let Value::String(ref key_str) = key
            && key_str.to_string_lossy() == "kind"
        {
            continue;
        }
        let key = match &key {
            Value::String(s) => s.to_string_lossy(),
            other => other.to_string()?,
        };
        properties.insert(key, value);
    }

    Ok(VirtualNode { kind, properties })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua_with_constructors() -> Lua {
        let lua = Lua::new();
        register_node_constructors(&lua).unwrap();
        lua
    }

    #[test]
    fn a_node_constructor_tags_the_props_table_with_its_kind() {
        let lua = lua_with_constructors();
        let table: Table = lua
            .load(r##"return rect { background = "#11111B", width = "Fill", height = 32 }"##)
            .eval()
            .unwrap();
        assert_eq!(table.get::<String>("kind").unwrap(), "rect");
        assert_eq!(table.get::<String>("background").unwrap(), "#11111B");
    }

    #[test]
    fn deserialize_lua_table_pulls_kind_out_and_keeps_every_other_field() {
        let lua = lua_with_constructors();
        let table: Table = lua.load(r#"return text { content = "hi", font_size = 14 }"#).eval().unwrap();

        let node = deserialize_lua_table(&table).unwrap();
        assert_eq!(node.kind, "text");
        assert!(!node.properties.contains_key("kind"), "kind must be pulled out, not duplicated into properties");
        assert_eq!(node.properties.get("content").unwrap().as_string().unwrap().to_string_lossy(), "hi");
        assert_eq!(node.properties.get("font_size").unwrap().as_integer().unwrap(), 14);
    }

    #[test]
    fn deserialize_lua_table_rejects_a_table_with_no_kind_field() {
        let lua = Lua::new();
        let table: Table = lua.create_table().unwrap();
        table.set("width", 32).unwrap();

        let err = deserialize_lua_table(&table).unwrap_err();
        assert!(matches!(err, DeserializeError::MissingKind));
    }

    #[test]
    fn deserialize_lua_table_leaves_a_nested_child_table_unconverted() {
        let lua = lua_with_constructors();
        let table: Table = lua
            .load(r##"return panel { id = "bar", layer = "Top", child = rect { background = "#000000" } }"##)
            .eval()
            .unwrap();

        let node = deserialize_lua_table(&table).unwrap();
        let child = node.properties.get("child").unwrap();
        assert!(matches!(child, Value::Table(_)), "child stays a raw Lua table, not a converted VirtualNode");
    }

    #[test]
    fn every_section_5_2_and_6_1_to_6_3_node_kind_constructs_and_tags_correctly() {
        let lua = lua_with_constructors();
        for kind in NODE_KINDS {
            let table: Table = lua.load(format!("return {kind} {{}}")).eval().unwrap();
            assert_eq!(table.get::<String>("kind").unwrap(), kind);
        }
    }

    #[test]
    fn window_and_popup_are_constructors_a_config_can_call() {
        // The two roles docs/adr/0040 decision 1 gives a config beside `panel`. Named explicitly
        // rather than left to the loop above, because the loop passes whatever the array happens
        // to hold and this is the pair build-steps.md Phase 22 exists to add.
        let lua = lua_with_constructors();
        assert!(NODE_KINDS.contains(&"window") && NODE_KINDS.contains(&"popup"));
        let table: Table = lua.load(r#"return popup { id = "menu", parent = "bar" }"#).eval().unwrap();
        assert_eq!(table.get::<String>("kind").unwrap(), "popup");
        assert_eq!(table.get::<String>("parent").unwrap(), "bar");
    }
}
