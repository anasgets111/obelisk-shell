//! Node constructors (`oblisk-idl-api-specs.md` § 5.2/§ 6.1) and `VirtualNode`, the loader's
//! shallow, unvalidated table-to-Rust conversion.
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

/// § 5.2's eight geometric nodes plus all four top-level surface roles a config declares: § 6.1's
/// `panel`, § 6.2's `window`, § 6.3's `popup` and § 6.4's `lock` (ADR-0040: these are surface
/// *roles*; "surface" is the umbrella term covering all four).
///
/// `lock` joined this array under docs/adr/0052 decision 2: a constructor decides where a
/// declaration is *written*, which docs/adr/0049 already separated from when the Wayland object
/// exists. `window` and `popup` own no `xdg_toplevel`/`xdg_popup` until `visible` says so; `lock`
/// is the same shape with the compositor's `locked` event as its trigger instead of a signal.
const NODE_KINDS: [&str; 13] = [
    "rect",
    "row",
    "column",
    "text",
    "icon",
    "image",
    "button",
    "list",
    "textfield",
    "panel",
    "window",
    "popup",
    "lock",
];

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
/// passed and tags it with `kind`. One loop over the array rather than a list spelled out again
/// here, so adding a role is one edit.
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
        let table: Table =
            lua.load(r##"return rect { background = "#11111B", width = "Fill", height = 32 }"##).eval().unwrap();
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
        // Named explicitly rather than left to the loop above, since the loop passes whatever
        // the array happens to hold.
        let lua = lua_with_constructors();
        assert!(NODE_KINDS.contains(&"window") && NODE_KINDS.contains(&"popup"));
        let table: Table = lua.load(r#"return popup { id = "menu", parent = "bar" }"#).eval().unwrap();
        assert_eq!(table.get::<String>("kind").unwrap(), "popup");
        assert_eq!(table.get::<String>("parent").unwrap(), "bar");
    }

    #[test]
    fn image_is_a_constructor_and_is_the_one_kind_section_5_2_does_not_list() {
        // docs/adr/0054 decision 3 adds this outside § 5.2's eight; pinned by name so an edit
        // that dropped it would fail loudly rather than take the wallpaper with it silently.
        let lua = lua_with_constructors();
        assert!(NODE_KINDS.contains(&"image"));
        let table: Table = lua.load(r#"return image { source = "/tmp/wall.png", fit = "cover" }"#).eval().unwrap();
        assert_eq!(table.get::<String>("kind").unwrap(), "image");
        assert_eq!(table.get::<String>("source").unwrap(), "/tmp/wall.png");
        assert_eq!(table.get::<String>("fit").unwrap(), "cover");
    }

    #[test]
    fn lock_is_a_constructor_a_config_can_call_because_declaring_one_is_not_locking() {
        // Pinned by name (docs/adr/0052 decision 2) rather than by the loop above, which would go
        // on passing if a later edit dropped the entry.
        let lua = lua_with_constructors();
        assert!(NODE_KINDS.contains(&"lock"));
        let table: Table = lua.load(r#"return lock { id = "screen-lock" }"#).eval().unwrap();
        assert_eq!(table.get::<String>("kind").unwrap(), "lock");
        assert_eq!(table.get::<String>("id").unwrap(), "screen-lock");
    }
}

/// `lua-meta/nodes.lua` and `lua-meta/surfaces.lua` are hand-written and always will be. There is
/// no type to generate them from: a node's schema is 29 scattered `properties.get("...")` calls
/// across `layout/node/`, each validating one key inline, so the schema is control flow rather
/// than data. `lua-meta/oblisk.lua` is the opposite case and is generated
/// (`supervisor/src/stubs.rs`), because the capability payloads are real `Serialize` structs.
///
/// So this guard covers the hand-written half. It checks the roster, not the fields, which is the
/// drift that actually happens: someone adds a node kind and forgets the stub. The capability
/// check stays here too, because a config reaches `oblisk.<name>` through the same file and this
/// crate is the one that owns `shared::CAPABILITIES`'s Lua-side spelling.
#[cfg(test)]
mod meta_stub_tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    fn meta(file: &str) -> String {
        let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../lua-meta").join(file);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("{} is missing or unreadable: {err}", path.display()))
    }

    /// Every name a config can call as a node constructor, declared exactly once.
    #[test]
    fn the_stubs_declare_every_node_kind_and_no_others() {
        let source = meta("nodes.lua") + &meta("surfaces.lua");
        let declared: BTreeSet<&str> =
            source.lines().filter_map(|line| line.strip_prefix("function ")?.split('(').next()).collect();
        let expected: BTreeSet<&str> = super::NODE_KINDS.iter().copied().collect();
        assert_eq!(declared, expected, "lua-meta is out of step with NODE_KINDS");
    }

    /// Every `shared::CAPABILITIES` name, as a field on the `Oblisk` class.
    #[test]
    fn the_stubs_declare_every_capability_and_no_others() {
        let source = meta("oblisk.lua");
        // The `---@field` block under `---@class Oblisk`, which is the namespace a config sees.
        // Split on the trailing newline too, or this matches `---@class ObliskVersion` first.
        let class = source.split("---@class Oblisk\n").nth(1).expect("oblisk.lua declares an Oblisk class");
        let renderer_sourced = ["screens", "rescue", "version", "config_dir"];
        let declared: BTreeSet<&str> = class
            .lines()
            .take_while(|line| line.starts_with("---@field"))
            .filter_map(|line| line.split_whitespace().nth(1))
            .filter(|name| !renderer_sourced.contains(name))
            .collect();
        let expected: BTreeSet<&str> = shared::CAPABILITIES.iter().copied().collect();
        assert_eq!(declared, expected, "lua-meta/oblisk.lua is out of step with shared::CAPABILITIES");
    }
}
