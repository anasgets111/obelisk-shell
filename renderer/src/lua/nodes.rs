//! Node constructors (`oblisk-idl-api-specs.md` § 5.2/§ 6.1) and `VirtualNode`, the loader's
//! shallow, unvalidated table-to-Rust conversion.
//!
//! ponytail: shallow by design. `deserialize_lua_table` reads `kind`, copies every other key
//! as-is into `properties`, and never recurses into a nested `children`/`child` table (that's
//! reconciliation's job) or validates a value's shape, e.g. a `width` that's neither an integer
//! nor `"Fill"` (that's the layout engine's, the only real consumer of typed properties).

use std::collections::HashMap;

use mlua::{Lua, Table, Value};

/// § 5.2's eight geometric nodes plus all four top-level surface roles a config declares: § 6.1's
/// `panel`, § 6.2's `window`, § 6.3's `popup` and § 6.4's `lock` (ADR-0040: surface *roles*;
/// "surface" is the umbrella term). `lock` joined under ADR-0052 decision 2: a constructor only
/// decides where a declaration is *written*, separate (ADR-0049) from when the Wayland object
/// exists. `window`/`popup` own no `xdg_toplevel`/`xdg_popup` until `visible`; `lock`'s trigger
/// is the compositor's `locked` event instead.
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
const COMMON_PROPERTIES: &[&str] = &[
    "align_h",
    "align_v",
    "cursor",
    "height",
    "hover",
    "id",
    "margin",
    "max_height",
    "max_width",
    "on_hover",
    "opacity",
    "padding",
    "visible",
    "width",
];

/// What every kind that paints as a box takes on top of [`COMMON_PROPERTIES`]: the fill, then the
/// border. This is `node::paint_style`'s first match arm: `row`, `column` and `button` paint no
/// differently from a `rect`, and neither do the four § 6 surface roles.
const BOX_PROPERTIES: &[&str] = &["background", "border_color", "border_width", "clip", "radius"];

/// Which kinds that arm covers.
const BOX_KINDS: [&str; 8] = ["rect", "row", "column", "button", "panel", "window", "popup", "lock"];

/// What each `kind` accepts beyond the two lists above, and why [`deserialize_lua_table`] can
/// reject an unknown key at all: before this, a misspelled `aling_v = "Center"` was copied into
/// `properties` and read by nothing, so the node silently didn't centre.
///
/// ponytail: hand-written, because a node's schema is not a type: it's ~60 `properties.get("...")`
/// calls across `layout/node/`, `layout/scene.rs` and `wayland/`, each with its own defaulting and
/// coercion, guarded against drift by `every_property_a_parser_reads_is_accepted` and
/// `the_stubs_declare_the_same_properties`. Upgrade path: a per-kind props struct, a rewrite.
///
/// A name here that no parser reads yet is deliberate: `textfield`'s `on_change`/`on_submit` are
/// typed to ADR-0027's shape while `zwp_text_input_v3` is unwired; rejecting them would fail a
/// config written against the documented API.
const NODE_PROPERTIES: &[(&str, &[&str])] = &[
    ("rect", &["children"]),
    ("row", &["children", "scroll", "spacing"]),
    ("column", &["children", "scroll", "spacing"]),
    ("text", &["content", "elide", "font_size", "foreground", "max_lines", "on_link", "text_align", "wrap"]),
    // `foreground` means what CSS `color` means: what a `currentColor` fill in the resolved SVG
    // resolves to (ADR-0072). A full-colour icon names no `currentColor`, so this is always safe.
    ("icon", &["foreground", "name", "size"]),
    ("image", &["async", "fit", "source"]),
    ("button", &["children", "on_click", "on_drag", "on_wheel", "submit"]),
    ("list", &["direction", "itemfn", "key", "scroll", "source", "spacing"]),
    // `font_size`, `foreground` and `text_align` are the text half `node::paint_style` reads off a
    // `textfield` too: it draws either its placeholder or its masked content.
    (
        "textfield",
        &[
            "autofocus",
            "font_size",
            "foreground",
            "mask_character",
            "on_cancel",
            "on_change",
            "on_navigate",
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

/// Whether `kind` accepts `property`. An unknown `kind` accepts everything: it's a new
/// constructor whose [`NODE_PROPERTIES`] row isn't written yet, and refusing it all would be
/// worse than the silence this replaces.
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
/// untouched. Not the final in-memory scene node. See the module doc comment.
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
/// passed and tags it with `kind`. One loop, so adding a role is one edit.
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
        // ADR-0054 decision 3 adds this outside § 5.2's eight; pinned by name so an edit
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
        // Pinned by name (ADR-0052 decision 2) rather than by the loop above, which would go
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
/// crate is the one that owns `shared::Capability::ALL`'s Lua-side spelling.
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

    /// Every type `lua-meta` declares, fed to the engine through a real `Scene::apply`.
    ///
    /// The other two tests here check *names*. This one checks the claims the names carry, which is
    /// the half nothing checked until ADR-0081: 21 properties declared a type that refused a
    /// binding the engine takes, and the stubs had said so for as long as they had existed.
    ///
    /// It runs in this direction only. A member the engine accepts and the stub omits is invisible
    /// here, and is what `just types` catches from the other side by checking `dev-config` against
    /// these same declarations. Together they close the loop: this proves the stub does not promise
    /// what the engine refuses, and `just types` proves a config written to the stub compiles.
    ///
    /// All 455 of them, with no skips: a type `sample` has no row for fails the test rather than
    /// passing quietly, so the table cannot rot into covering half the file.
    ///
    /// ponytail: this checks the types, it does not derive them. `lua-meta/nodes.lua` is still
    /// hand-written, because a node's schema is 49 parse functions and 45 `properties.get` calls
    /// across ten files rather than a type to hang a derive on. The upgrade path is a per-kind
    /// props struct the parsers read fields off, which would make the file generable the way
    /// `oblisk.lua` is; it is a rewrite of the parse layer, and it would trade this crate's
    /// property-by-property error messages for serde's. Not worth it while this test holds.
    #[test]
    fn every_type_the_stubs_declare_is_accepted_by_the_engine() {
        let source = meta("nodes.lua") + &meta("surfaces.lua");
        let classes = parse_typed_classes(&source);

        let mut failures: Vec<String> = Vec::new();
        let mut unsampled: Vec<String> = Vec::new();
        let mut probed = 0usize;
        for kind in super::NODE_KINDS {
            let class = format!("{}Props", capitalize(kind));
            for (field, ty) in typed_fields(&classes, &class) {
                // Split on `|` only when the spelling is a flat union. `constraint_adjustment` is
                // `("SlideX"|"SlideY"|...)[]`, an array *of* a union, and splitting it yields
                // fragments that are not types; an inline table shape holds `|` of its own. Those
                // go in the sample table whole, or not at all. A bare `[]` suffix is fine to split
                // around: `string|TextRun[]|Bound` is three types, one of them an array.
                let members: Vec<&str> =
                    if ty.contains(['(', '{']) { vec![ty.as_str()] } else { ty.split('|').collect() };
                for member in members {
                    // `Bound` is not a type of its own here, it is a carrier: the engine
                    // resolves the handle and then applies the sibling member's rules to what came
                    // out. So the probe wraps a sibling's sample rather than an arbitrary value,
                    // which is the difference between testing `align_h = Bound` and testing
                    // `align_h = <a signal holding 8>`.
                    // `Bound` prefers to wrap a sibling, and only falls back to the field's own
                    // sample when it has none. The order matters both ways: `image.source` is
                    // `string|Bound` and wants a signal holding a string, while `list.source` is
                    // `Bound` alone and wants one holding an array. Keying on the field first would
                    // give both the array; keying on the wrap first would give both the string.
                    let literal = if member == "Bound" {
                        // `ty.split` is safe here: a bracketed type never splits, so it never
                        // reaches this branch with `member == "Bound"`.
                        ty.split('|')
                            .filter(|m| *m != "Bound")
                            .find_map(|m| sample(&field, m))
                            .map(|inner| format!("state(\"probe\", {inner})"))
                            .or_else(|| sample(&field, member))
                    } else {
                        sample(&field, member)
                    };
                    let Some(literal) = literal else {
                        unsampled.push(format!("  {kind}.{field}: `{member}`"));
                        continue;
                    };
                    probed += 1;
                    if let Err(err) = apply_one(kind, &field, &literal) {
                        failures.push(format!("  {kind}.{field} declares `{member}`, engine says: {err}"));
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} declared type(s) the engine refuses:\n{}",
            failures.len(),
            failures.join("\n")
        );
        // A skip is a hole in the check, so it fails here rather than passing quietly. Adding a
        // spelling to `sample` is how you close one; there is no arm for "cannot be probed".
        assert!(
            unsampled.is_empty(),
            "{} declared type(s) have no sample, so nothing checked them. Add a row to `sample`:\n{}",
            unsampled.len(),
            unsampled.join("\n")
        );
        assert_eq!(probed, 585, "the number of declared type members moved; confirm the change is intended");
    }

    /// One Lua literal per declared type. `None` means "no sample", which skips rather than guesses.
    ///
    /// Takes the field name because two properties share a spelling and not a domain: `opacity` is
    /// a `number` in `[0, 1]` where `size` is a `number` of pixels, and `border_color`'s `Edges`
    /// holds colours where `margin`'s holds lengths. A type-only table would probe those with a
    /// value the engine is right to refuse, and the test would be reporting itself.
    fn sample(field: &str, ty: &str) -> Option<String> {
        match (field, ty) {
            ("opacity", _) => return Some("0.5".to_string()),
            ("border_color", "BorderColors") => return Some("{ top = \"#112233\" }".to_string()),
            ("constraint_adjustment", _) => return Some("{ \"SlideX\" }".to_string()),
            // The inline table shapes, which have no alias to key off.
            ("anchor", shape) if shape.starts_with('{') => return Some("{ top = true, left = true }".to_string()),
            ("min_size" | "max_size", _) => return Some("{ width = 8, height = 8 }".to_string()),
            ("offset", _) => return Some("{ x = 1, y = 1 }".to_string()),
            ("secure_submit", _) => {
                return Some("{ capability = \"lock\", action = \"authenticate\" }".to_string());
            }
            // A callback: the parsers ask whether it is a function, not what its arity is. The two
            // a `list` calls during layout are the exception, since it uses what comes back.
            // Typed `Bound` with nothing beside it: these three take the handle itself.
            ("hover", _) => return Some("hover(\"probe\")".to_string()),
            ("scroll", _) => return Some("scroll(\"probe\")".to_string()),
            ("source", "Bound") => return Some("SIGNAL_LIST".to_string()),
            // The literal-array half of `list.source`: legal, and fixed for the life of the pass,
            // which is why the signal beside it is what a real list uses (ADR-0113 decision 3).
            ("source", "any[]") => return Some("{ 1, 2 }".to_string()),
            ("itemfn", _) => return Some("function(item) return rect {} end".to_string()),
            // `panel`/`lock`'s per-output builder (ADR-0121): the probe calls it with `"PROBE"`.
            ("child", shape) if shape.contains("fun(") => {
                return Some("function(output) return rect {} end".to_string());
            }
            ("key", _) => return Some("function(item) return tostring(item) end".to_string()),
            (_, shape) if shape.starts_with("fun(") || shape.starts_with("fun()") => {
                return Some("function() end".to_string());
            }
            _ => {}
        }
        Some(
            match ty {
                "integer" | "number" => "8",
                "string" => "\"x\"",
                "boolean" => "true",
                "Color" => "\"#112233\"",
                "Length" => "\"Fill\"",
                "Edges" => "{ top = 1 }",
                "Node" => "rect {}",
                "Node[]" => "{ rect {} }",
                "TextRun[]" => {
                    "{ { text = \"x\", bold = true, underline = true, color = \"#112233\", href = \"https://x/\" } }"
                }
                "Align" => "\"Center\"",
                "Cursor" => "\"pointer\"",
                // No row for a bare `Bound`: the caller wraps a sibling member's sample instead,
                // and the three properties typed `Bound` alone are handled by field above. A row
                // here would shadow both and probe every property with the same wrong payload.
                "Rect" => "{ x = 0, y = 0, width = 1, height = 1 }",
                "PopupAnchor" => "\"Top\"",
                // A string-literal union: its first member stands for all of them, since the
                // parser matches them in one `match`.
                literal if literal.starts_with('"') => literal,
                _ => return None,
            }
            .to_string(),
        )
    }

    /// The properties each kind cannot be built without, so a probe of one optional field is not
    /// rejected for the absence of a required one. Taken from the stubs' own non-optional fields.
    fn required(kind: &str) -> &'static [(&'static str, &'static str)] {
        match kind {
            "panel" => &[("id", "\"probe\""), ("layer", "\"Top\"")],
            "window" | "lock" => &[("id", "\"probe\"")],
            "popup" => &[
                ("id", "\"probe\""),
                ("parent", "\"host\""),
                ("anchor_rect", "{ x = 0, y = 0, width = 1, height = 1 }"),
                ("width", "8"),
                ("height", "8"),
            ],
            "list" => &[("source", "SIGNAL_LIST"), ("itemfn", "function(item) return rect {} end")],
            _ => &[],
        }
    }

    /// What a *field* needs a sibling for, as opposed to what a [`required`] kind does. `on_hover`
    /// is refused without a `hover` slot on the same node (ADR-0095), so probing it alone would be
    /// testing the pairing rule rather than the type the stub declares for it.
    fn companions(field: &str) -> &'static [(&'static str, &'static str)] {
        match field {
            "on_hover" => &[("hover", "hover(\"probe\")")],
            _ => &[],
        }
    }

    /// `kind { field = literal }` through `Scene::apply`, which is what actually calls all 49
    /// parsers. A surface role is its own root; anything else hangs under a minimal `panel`.
    fn apply_one(kind: &str, field: &str, literal: &str) -> Result<(), String> {
        let mut props: Vec<String> = required(kind)
            .iter()
            .chain(companions(field))
            .filter(|(name, _)| *name != field)
            .map(|(name, value)| format!("{name} = {value}"))
            .collect();
        props.push(format!("{field} = {literal}"));
        let node = format!("{kind} {{ {} }}", props.join(", "));
        let surface = if SURFACE_KINDS.contains(&kind) {
            node
        } else {
            format!("panel {{ id = \"probe\", layer = \"Top\", child = {node} }}")
        };

        let lua = mlua::Lua::new();
        super::register_node_constructors(&lua).map_err(|e| e.to_string())?;
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).map_err(|e| e.to_string())?;
        // `state` rather than a bare `signal()`, because there is no such global: the five the
        // engine registers are `state`, `computed`, `hover`, `hover_rect` and `scroll`, which is
        // what `lua-meta/signals.lua` declares.
        let prelude = r#"
            local SIGNAL_LIST = state("probe_list", { 1, 2 })
        "#;
        let table: mlua::Table = lua.load(format!("{prelude}\nreturn {surface}")).eval().map_err(|e| e.to_string())?;
        let virtual_node = super::deserialize_lua_table(&table).map_err(|e| format!("{e:?}"))?;
        let mut scene = crate::layout::scene::Scene::new();
        let shaping = crate::text::shaping::ShapingHandle::spawn();
        // `scene::tests::apply_at` is `pub(super)` and stays that way: widening a test helper's
        // visibility to reach it from here is the move the `docs` baseline in the justfile exists to
        // discourage. One instance, one output, which is all this probe needs.
        let declared = crate::layout::node::parse_surface_id(&virtual_node.properties).map_err(|e| format!("{e:?}"))?;
        let instances = [crate::layout::instance::SurfaceInstance {
            instance_id: format!("{declared}@PROBE"),
            declared_id: declared,
            output: "PROBE".to_string(),
            available: crate::layout::LogicalSize { width: 1000.0, height: 500.0 },
        }];
        scene.apply(std::slice::from_ref(&virtual_node), &instances, &shaping, &lua).map_err(|e| format!("{e:?}"))
    }

    const SURFACE_KINDS: [&str; 4] = ["panel", "window", "popup", "lock"];

    /// One `---@class` as the checker needs it: its name, the classes it extends, and its own
    /// `(field, declared type)` pairs in declaration order.
    type TypedClass = (String, Vec<String>, Vec<(String, String)>);

    /// Like [`parse_classes`], keeping each field's declared type alongside its name.
    fn parse_typed_classes(source: &str) -> Vec<TypedClass> {
        let mut classes: Vec<TypedClass> = Vec::new();
        for line in source.lines() {
            if let Some(rest) = line.strip_prefix("---@class ") {
                let (name, parents) = match rest.split_once(':') {
                    Some((name, parents)) => (name.trim(), parents.split(',').map(|p| p.trim().to_string()).collect()),
                    None => (rest.trim(), Vec::new()),
                };
                classes.push((name.to_string(), parents, Vec::new()));
            } else if let Some(rest) = line.strip_prefix("---@field ")
                && let Some(current) = classes.last_mut()
            {
                let mut parts = rest.split_whitespace();
                if let (Some(name), Some(ty)) = (parts.next(), parts.next()) {
                    current.2.push((name.trim_end_matches('?').to_string(), ty.to_string()));
                }
            }
        }
        classes
    }

    /// One class's `(field, type)` pairs plus every parent's.
    fn typed_fields(classes: &[TypedClass], name: &str) -> Vec<(String, String)> {
        let Some((_, parents, own)) = classes.iter().find(|(class, ..)| class == name) else {
            panic!("lua-meta declares no `{name}` class");
        };
        let mut out = Vec::new();
        for parent in parents {
            out.extend(typed_fields(classes, parent));
        }
        out.extend(own.iter().cloned());
        out
    }

    /// Every `shared::Capability::ALL` name, as a field on the `Oblisk` class.
    #[test]
    fn the_stubs_declare_every_capability_and_no_others() {
        let source = meta("oblisk.lua");
        // The `---@field` block under `---@class Oblisk`, which is the namespace a config sees.
        // Split on the trailing newline too, or this matches `---@class ObliskVersion` first.
        let class = source.split("---@class Oblisk\n").nth(1).expect("oblisk.lua declares an Oblisk class");
        // The `oblisk` table's off-roster members, which have no `StateSnapshot` behind them and
        // so no roster entry: see `lua::namespace::build`. `idle` left this list with ADR-0141 --
        // it is a roster capability now, wrapped in its own userdata for the three methods whose
        // callbacks cannot cross the wire.
        let off_roster = ["screens", "rescue", "version", "config_dir"];
        let declared: BTreeSet<&str> = class
            .lines()
            .take_while(|line| line.starts_with("---@field"))
            .filter_map(|line| line.split_whitespace().nth(1))
            .filter(|name| !off_roster.contains(name))
            .collect();
        let expected: BTreeSet<&str> = shared::Capability::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(declared, expected, "lua-meta/oblisk.lua is out of step with shared::Capability::ALL");
    }
}
