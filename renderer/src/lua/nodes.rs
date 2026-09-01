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

/// The § 5.1 properties every kind takes, surface roles included: geometry, identity and the two
/// flags. `layout::scene` reads these off any node it resolves without asking what kind it is.
const COMMON_PROPERTIES: &[&str] =
    &["align_h", "align_v", "height", "hover", "id", "margin", "opacity", "padding", "visible", "width"];

/// What every kind that paints as a box takes on top of [`COMMON_PROPERTIES`]: the fill, then the
/// border. The set is `node::paint_style`'s own first match arm -- `row`, `column` and `button`
/// have no paint properties beyond a `rect`'s, and all four § 6 surface roles paint exactly like
/// one.
const BOX_PROPERTIES: &[&str] = &["background", "border_color", "border_width", "radius"];

/// Which kinds that arm covers.
const BOX_KINDS: [&str; 8] = ["rect", "row", "column", "button", "panel", "window", "popup", "lock"];

/// What each `kind` accepts beyond the two lists above, and the reason [`deserialize_lua_table`]
/// can reject an unknown key at all.
///
/// Until this existed, an unrecognized key was copied into `properties` and then read by nothing:
/// `aling_v = "Center"` in a config was silent, and the node just did not centre. Every parser
/// only ever asks for the keys it knows, so nothing was in a position to notice.
///
/// ponytail: hand-written, and it has to be. A node's schema is not a type -- it is roughly 60
/// `properties.get("...")` calls spread across `layout/node/`, `layout/scene.rs` and `wayland/`,
/// each with its own defaulting and coercion rules, so there is nothing to derive it from the way
/// `supervisor/src/stubs.rs` derives a capability payload. Two guards hold it in place:
/// `every_property_a_parser_reads_is_accepted` greps those calls out of the source, and
/// `the_stubs_declare_the_same_properties` compares it against `lua-meta`. The upgrade path is a
/// per-kind props struct the parsers read fields off, which is a rewrite of the parse layer rather
/// than a derive.
///
/// A name here that no parser reads yet is allowed and deliberate: `textfield`'s `on_change` and
/// `on_submit` are typed to ADR-0027's settled shape while `zwp_text_input_v3` is unwired, and
/// rejecting them would make a config written against the documented API fail to load.
const NODE_PROPERTIES: &[(&str, &[&str])] = &[
    ("rect", &["children"]),
    ("row", &["children", "scroll", "spacing"]),
    ("column", &["children", "scroll", "spacing"]),
    ("text", &["content", "elide", "font_size", "foreground", "text_align"]),
    // `foreground` here means what CSS `color` means: the value a `currentColor` fill in the
    // resolved SVG resolves to (docs/adr/0072). A full-colour icon names no `currentColor` and is
    // unaffected, so a config may pass it unconditionally.
    ("icon", &["foreground", "name", "size"]),
    ("image", &["fit", "source"]),
    ("button", &["children", "on_click"]),
    ("list", &["direction", "itemfn", "key", "scroll", "source", "spacing"]),
    // `font_size`, `foreground` and `text_align` are the text half `node::paint_style` reads off a
    // `textfield` too: it draws either its placeholder or its masked content.
    (
        "textfield",
        &[
            "font_size",
            "foreground",
            "mask_character",
            "on_change",
            "on_submit",
            "placeholder",
            "secure_submit",
            "text_align",
        ],
    ),
    ("panel", &["anchor", "child", "exclusive", "keyboard_interactivity", "layer", "monitor", "namespace"]),
    ("window", &["app_id", "child", "max_size", "min_size", "on_close", "title"]),
    (
        "popup",
        &[
            "anchor",
            "anchor_rect",
            "child",
            "constraint_adjustment",
            "grab",
            "gravity",
            "offset",
            "on_dismiss",
            "parent",
        ],
    ),
    ("lock", &["child"]),
];

/// Whether `kind` accepts `property`. An unknown `kind` accepts everything:
/// `register_node_constructors` is the only thing that tags a table with one, so a kind missing
/// from [`NODE_PROPERTIES`] is a new constructor whose row has not been written, and refusing
/// every property of it would be a worse failure than the silence this replaces.
fn accepts(kind: &str, property: &str) -> bool {
    let Some((_, own)) = NODE_PROPERTIES.iter().find(|(name, _)| *name == kind) else {
        return true;
    };
    own.contains(&property)
        || COMMON_PROPERTIES.contains(&property)
        || (BOX_KINDS.contains(&kind) && BOX_PROPERTIES.contains(&property))
}

/// Every property `kind` accepts, sorted, for the error message.
fn accepted_properties(kind: &str) -> Vec<&'static str> {
    let mut names: Vec<&'static str> =
        NODE_PROPERTIES.iter().find(|(name, _)| *name == kind).map_or_else(Vec::new, |(_, own)| own.to_vec());
    names.extend_from_slice(COMMON_PROPERTIES);
    if BOX_KINDS.contains(&kind) {
        names.extend_from_slice(BOX_PROPERTIES);
    }
    names.sort_unstable();
    names
}

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
    /// A key no parser of this `kind` reads. Rejected rather than copied through: see
    /// [`NODE_PROPERTIES`].
    #[error("`{kind}` has no property `{property}`; it accepts {accepted}")]
    UnknownProperty { kind: String, property: String, accepted: String },
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
        if !accepts(&kind, &key) {
            return Err(DeserializeError::UnknownProperty {
                kind: kind.clone(),
                property: key,
                accepted: accepted_properties(&kind).join(", "),
            });
        }
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

    /// The silence this replaces: `aling_v` was copied into `properties`, read by nothing, and
    /// the node just did not centre.
    #[test]
    fn a_misspelled_property_is_refused_and_the_message_names_what_the_kind_takes() {
        let lua = lua_with_constructors();
        let table: mlua::Table = lua.load(r#"return row { aling_v = "Center" }"#).eval().unwrap();

        let err = deserialize_lua_table(&table).unwrap_err().to_string();

        assert!(err.contains("aling_v"), "the message must name the key that was refused: {err}");
        assert!(err.contains("align_v"), "and the ones it accepts, so the typo is visible: {err}");
    }

    /// The per-kind half. `layer` is real § 6.1 topology, and meaningless on a `rect`.
    #[test]
    fn a_property_of_another_kind_is_refused_too() {
        let lua = lua_with_constructors();
        let table: mlua::Table = lua.load(r#"return rect { layer = "Top" }"#).eval().unwrap();
        assert!(deserialize_lua_table(&table).unwrap_err().to_string().contains("layer"));
    }

    /// A surface root takes the § 5.1 base properties and paints like a rect, so neither half is
    /// refused on one.
    #[test]
    fn a_surface_root_takes_the_base_properties_and_the_box_ones() {
        let lua = lua_with_constructors();
        let table: mlua::Table = lua
            .load(r##"return panel { id = "bar", layer = "Top", padding = { top = 4 }, radius = 8, opacity = 0.5 }"##)
            .eval()
            .unwrap();
        assert!(deserialize_lua_table(&table).is_ok());
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

    /// Every kind's `---@field` set, inherited classes folded in, against [`super::accepted_properties`].
    ///
    /// The stubs are what a config author sees in an editor, so a name here that the engine
    /// refuses is worse than a missing one: the completion offers it and the config fails to
    /// load. This ran red the day it was written -- `RowProps` declared no `background` while
    /// `node::paint_style` has painted one since ADR-0068, and all four surface classes were
    /// missing the `NodeBase` half they have always taken.
    #[test]
    fn the_stubs_declare_the_same_properties_the_engine_accepts() {
        let source = meta("nodes.lua") + &meta("surfaces.lua");
        let classes = parse_classes(&source);
        for kind in super::NODE_KINDS {
            let class = format!("{}Props", capitalize(kind));
            let declared = fields_of(&classes, &class);
            let expected: BTreeSet<String> = super::accepted_properties(kind).into_iter().map(str::to_string).collect();
            assert_eq!(declared, expected, "lua-meta's {class} is out of step with NODE_PROPERTIES for `{kind}`");
        }
    }

    /// Every property name the parsers actually read has to be accepted by some kind, or that
    /// parser is dead code reading a key `deserialize_lua_table` already refused.
    ///
    /// One-directional on purpose: a name in the table that no parser reads yet is fine and
    /// deliberate (`textfield`'s `on_change`/`on_submit`, typed while `zwp_text_input_v3` is
    /// unwired). A source grep, like `supervisor/src/stubs.rs`'s used to be, because a property
    /// name lives in a string literal and not in a type.
    #[test]
    fn every_property_a_parser_reads_is_accepted_by_some_kind() {
        let accepted: BTreeSet<String> =
            super::NODE_KINDS.iter().flat_map(|kind| super::accepted_properties(kind)).map(str::to_string).collect();
        let mut read = BTreeSet::new();
        for source in rust_sources(Path::new(env!("CARGO_MANIFEST_DIR")).join("src")) {
            let text = std::fs::read_to_string(&source).expect("a source file this build compiled is readable");
            // Below `#[cfg(test)]`, a fixture is free to name anything.
            let production = text.split_once("\n#[cfg(test)]").map_or(text.as_str(), |(before, _)| before);
            read.extend(property_literals(production));
        }
        let unreachable: Vec<&String> = read.difference(&accepted).collect();
        assert!(
            unreachable.is_empty(),
            "these parsers read a property no kind accepts, so `deserialize_lua_table` refuses it first: {unreachable:?}"
        );
    }

    /// `rect` -> `Rect`.
    fn capitalize(name: &str) -> String {
        let mut chars = name.chars();
        chars.next().map(|first| first.to_ascii_uppercase().to_string() + chars.as_str()).unwrap_or_default()
    }

    /// Each `---@class Name: Parent, Parent` and its own `---@field` names.
    fn parse_classes(source: &str) -> Vec<(String, Vec<String>, BTreeSet<String>)> {
        let mut classes: Vec<(String, Vec<String>, BTreeSet<String>)> = Vec::new();
        for line in source.lines() {
            if let Some(rest) = line.strip_prefix("---@class ") {
                let (name, parents) = match rest.split_once(':') {
                    Some((name, parents)) => (name.trim(), parents.split(',').map(|p| p.trim().to_string()).collect()),
                    None => (rest.trim(), Vec::new()),
                };
                classes.push((name.to_string(), parents, BTreeSet::new()));
            } else if let Some(rest) = line.strip_prefix("---@field ")
                && let Some((name, _)) = rest.split_once(char::is_whitespace)
                && let Some(current) = classes.last_mut()
            {
                current.2.insert(name.trim_end_matches('?').to_string());
            }
        }
        classes
    }

    /// One class's fields plus every parent's, which is what completion offers on it.
    fn fields_of(classes: &[(String, Vec<String>, BTreeSet<String>)], name: &str) -> BTreeSet<String> {
        let Some((_, parents, own)) = classes.iter().find(|(class, ..)| class == name) else {
            panic!("lua-meta declares no `{name}` class");
        };
        let mut fields = own.clone();
        for parent in parents {
            fields.extend(fields_of(classes, parent));
        }
        fields
    }

    fn rust_sources(root: PathBuf) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    out.push(path);
                }
            }
        }
        out
    }

    /// The two shapes a property name is written in: `properties.get("x")`, and the `"x"` a
    /// parser taking a property name is called with (`parse_align(properties, "align_v")`).
    fn property_literals(text: &str) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        for (index, _) in text.match_indices("properties") {
            let rest = &text[index + "properties".len()..];
            let head: String = rest.chars().take(40).collect();
            let opener = match head.trim_start().chars().next() {
                // `properties.get("x")` and its `remove`/`contains_key` siblings.
                Some('.') => head.find('"'),
                // `(properties, "x")`, the trailing argument of a name-taking parser.
                Some(',') => head.find('"'),
                _ => None,
            };
            let Some(quote) = opener else { continue };
            let Some(end) = head[quote + 1..].find('"') else { continue };
            let name = &head[quote + 1..quote + 1 + end];
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                names.insert(name.to_string());
            }
        }
        names
    }

    /// Every `shared::CAPABILITIES` name, as a field on the `Oblisk` class.
    #[test]
    fn the_stubs_declare_every_capability_and_no_others() {
        let source = meta("oblisk.lua");
        // The `---@field` block under `---@class Oblisk`, which is the namespace a config sees.
        // Split on the trailing newline too, or this matches `---@class ObliskVersion` first.
        let class = source.split("---@class Oblisk\n").nth(1).expect("oblisk.lua declares an Oblisk class");
        // The `oblisk` table's off-roster members, which have no `StateSnapshot` behind them and
        // so no roster entry: see `lua::namespace::build` and `lua::idle`.
        let off_roster = ["idle", "screens", "rescue", "version", "config_dir"];
        let declared: BTreeSet<&str> = class
            .lines()
            .take_while(|line| line.starts_with("---@field"))
            .filter_map(|line| line.split_whitespace().nth(1))
            .filter(|name| !off_roster.contains(name))
            .collect();
        let expected: BTreeSet<&str> = shared::CAPABILITIES.iter().copied().collect();
        assert_eq!(declared, expected, "lua-meta/oblisk.lua is out of step with shared::CAPABILITIES");
    }
}
