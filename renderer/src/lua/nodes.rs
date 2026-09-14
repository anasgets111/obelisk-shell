//! Node constructors and `VirtualNode`, the loader's shallow table-to-Rust conversion.
//!
//! ponytail: shallow by design. `deserialize_lua_table` reads `kind`, copies other keys unchanged,
//! never recurses into `children`/`child` (reconciliation's job), and does not validate shapes such
//! as `width` being an integer or `"Fill"` (the layout engine is the only typed-property consumer).

use std::collections::HashMap;

use mlua::{Lua, Table, Value};

/// Nine geometric nodes plus four root roles: `panel`, `window`, `popup`, `lock`
/// (ADR-0040). `lock` joined under ADR-0052 decision 2: declaration location is separate from
/// Wayland object lifetime (ADR-0049); `window`/`popup` wait for `visible`, `lock` for compositor
/// `locked`.
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

/// Properties every kind, including surface roles, takes: geometry, identity, and two flags.
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
    "min_height",
    "min_width",
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
/// `row`, `column`, `button`, `rect`, and all four root roles alike.
const BOX_PROPERTIES: &[&str] = &["background", "blur", "border_color", "border_width", "clip", "radius"];

/// Which kinds that arm covers.
const BOX_KINDS: [&str; 8] = ["rect", "row", "column", "button", "panel", "window", "popup", "lock"];

/// Per-kind properties beyond the common and box lists. This rejects unknown keys; before it,
/// misspelled `aling_v = "Center"` was copied, read by nothing, and silently failed to centre.
///
/// ponytail: hand-written because the schema is scattered `properties.get("...")` calls across
/// `layout/node/`, `layout/scene.rs`, and `wayland/`, each with its own defaulting/coercion.
/// Guards:
/// `every_property_a_parser_reads_is_accepted`, `the_stubs_declare_the_same_properties`. Upgrade:
/// per-kind props structs, which means rewriting the parsers.
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

    /// Per-kind check: `layer` is root-role topology, not a `rect` property.
    #[test]
    fn a_property_of_another_kind_is_refused_too() {
        let lua = lua_with_constructors();
        let table: mlua::Table = lua.load(r#"return rect { layer = "Top" }"#).eval().unwrap();
        assert!(deserialize_lua_table(&table).unwrap_err().to_string().contains("layer"));
    }

    /// A surface root takes the base and box properties, like `rect` paint.
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
    fn every_node_kind_constructs_and_tags_correctly() {
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
    fn image_is_a_constructor() {
        // Pin by name (ADR-0054 decision 3); the loop could pass after this entry was dropped,
        // silently removing wallpaper support.
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

/// `lua-meta/nodes.lua` and `lua-meta/surfaces.lua` stay hand-written: no type describes their
/// scattered `properties.get("...")` calls, each validating inline.
/// `lua-meta/obelisk.lua` is generated (`supervisor/src/stubs.rs`) because capability payloads are
/// real `Serialize` structs.
///
/// This guard covers the hand-written half, checking roster drift where a new kind lacks a stub;
/// the capability check stays here because this crate owns `shared::Capability::ALL`'s Lua
/// spelling.
#[cfg(test)]
mod meta_stub_tests {
    use std::collections::{BTreeMap, BTreeSet};
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

    /// Parser reads and accepted names match both ways: a read no kind accepts is refused first, an
    /// accepted name nothing reads is silently ignored. Source grep because names live in literals.
    #[test]
    fn every_property_a_parser_reads_is_accepted_by_some_kind() {
        let accepted: BTreeSet<String> =
            super::NODE_KINDS.iter().flat_map(|kind| super::accepted_properties(kind)).map(str::to_string).collect();
        let mut read = BTreeSet::new();
        for source in rust_sources(Path::new(env!("CARGO_MANIFEST_DIR")).join("src")) {
            let text = std::fs::read_to_string(&source).expect("a source file this build compiled is readable");
            read.extend(property_literals(&without_test_modules(&text)));
        }
        let unreachable: Vec<&String> = read.difference(&accepted).collect();
        assert!(
            unreachable.is_empty(),
            "these parsers read a property no kind accepts, so `deserialize_lua_table` refuses it first: {unreachable:?}"
        );
        let unread: Vec<&String> = accepted.difference(&read).collect();
        assert!(unread.is_empty(), "these properties are accepted but no parser reads them: {unread:?}");
    }

    /// Drops top-level `#[cfg(test)] mod … { … }` blocks, whose fixtures may name anything. Other
    /// `#[cfg(test)]` items often sit above production code, so they stay.
    fn without_test_modules(text: &str) -> String {
        let mut out = String::new();
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            if line == "#[cfg(test)]" && lines.peek().is_some_and(|next| next.contains("mod ") && next.ends_with('{')) {
                // rustfmt closes a top-level block with `}` in column 0.
                lines.by_ref().find(|line| *line == "}");
                continue;
            }
            out.push_str(line);
            out.push('\n');
        }
        out
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

    /// Property literals in `properties.get("x")`, as the named parser argument in
    /// `parse_align(properties, "align_v")`, or through `keyboard.rs`'s `function("on_cancel")`.
    fn property_literals(text: &str) -> BTreeSet<String> {
        let mut names: BTreeSet<String> = text
            .split("function(\"")
            .skip(1)
            .filter_map(|rest| rest.split_once("\")"))
            .map(|(name, _)| name.to_string())
            .filter(|name| name.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
            .collect();
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

    /// Feeds every `lua-meta` type through the evaluation-time surface specs, a real `Scene::apply`,
    /// and the resolved specs. Names are checked elsewhere; this catches type claims that the engine
    /// rejects, the ADR-0081 gap that once affected 21 properties, plus a closed literal set the
    /// engine does not close, an `integer` it lets take a fraction, and a required field it does
    /// not require. One-way by design: `just types` catches engine-accepted fields missing from the
    /// stub. Missing `sample` rows fail.
    /// ponytail: checks, does not derive. Upgrade to per-kind props structs, making `nodes.lua`
    /// generable like `obelisk.lua`; that rewrites parsing and trades property-specific errors for
    /// serde's. Not worth it while this test holds.
    #[test]
    fn every_type_the_stubs_declare_is_accepted_by_the_engine() {
        let source = meta("nodes.lua") + &meta("surfaces.lua");
        let classes = parse_typed_classes(&source);
        let aliases: BTreeMap<&str, &str> = source
            .lines()
            .filter_map(|line| line.strip_prefix("---@alias ")?.split_once(' '))
            .map(|(name, rest)| (name, declared_type(rest)))
            .collect();

        let mut report = Report::default();
        for kind in super::NODE_KINDS {
            let class = format!("{}Props", capitalize(kind));
            let fields = typed_fields(&classes, &class);
            let mut required: Vec<(String, String)> = Vec::new();
            for field in fields.iter().filter(|field| field.required) {
                match split_top(&field.ty, '|').into_iter().find_map(|member| sample(&field.name, member)) {
                    Some(literal) => required.push((field.name.clone(), literal)),
                    None => report.unsampled.push(format!("  {kind}.{}: `{}`", field.name, field.ty)),
                }
            }
            for (name, _) in &required {
                if apply_one(kind, &required, name, None).is_ok() {
                    report.failures.push(format!("  {kind}.{name} is declared required, engine accepts it absent"));
                }
            }
            for Field { name, ty, .. } in &fields {
                probe(&aliases, kind, &required, name, ty, &|literal| literal.to_string(), &mut report);
            }
        }
        // Alias internals no field declares directly; `@` is the probed slot.
        let easing = typed_fields(&classes, "Transition")
            .into_iter()
            .find(|f| f.name == "easing")
            .expect("Transition.easing")
            .ty;
        let steps = shape_field(&aliases, &easing, "steps").expect("Easing declares `{ steps }`");
        let loops = shape_field(&aliases, "Animation", "loops").expect("Animation declares `loops`");
        for (kind, field, ty, around) in [
            ("image", "transition", easing.as_str(), "{ duration = 400, easing = @ }"),
            ("image", "transition", steps, "{ duration = 400, easing = { steps = @ } }"),
            ("rect", "animate", loops, "{ opacity = { duration = 200, keyframes = { 0, 1 }, loops = @ } }"),
        ] {
            probe(&aliases, kind, &[], field, ty, &|literal| around.replace('@', literal), &mut report);
        }
        let Report { failures, unsampled } = report;
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
    }

    #[derive(Default)]
    struct Report {
        failures: Vec<String>,
        unsampled: Vec<String>,
    }

    /// Applies every member of `ty` as `field = around(literal)`. Union aliases and `(…)[]` expand,
    /// so each declared literal is probed. Returns the first literal, for `Bound` to wrap.
    fn probe(
        aliases: &BTreeMap<&str, &str>,
        kind: &str,
        required: &[(String, String)],
        field: &str,
        ty: &str,
        around: &dyn Fn(&str) -> String,
        report: &mut Report,
    ) -> Option<String> {
        let mut members = expand(aliases, ty);
        members.sort_by_key(|member| *member == "Bound");
        let mut first: Option<String> = None;
        for member in &members {
            if let Some(inner) = member.strip_prefix('(').and_then(|m| m.strip_suffix(")[]")) {
                let nested = probe(aliases, kind, required, field, inner, &|l| around(&format!("{{ {l} }}")), report);
                first = first.or(nested);
                continue;
            }
            let literal = match *member {
                // The engine resolves a handle before sibling rules apply, so wrap a sibling's
                // literal: `image.source` needs a string signal, `list.source` an array. The bare
                // `hover`/`geometry`/`scroll` fall back to their own rows.
                "Bound" => first.as_ref().map(|l| format!("state(\"probe\", {l})")).or_else(|| sample(field, member)),
                _ => sample(field, member).map(|l| around(&l)),
            };
            let Some(literal) = literal else {
                report.unsampled.push(format!("  {kind}.{field}: `{member}`"));
                continue;
            };
            if let Err(err) = apply_one(kind, required, field, Some(&literal)) {
                report.failures.push(format!("  {kind}.{field} declares `{member}`, engine says: {err}"));
            }
            if *member == "integer" && apply_one(kind, required, field, Some(&around("8.5"))).is_ok() {
                report
                    .failures
                    .push(format!("  {kind}.{field} declares `integer`, engine accepts `{}`", around("8.5")));
            }
            first.get_or_insert(literal);
        }
        let bogus = around("\"obelisk_bogus\"");
        if members.iter().any(|m| m.starts_with('"'))
            && !members.contains(&"string")
            && apply_one(kind, required, field, Some(&bogus)).is_ok()
        {
            report.failures.push(format!("  {kind}.{field} declares a closed literal set, engine accepts `{bogus}`"));
        }
        first
    }

    /// Lua literal for a declared type; `None` skips rather than guesses. Field name matters when
    /// spelling shares a type but not a domain: `opacity` is `[0, 1]`, `size` is pixels, and
    /// `border_color`'s `Edges` holds colors while `margin`'s holds lengths.
    fn sample(field: &str, ty: &str) -> Option<String> {
        match (field, ty) {
            ("opacity", _) => return Some("0.5".to_string()),
            ("animate", "Animations") => return Some("{ opacity = 200.5 }".to_string()),
            ("scale", "Axes") | ("translate", _) => return Some("{ x = 1, y = 2 }".to_string()),
            ("scale", _) => return Some("1.5".to_string()),
            ("rotate", _) => return Some("7.5".to_string()),
            ("origin", _) => return Some("{ x = 0.5, y = 0.5 }".to_string()),
            ("transition", "Transition") => return Some("{ duration = 400.5, easing = \"InOutCubic\" }".to_string()),
            ("border_color", "BorderColors") => return Some("{ top = \"#112233\" }".to_string()),
            // Inline table shapes have no alias.
            ("anchor", shape) if shape.starts_with('{') => return Some("{ top = true, left = true }".to_string()),
            ("min_size" | "max_size", _) => return Some("{ width = 8.5, height = 8.5 }".to_string()),
            ("offset", _) => return Some("{ x = 1.5, y = 1 }".to_string()),
            ("secure_submit", _) => {
                return Some("{ capability = \"lock\", action = \"authenticate\" }".to_string());
            }
            // Parsers check callbacks only as functions, except `list`'s two layout calls, which
            // use the return value. These three bare `Bound` fields take the handle itself.
            ("hover", _) => return Some("hover(\"probe\")".to_string()),
            ("geometry", _) => return Some("geometry(\"probe\")".to_string()),
            ("scroll", _) => return Some("scroll(\"probe\")".to_string()),
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
                // A `lock`'s refused `NodeBase` fields.
                "nil" => "nil",
                "integer" => "8",
                "number" => "8.5",
                "string" => "\"x\"",
                "boolean" => "true",
                "Color" => "\"#112233\"",
                "Percent" => "\"50%\"",
                "Edges" => "{ top = 1.5 }",
                "[number, number, number, number]" => "{ 0.25, 0.1, 0.25, 1 }",
                "{ steps: integer }" => "{ steps = 4 }",
                "Node" => "rect {}",
                "Node[]" => "{ rect {} }",
                "TextRun[]" => {
                    "{ { text = \"x\", bold = true, underline = true, color = \"#112233\", href = \"https://x/\" } }"
                }
                // No bare `Bound` row: callers wrap sibling samples, while the three bare fields
                // are handled above. A row would shadow both and feed every property the wrong
                // value.
                "Rect" => "{ x = 0, y = 0, width = 1, height = 1 }",
                literal if literal.starts_with('"') => literal,
                _ => return None,
            }
            .to_string(),
        )
    }

    /// Field companions, distinct from kind requirements. `on_hover` needs a same-node `hover`
    /// slot (ADR-0095), or a probe tests pairing rather than its declared type.
    fn companions(field: &str) -> &'static [(&'static str, &'static str)] {
        match field {
            "on_hover" => &[("hover", "hover(\"probe\")")],
            _ => &[],
        }
    }

    /// Applies `kind { required..., field = literal }` (`None` omits `field`) the way a reload does:
    /// `surface_specs`, `Scene::apply`, then the resolved spec `wayland::surface` builds. Surface
    /// roles are roots; other kinds hang under a minimal `panel`.
    fn apply_one(kind: &str, required: &[(String, String)], field: &str, literal: Option<&str>) -> Result<(), String> {
        let mut props: Vec<String> = required
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .chain(companions(field).iter().copied())
            .filter(|(name, _)| *name != field)
            .map(|(name, value)| format!("{name} = {value}"))
            .collect();
        props.extend(literal.map(|literal| format!("{field} = {literal}")));
        let node = format!("{kind} {{ {} }}", props.join(", "));
        let surface = if SURFACE_KINDS.contains(&kind) {
            node
        } else {
            format!("panel {{ id = \"probe\", layer = \"Top\", child = {node} }}")
        };

        let lua = mlua::Lua::new();
        super::register_node_constructors(&lua).map_err(|e| e.to_string())?;
        crate::lua::signal::register(&lua, crate::lua::signal::DirtyFlag::new()).map_err(|e| e.to_string())?;
        let table: mlua::Table = lua.load(format!("return {surface}")).eval().map_err(|e| e.to_string())?;
        let virtual_node = super::deserialize_lua_table(&table).map_err(|e| format!("{e:?}"))?;
        // Topology (`layer`, popup `anchor`, ...) is validated here and never read by `Scene::apply`.
        crate::lua::surfaces::surface_specs(&crate::lua::LoadOutput { surfaces: vec![virtual_node.clone()] })
            .map_err(|e| e.to_string())?;
        let mut scene = crate::layout::scene::Scene::new();
        let shaping = crate::text::shaping::ShapingHandle::spawn();
        // Keep `scene::tests::apply_at` `pub(super)`; widening a test helper is what the justfile's
        // `docs` baseline discourages. One instance and output suffice for this probe.
        let declared = crate::layout::node::parse_surface_id(&virtual_node.properties).map_err(|e| format!("{e:?}"))?;
        let instance_id = format!("{declared}@PROBE");
        let instances = [crate::layout::instance::SurfaceInstance {
            instance_id: instance_id.clone(),
            declared_id: declared,
            output: "PROBE".to_string(),
            available: crate::layout::LogicalSize { width: 1000.0, height: 500.0 },
            measured_axes: (false, false),
        }];
        scene.apply(std::slice::from_ref(&virtual_node), &instances, &shaping, &lua).map_err(|e| format!("{e:?}"))?;
        // A signal defers the evaluation-time spec, so the resolved one is where a bound value is checked.
        let resolved = &scene.surface(&instance_id).ok_or("the probe surface was not retained")?.properties;
        match virtual_node.kind.as_str() {
            "panel" => crate::layout::node::panel_spec(resolved).map(drop),
            "window" => crate::layout::node::window_spec(resolved).map(drop),
            "popup" => crate::layout::node::popup_spec(resolved).map(drop),
            _ => crate::layout::node::lock_spec(resolved).map(drop),
        }
        .map_err(|e| format!("{e:?}"))
    }

    const SURFACE_KINDS: [&str; 4] = ["panel", "window", "popup", "lock"];

    #[derive(Clone)]
    struct Field {
        name: String,
        ty: String,
        /// No `?` on the name.
        required: bool,
    }

    /// One `---@class`: name, parents, and own fields in order.
    type TypedClass = (String, Vec<String>, Vec<Field>);

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
                && let Some((name, rest)) = rest.split_once(' ')
            {
                current.2.push(Field {
                    name: name.trim_end_matches('?').to_string(),
                    ty: declared_type(rest).to_string(),
                    required: !name.ends_with('?'),
                });
            }
        }
        classes
    }

    /// The type at the start of `rest`: up to the first space outside brackets, reading a
    /// `fun(..): R` return past its colon.
    fn declared_type(rest: &str) -> &str {
        let (mut depth, mut prev) = (0, ' ');
        for (index, c) in rest.char_indices() {
            match c {
                '(' | '{' | '[' | '<' => depth += 1,
                ')' | '}' | ']' | '>' => depth -= 1,
                ' ' if depth == 0 && prev != ':' => return &rest[..index],
                _ => {}
            }
            prev = c;
        }
        rest
    }

    /// `ty` split on top-level `sep`; `("SlideX"|...)[]` and inline shapes stay whole.
    fn split_top(ty: &str, sep: char) -> Vec<&str> {
        let (mut depth, mut start, mut out) = (0, 0, Vec::new());
        for (index, c) in ty.char_indices() {
            match c {
                '(' | '{' | '[' | '<' => depth += 1,
                ')' | '}' | ']' | '>' => depth -= 1,
                c if c == sep && depth == 0 => {
                    out.push(&ty[start..index]);
                    start = index + 1;
                }
                _ => {}
            }
        }
        out.push(&ty[start..]);
        out
    }

    /// `ty`'s union members, with union aliases replaced by theirs.
    fn expand<'a>(aliases: &BTreeMap<&str, &'a str>, ty: &'a str) -> Vec<&'a str> {
        split_top(ty, '|')
            .into_iter()
            .flat_map(|member| match aliases.get(member) {
                Some(def) if split_top(def, '|').len() > 1 => expand(aliases, def),
                _ => vec![member],
            })
            .collect()
    }

    /// `key`'s type inside the `{ key: T, ... }` members `ty` expands to.
    fn shape_field<'a>(aliases: &BTreeMap<&str, &'a str>, ty: &'a str, key: &str) -> Option<&'a str> {
        expand(aliases, ty)
            .into_iter()
            .filter_map(|member| member.strip_prefix("{ ")?.strip_suffix(" }"))
            .flat_map(|body| split_top(body, ','))
            .find_map(|entry| {
                let (name, ty) = entry.trim().split_once(": ")?;
                (name.trim_end_matches('?') == key).then_some(ty)
            })
    }

    /// One class's fields plus every parent's.
    fn typed_fields(classes: &[TypedClass], name: &str) -> Vec<Field> {
        let Some((_, parents, own)) = classes.iter().find(|(class, ..)| class == name) else {
            panic!("lua-meta declares no `{name}` class");
        };
        let mut out = Vec::new();
        for parent in parents {
            out.extend(typed_fields(classes, parent));
        }
        // A redeclared field replaces the parent's, as the language server reads it.
        out.retain(|field: &Field| !own.iter().any(|mine| mine.name == field.name));
        out.extend(own.iter().cloned());
        out
    }

    /// Every `shared::Capability::ALL` name as an `Obelisk` field.
    #[test]
    fn the_stubs_declare_every_capability_and_no_others() {
        let source = meta("obelisk.lua");
        // The `---@field` block under `---@class Obelisk`, not `ObeliskVersion`.
        let class = source.split("---@class Obelisk\n").nth(1).expect("obelisk.lua declares an Obelisk class");
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
        assert_eq!(declared, expected, "lua-meta/obelisk.lua is out of step with shared::Capability::ALL");
    }
}
