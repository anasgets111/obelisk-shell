//! Node constructors (`oblisk-idl-api-specs.md` § 5.2/§ 6) and `VirtualNode`, the loader's shallow
//! table-to-Rust conversion.
//!
//! ponytail: shallow by design. `deserialize_lua_table` reads `kind`, copies other keys unchanged,
//! never recurses into `children`/`child` (reconciliation's job), and does not validate shapes such
//! as `width` being an integer or `"Fill"` (the layout engine is the only typed-property consumer).

use std::collections::HashMap;

use mlua::{Lua, Table, Value};

/// § 5.2's eight geometric nodes plus § 6's four root roles: `panel`, `window`, `popup`, `lock`
/// (ADR-0040). `lock` joined under ADR-0052 decision 2: declaration location is separate from
/// Wayland
/// object lifetime (ADR-0049); `window`/`popup` wait for `visible`, `lock` for compositor `locked`.
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

/// § 5.1 properties every kind, including surface roles, takes: geometry, identity, and two flags.
/// `layout::scene` reads them without checking kind.
const COMMON_PROPERTIES: &[&str] = &[
    "align_h",
    "align_v",
    "animate",
    "cursor",
    "geometry",
    "height",
    "hover",
    "id",
    "margin",
    "max_height",
    "max_width",
    "on_hover",
    "opacity",
    "origin",
    "padding",
    "rotate",
    "scale",
    "translate",
    "visible",
    "width",
];

/// Box-paint properties beyond [`COMMON_PROPERTIES`]. `node::paint_style`'s first arm paints
/// `row`, `column`, `button`, `rect`, and all four § 6 roles alike.
const BOX_PROPERTIES: &[&str] = &["background", "blur", "border_color", "border_width", "clip", "radius"];

/// Which kinds that arm covers.
const BOX_KINDS: [&str; 8] = ["rect", "row", "column", "button", "panel", "window", "popup", "lock"];

/// Per-kind properties beyond the common and box lists. This rejects unknown keys; before it,
/// misspelled `aling_v = "Center"` was copied, read by nothing, and silently failed to centre.
///
/// ponytail: hand-written because schema is ~60 `properties.get("...")` calls across
/// `layout/node/`, `layout/scene.rs`, and `wayland/`, each with its own defaulting/coercion.
/// Guards:
/// `every_property_a_parser_reads_is_accepted`, `the_stubs_declare_the_same_properties`. Upgrade:
/// per-kind props structs, which means rewriting the parsers.
///
/// Deliberate parser-less names: `textfield`'s `on_change`/`on_submit` follow ADR-0027 while
/// `zwp_text_input_v3` is unwired; rejecting them would break documented configs.
const NODE_PROPERTIES: &[(&str, &[&str])] = &[
    ("rect", &["children"]),
    ("row", &["children", "scroll", "spacing"]),
    ("column", &["children", "scroll", "spacing"]),
    ("text", &["content", "elide", "font", "font_size", "foreground", "max_lines", "on_link", "text_align", "wrap"]),
    // `foreground` is CSS `color`: the resolved SVG's `currentColor` fill (ADR-0072). Full-colour
    // icons name no `currentColor`, so this is safe.
    ("icon", &["foreground", "name", "size"]),
    ("image", &["async", "fit", "retain", "source", "transition"]),
    ("button", &["children", "on_click", "on_drag", "on_wheel", "submit"]),
    ("list", &["direction", "itemfn", "key", "scroll", "source", "spacing"]),
    // `node::paint_style` reads these for `textfield`, which draws a placeholder or masked content.
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

/// Whether `kind` accepts `property`. Unknown kinds accept all properties until their
/// [`NODE_PROPERTIES`] row is written; rejecting all would be worse than the silence being fixed.
pub(crate) fn accepts(kind: &str, property: &str) -> bool {
    let Some((_, own)) = NODE_PROPERTIES.iter().find(|(name, _)| *name == kind) else {
        return true;
    };
    own.contains(&property)
        || COMMON_PROPERTIES.contains(&property)
        || (BOX_KINDS.contains(&kind) && BOX_PROPERTIES.contains(&property))
}

/// Accepted properties, sorted for errors.
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

/// Lua node table tagged with `kind`, carrying other properties unchanged. Not the final scene
/// node.
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
    /// A key no parser for this `kind` reads; rejected instead of copied through
    /// ([`NODE_PROPERTIES`]).
    #[error("`{kind}` has no property `{property}`; it accepts {accepted}")]
    UnknownProperty { kind: String, property: String, accepted: String },
}

/// Registers each [`NODE_KINDS`] entry as a constructor that tags its props table with `kind`.
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

    /// Previously `aling_v` was copied, read by nothing, and silently failed to centre.
    #[test]
    fn a_misspelled_property_is_refused_and_the_message_names_what_the_kind_takes() {
        let lua = lua_with_constructors();
        let table: mlua::Table = lua.load(r#"return row { aling_v = "Center" }"#).eval().unwrap();

        let err = deserialize_lua_table(&table).unwrap_err().to_string();

        assert!(err.contains("aling_v"), "the message must name the key that was refused: {err}");
        assert!(err.contains("align_v"), "and the ones it accepts, so the typo is visible: {err}");
    }

    /// Per-kind check: `layer` is § 6 topology, not a `rect` property.
    #[test]
    fn a_property_of_another_kind_is_refused_too() {
        let lua = lua_with_constructors();
        let table: mlua::Table = lua.load(r#"return rect { layer = "Top" }"#).eval().unwrap();
        assert!(deserialize_lua_table(&table).unwrap_err().to_string().contains("layer"));
    }

    /// A surface root takes § 5.1 base and box properties, like `rect` paint.
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
        // Explicitly named; the loop would pass whatever the array contains.
        let lua = lua_with_constructors();
        assert!(NODE_KINDS.contains(&"window") && NODE_KINDS.contains(&"popup"));
        let table: Table = lua.load(r#"return popup { id = "menu", parent = "bar" }"#).eval().unwrap();
        assert_eq!(table.get::<String>("kind").unwrap(), "popup");
        assert_eq!(table.get::<String>("parent").unwrap(), "bar");
    }

    #[test]
    fn image_is_a_constructor_and_is_the_one_kind_section_5_2_does_not_list() {
        // ADR-0054 decision 3 adds this outside § 5.2's eight; pin the name so dropping it fails
        // loudly
        // instead of silently removing wallpaper support.
        let lua = lua_with_constructors();
        assert!(NODE_KINDS.contains(&"image"));
        let table: Table = lua.load(r#"return image { source = "/tmp/wall.png", fit = "cover" }"#).eval().unwrap();
        assert_eq!(table.get::<String>("kind").unwrap(), "image");
        assert_eq!(table.get::<String>("source").unwrap(), "/tmp/wall.png");
        assert_eq!(table.get::<String>("fit").unwrap(), "cover");
    }

    #[test]
    fn lock_is_a_constructor_a_config_can_call_because_declaring_one_is_not_locking() {
        // Pin by name (ADR-0052 decision 2); the loop could pass after this entry was dropped.
        let lua = lua_with_constructors();
        assert!(NODE_KINDS.contains(&"lock"));
        let table: Table = lua.load(r#"return lock { id = "screen-lock" }"#).eval().unwrap();
        assert_eq!(table.get::<String>("kind").unwrap(), "lock");
        assert_eq!(table.get::<String>("id").unwrap(), "screen-lock");
    }
}

/// `lua-meta/nodes.lua` and `lua-meta/surfaces.lua` stay hand-written: no type describes their 29
/// scattered `properties.get("...")` calls across `layout/node/`, each validating inline.
/// `lua-meta/oblisk.lua` is generated (`supervisor/src/stubs.rs`) because capability payloads are
/// real `Serialize` structs.
///
/// This guard covers the hand-written half, checking roster drift where a new kind lacks a stub;
/// the capability check stays here because this crate owns `shared::Capability::ALL`'s Lua
/// spelling.
#[cfg(test)]
mod meta_stub_tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    fn meta(file: &str) -> String {
        let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../lua-meta").join(file);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("{} is missing or unreadable: {err}", path.display()))
    }

    /// Every callable node constructor, exactly once.
    #[test]
    fn the_stubs_declare_every_node_kind_and_no_others() {
        let source = meta("nodes.lua") + &meta("surfaces.lua");
        let declared: BTreeSet<&str> =
            source.lines().filter_map(|line| line.strip_prefix("function ")?.split('(').next()).collect();
        let expected: BTreeSet<&str> = super::NODE_KINDS.iter().copied().collect();
        assert_eq!(declared, expected, "lua-meta is out of step with NODE_KINDS");
    }

    /// Every kind's inherited `---@field` set matches [`super::accepted_properties`]. Editor
    /// stubs offering a refused name are worse than omission. This first ran red because `RowProps`
    /// lacked `background` despite `node::paint_style` painting it since ADR-0068, and all four
    /// surface classes lacked their long-standing `NodeBase` fields.
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

    /// Every parser-read property must be accepted by some kind, or deserialization refuses it
    /// first. One-way by design: parser-less names remain allowed (`textfield`'s `on_change`/
    /// `on_submit`, typed while `zwp_text_input_v3` is unwired). Source grep is needed because
    /// property names live in string literals, not types.
    #[test]
    fn every_property_a_parser_reads_is_accepted_by_some_kind() {
        let accepted: BTreeSet<String> =
            super::NODE_KINDS.iter().flat_map(|kind| super::accepted_properties(kind)).map(str::to_string).collect();
        let mut read = BTreeSet::new();
        for source in rust_sources(Path::new(env!("CARGO_MANIFEST_DIR")).join("src")) {
            let text = std::fs::read_to_string(&source).expect("a source file this build compiled is readable");
            // Test fixtures below `#[cfg(test)]` may name anything.
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

    /// Property literals in `properties.get("x")` or as the named parser argument in
    /// `parse_align(properties, "align_v")`.
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

    /// Feeds every `lua-meta` type through a real `Scene::apply`. Names are checked elsewhere; this
    /// catches type claims that the engine rejects, the ADR-0081 gap that once affected 21
    /// properties. One-way by design: `just types` catches engine-accepted fields missing from the
    /// stub by checking `dev-config`. All 455 types are sampled; missing `sample` rows fail.
    /// ponytail: checks, does not derive. The node schema remains hand-written: 49 parse functions
    /// and 45 `properties.get` calls across ten files. Upgrade to per-kind props structs, making
    /// `nodes.lua` generable like `oblisk.lua`; that rewrites parsing and trades property-specific
    /// errors for serde's. Not worth it while this test holds.
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
                // Split only flat unions. `constraint_adjustment` is `("SlideX"|...)[]`, an array
                // of a union, and inline table shapes contain their own `|`; keep those whole.
                // `string|TextRun[]|Bound` is safe to split.
                let members: Vec<&str> =
                    if ty.contains(['(', '{']) { vec![ty.as_str()] } else { ty.split('|').collect() };
                for member in members {
                    // `Bound` carries a handle; the engine resolves it, then applies sibling rules.
                    // Wrap a sibling sample, falling back to the field sample. Order is
                    // load-bearing: `image.source` (`string|Bound`) needs a string signal, while
                    // `list.source` (`Bound`) needs an array; field-first gives both arrays,
                    // wrap-first both strings.
                    let literal = if member == "Bound" {
                        // Bracketed types never split, so cannot reach `Bound` here.
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
        // A skip is a hole, so fail; add its spelling to `sample`. No "cannot be probed" arm.
        assert!(
            unsampled.is_empty(),
            "{} declared type(s) have no sample, so nothing checked them. Add a row to `sample`:\n{}",
            unsampled.len(),
            unsampled.join("\n")
        );
        assert_eq!(probed, 766, "the number of declared type members moved; confirm the change is intended");
    }

    /// Lua literal for a declared type; `None` skips rather than guesses. Field name matters when
    /// spelling shares a type but not a domain: `opacity` is `[0, 1]`, `size` is pixels, and
    /// `border_color`'s `Edges` holds colors while `margin`'s holds lengths.
    fn sample(field: &str, ty: &str) -> Option<String> {
        match (field, ty) {
            ("opacity", _) => return Some("0.5".to_string()),
            ("animate", _) => return Some("{ opacity = 200 }".to_string()),
            ("scale", "Axes") | ("translate", _) => return Some("{ x = 1, y = 2 }".to_string()),
            ("scale", _) => return Some("1.5".to_string()),
            ("rotate", _) => return Some("15".to_string()),
            ("origin", _) => return Some("{ x = 0.5, y = 0.5 }".to_string()),
            ("transition", _) => return Some("{ duration = 400, easing = \"InOutCubic\" }".to_string()),
            ("border_color", "BorderColors") => return Some("{ top = \"#112233\" }".to_string()),
            ("constraint_adjustment", _) => return Some("{ \"SlideX\" }".to_string()),
            // Inline table shapes have no alias.
            ("anchor", shape) if shape.starts_with('{') => return Some("{ top = true, left = true }".to_string()),
            ("min_size" | "max_size", _) => return Some("{ width = 8, height = 8 }".to_string()),
            ("offset", _) => return Some("{ x = 1, y = 1 }".to_string()),
            ("secure_submit", _) => {
                return Some("{ capability = \"lock\", action = \"authenticate\" }".to_string());
            }
            // Parsers check callbacks only as functions, except `list`'s two layout calls, which
            // use the return value. These three bare `Bound` fields take the handle itself.
            ("hover", _) => return Some("hover(\"probe\")".to_string()),
            ("geometry", _) => return Some("geometry(\"probe\")".to_string()),
            ("scroll", _) => return Some("scroll(\"probe\")".to_string()),
            ("source", "Bound") => return Some("SIGNAL_LIST".to_string()),
            // Literal-array `list.source` is fixed for the pass; real lists therefore use the
            // adjacent signal (ADR-0113 decision 3).
            ("source", "any[]") => return Some("{ 1, 2 }".to_string()),
            ("itemfn", _) => return Some("function(item) return rect {} end".to_string()),
            // `panel`/`lock` per-output builder (ADR-0121), called with `"PROBE"` here.
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
                // No bare `Bound` row: callers wrap sibling samples, while the three bare fields
                // are handled above. A row would shadow both and feed every property the wrong
                // value.
                "Rect" => "{ x = 0, y = 0, width = 1, height = 1 }",
                "PopupAnchor" => "\"Top\"",
                // First string-literal union member stands for all; the parser matches one `match`.
                literal if literal.starts_with('"') => literal,
                _ => return None,
            }
            .to_string(),
        )
    }

    /// Required properties per kind, so optional-field probes can build. Taken from non-optional
    /// stub fields.
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

    /// Field companions, distinct from kind requirements. `on_hover` needs a same-node `hover`
    /// slot (ADR-0095), or a probe tests pairing rather than its declared type.
    fn companions(field: &str) -> &'static [(&'static str, &'static str)] {
        match field {
            "on_hover" => &[("hover", "hover(\"probe\")")],
            _ => &[],
        }
    }

    /// Applies `kind { field = literal }` through `Scene::apply`, calling all 49 parsers. Surface
    /// roles are roots; other kinds hang under a minimal `panel`.
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
        // Use `state`; no bare `signal()` global exists. The five registered globals are `state`,
        // `computed`, `hover`, `hover_rect`, and `scroll`, matching `lua-meta/signals.lua`.
        let prelude = r#"
            local SIGNAL_LIST = state("probe_list", { 1, 2 })
        "#;
        let table: mlua::Table = lua.load(format!("{prelude}\nreturn {surface}")).eval().map_err(|e| e.to_string())?;
        let virtual_node = super::deserialize_lua_table(&table).map_err(|e| format!("{e:?}"))?;
        let mut scene = crate::layout::scene::Scene::new();
        let shaping = crate::text::shaping::ShapingHandle::spawn();
        // Keep `scene::tests::apply_at` `pub(super)`; widening a test helper is what the justfile's
        // `docs` baseline discourages. One instance and output suffice for this probe.
        let declared = crate::layout::node::parse_surface_id(&virtual_node.properties).map_err(|e| format!("{e:?}"))?;
        let instances = [crate::layout::instance::SurfaceInstance {
            instance_id: format!("{declared}@PROBE"),
            declared_id: declared,
            output: "PROBE".to_string(),
            available: crate::layout::LogicalSize { width: 1000.0, height: 500.0 },
            measured_axes: (false, false),
        }];
        scene.apply(std::slice::from_ref(&virtual_node), &instances, &shaping, &lua).map_err(|e| format!("{e:?}"))
    }

    const SURFACE_KINDS: [&str; 4] = ["panel", "window", "popup", "lock"];

    /// One `---@class`: name, parents, and own `(field, declared type)` pairs in order.
    type TypedClass = (String, Vec<String>, Vec<(String, String)>);

    /// [`parse_classes`] with each field's declared type.
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

    /// One class's pairs plus every parent's.
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

    /// Every `shared::Capability::ALL` name as an `Oblisk` field.
    #[test]
    fn the_stubs_declare_every_capability_and_no_others() {
        let source = meta("oblisk.lua");
        // The `---@field` block under `---@class Oblisk`, not `ObliskVersion`.
        let class = source.split("---@class Oblisk\n").nth(1).expect("oblisk.lua declares an Oblisk class");
        // Off-roster members lack a `StateSnapshot` and roster entry (`lua::namespace::build`).
        // `idle` left this list under ADR-0141: it is now a roster capability wrapped for three
        // callbacks that cannot cross the wire.
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
