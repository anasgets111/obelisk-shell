//! Generates `lua-meta/oblisk.lua` from the types that actually cross the socket.
//!
//! The stubs were hand-written first, and hand-writing them was wrong twice in one session: the
//! `process.run` callback arity, and `oblisk.notifications`'s command list, which claimed seven
//! commands where `dispatch` has four (`low`/`normal`/`critical` are urgency arms inside
//! `parse_set_sound_args`, not actions). Both were the same failure. A second copy of a schema,
//! maintained by hand, describes what someone believed rather than what runs.
//!
//! So the payload half is derived. Every capability's `*State` already derives `Serialize`, which
//! is what `push_snapshot` calls to build the `StateSnapshot`; adding `JsonSchema` beside it makes
//! the same type describe itself. Rust doc comments become LuaCATS descriptions, so the prose lives
//! next to the field it documents instead of in a parallel file that goes stale.
//!
//! ponytail: [`CAPABILITY_COMMANDS`] is still hand-written, because a command list lives in
//! `match` arms rather than in a type. It cannot drift silently -- `commands_match_dispatch`
//! parses those arms out of the source and compares -- but the check is a source grep, and the
//! upgrade path is a `#[derive(Deserialize, JsonSchema)]` action enum per capability, parsed at
//! the socket boundary so `dispatch` matches exhaustively and this table disappears.

use std::collections::BTreeMap;

use schemars::{Schema, schema_for};

/// The one place a capability name is tied to the type it pushes. That mapping exists nowhere else
/// in the tree: `push_snapshot` takes `&impl Serialize`, so the type is inferred at each of the 17
/// call sites and no table names them together. `every_capability_has_a_schema` is what keeps this
/// honest against `shared::CAPABILITIES`.
fn capability_schemas() -> Vec<(&'static str, Schema)> {
    vec![
        ("applications", schema_for!(crate::applications::controller::ApplicationsState)),
        ("audio", schema_for!(crate::audio::mixer::AudioState)),
        ("battery", schema_for!(crate::hardware::battery::controller::BatteryState)),
        ("bluetooth", schema_for!(crate::dbus::bluetooth::BluetoothState)),
        ("brightness", schema_for!(crate::hardware::brightness::controller::BrightnessState)),
        ("keyboard", schema_for!(crate::hardware::keyboard::controller::KeyboardState)),
        ("lock", schema_for!(crate::lock::LockState)),
        ("mpris", schema_for!(crate::dbus::mpris::controller::MprisState)),
        ("network", schema_for!(crate::dbus::network::NetworkState)),
        ("notifications", schema_for!(crate::dbus::notifications::NotificationsState)),
        ("power", schema_for!(crate::dbus::power::controller::PowerState)),
        ("privacy", schema_for!(crate::privacy::controller::PrivacyState)),
        ("sysinfo", schema_for!(crate::hardware::sysinfo::controller::SysinfoState)),
        ("system", schema_for!(crate::system::controller::SystemState)),
        ("tray", schema_for!(crate::dbus::tray::TrayState)),
        ("updates", schema_for!(crate::updates::controller::UpdatesState)),
        ("workspaces", schema_for!(crate::workspaces::controller::WorkspacesState)),
    ]
}

/// Every `capability:invoke(name, ...)` a `dispatch` accepts, in the order its `match` writes them.
/// A capability with no commands is read-only and gets a plain `invoke` inherited from `Capability`.
///
/// Kept in dispatch order rather than sorted, so a diff against the source reads straight down.
const CAPABILITY_COMMANDS: &[(&str, &[&str])] = &[
    ("applications", &["refresh", "launch"]),
    (
        "audio",
        &[
            "set_volume",
            "set_muted",
            "toggle_mute",
            "set_default_sink",
            "set_default_source",
            "set_app_volume",
            "set_app_muted",
        ],
    ),
    ("battery", &[]),
    ("bluetooth", &["set_enabled", "start_discovery", "stop_discovery", "connect", "disconnect", "pair", "forget"]),
    ("brightness", &["set"]),
    ("keyboard", &["set_backlight", "switch_layout"]),
    ("lock", &["lock"]),
    ("mpris", &["control", "seek", "seek_relative"]),
    ("network", &["scan", "connect", "forget", "set_wifi_enabled", "set_ethernet_enabled", "set_networking_enabled"]),
    ("notifications", &["dismiss", "reply", "set_sound", "set_dnd"]),
    ("power", &["set_profile"]),
    ("privacy", &[]),
    ("sysinfo", &["configure"]),
    ("system", &[]),
    ("tray", &["activate", "menu_will_show", "activate_menu_item"]),
    ("updates", &["configure", "install"]),
    ("workspaces", &["focus"]),
];

/// The Lua class name for a capability's payload, e.g. `audio` -> `AudioState`. Taken from the
/// schema's own `title`, which schemars fills in with the Rust type name, so a renamed struct
/// renames the Lua class without a second edit here.
fn payload_class(schema: &Schema) -> String {
    schema.get("title").and_then(|t| t.as_str()).unwrap_or("table").to_string()
}

/// `audio` -> `AudioCapability`.
fn capability_class(capability: &str) -> String {
    let mut out = String::new();
    for part in capability.split('_') {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out.push_str("Capability");
    out
}

/// A schema's `enum` as a LuaCATS string union, or `None` if it has none.
fn string_enum(fragment: &serde_json::Value) -> Option<String> {
    let variants = fragment.get("enum")?.as_array()?;
    let names: Vec<String> = variants.iter().filter_map(|v| v.as_str()).map(|v| format!("\"{v}\"")).collect();
    if names.is_empty() { None } else { Some(names.join("|")) }
}

/// A JSON Schema fragment as a LuaCATS type expression.
///
/// The five shapes that appear in these payloads, and nothing else: a `$ref` into `$defs`, an
/// array, a `Vec`-free map (`BTreeMap<String, T>` serializes as an object with
/// `additionalProperties`), a string enum, and a scalar. A fragment with no `type` at all is
/// `serde_json::Value`, which is `any`.
fn lua_type(fragment: &serde_json::Value) -> String {
    if let Some(reference) = fragment.get("$ref").and_then(|r| r.as_str()) {
        return reference.rsplit('/').next().unwrap_or("table").to_string();
    }
    if let Some(union) = string_enum(fragment) {
        return union;
    }
    // `Option<SomeStruct>` widens to `anyOf: [{$ref}, {"type": "null"}]` rather than to a `type`
    // array, because a `$ref` has no `type` to widen. Missing this typed every optional struct
    // field as `any`, which is how `WorkspacesState.active_client` lost its `ActiveClient`.
    if let Some(branches) = fragment.get("anyOf").and_then(|a| a.as_array())
        && let Some(concrete) = branches.iter().find(|b| b.get("type").and_then(|t| t.as_str()) != Some("null"))
    {
        return lua_type(concrete);
    }
    // `Option<T>` widens the type to `["T", "null"]` as well as dropping out of `required`.
    let type_name = match fragment.get("type") {
        Some(serde_json::Value::String(name)) => name.clone(),
        Some(serde_json::Value::Array(names)) => {
            names.iter().filter_map(|n| n.as_str()).find(|n| *n != "null").unwrap_or("any").to_string()
        }
        _ => return "any".to_string(),
    };
    match type_name.as_str() {
        "string" => "string".to_string(),
        "integer" => "integer".to_string(),
        "number" => "number".to_string(),
        "boolean" => "boolean".to_string(),
        "array" => {
            let item = fragment.get("items").map_or_else(|| "any".to_string(), lua_type);
            format!("{item}[]")
        }
        "object" => match fragment.get("additionalProperties") {
            Some(value) => format!("table<string, {}>", lua_type(value)),
            None => "table".to_string(),
        },
        _ => "any".to_string(),
    }
}

/// A Rust doc comment as one line of LuaCATS trailing description. Newlines collapse to spaces,
/// because `---@field name type description` is a single-line form and a wrapped description would
/// end the annotation early.
fn one_line(description: Option<&serde_json::Value>) -> String {
    match description.and_then(|d| d.as_str()) {
        Some(text) => {
            let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
            format!(" {joined}")
        }
        None => String::new(),
    }
}

/// Renders one object schema as a `---@class` with a `---@field` per property.
///
/// `oneOf` is the tagged-enum case, and it flattens. `NotificationSpan` is
/// `#[serde(tag = "kind")]`, so a value is one variant's fields plus a `kind` discriminating them.
/// LuaCATS has no tagged union, and the honest Lua shape is one class carrying every variant's
/// fields as optional, which is how a config reads it anyway: check `kind`, then use the fields
/// that variant carries.
fn render_class(name: &str, body: &serde_json::Value, out: &mut String) {
    // A fieldless enum (`Urgency` is `low`/`normal`/`critical`) is a set of strings, not an object.
    // Emitting it as a `---@class` gives it no fields, which types every reader of it as an empty
    // table and silently loses the three values that are the whole point.
    if body.get("properties").is_none()
        && body.get("oneOf").is_none()
        && let Some(union) = string_enum(body)
    {
        out.push_str(&format!("\n---@alias {name} {union}\n"));
        if let Some(description) = body.get("description").and_then(|d| d.as_str()) {
            for line in description.lines() {
                out.push_str(&format!("---{line}\n"));
            }
        }
        return;
    }
    out.push_str(&format!("\n---@class {name}\n"));
    if let Some(description) = body.get("description").and_then(|d| d.as_str()) {
        for line in description.lines() {
            out.push_str(&format!("---{}\n", if line.is_empty() { "" } else { line }));
        }
    }

    let mut fields: BTreeMap<String, (String, bool, String)> = BTreeMap::new();
    let mut variants: Vec<&serde_json::Value> = Vec::new();
    if let Some(one_of) = body.get("oneOf").and_then(|o| o.as_array()) {
        variants.extend(one_of.iter());
    } else {
        variants.push(body);
    }
    let tagged = variants.len() > 1;

    for variant in &variants {
        let required: Vec<&str> = variant
            .get("required")
            .and_then(|r| r.as_array())
            .map_or_else(Vec::new, |r| r.iter().filter_map(|v| v.as_str()).collect());
        let Some(properties) = variant.get("properties").and_then(|p| p.as_object()) else {
            continue;
        };
        for (field, fragment) in properties {
            // A flattened variant's fields are optional even where that variant requires them,
            // because a value of another variant does not carry them at all. The discriminator is
            // the exception: every variant has it.
            let is_discriminator = fragment.get("const").is_some() || (tagged && field == "kind");
            let optional = !required.contains(&field.as_str()) || (tagged && !is_discriminator);
            let type_name = if is_discriminator && tagged {
                variants
                    .iter()
                    .filter_map(|v| v.get("properties")?.get(field)?.get("const")?.as_str())
                    .map(|v| format!("\"{v}\""))
                    .collect::<Vec<_>>()
                    .join("|")
            } else {
                lua_type(fragment)
            };
            let description = one_line(fragment.get("description"));
            fields.entry(field.clone()).or_insert((type_name, optional, description));
        }
    }

    for (field, (type_name, optional, description)) in fields {
        let marker = if optional { "?" } else { "" };
        out.push_str(&format!("---@field {field}{marker} {type_name}{description}\n"));
    }
}

/// The whole generated file.
pub fn render() -> String {
    let mut out = String::new();
    out.push_str(&GENERATED_HEADER.replace("{VERSION}", env!("CARGO_PKG_VERSION")));

    let schemas = capability_schemas();

    // Every `$defs` entry across every capability, deduplicated by name and sorted, so the output
    // is stable whatever order the roster is in.
    let mut defs: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for (_, schema) in &schemas {
        let value = serde_json::to_value(schema).expect("a schema serializes");
        if let Some(entries) = value.get("$defs").and_then(|d| d.as_object()) {
            for (name, body) in entries {
                defs.insert(name.clone(), body.clone());
            }
        }
    }

    out.push_str(
        "\n--- Payload types ------------------------------------------------------------------------------\n",
    );
    for (name, body) in &defs {
        render_class(name, body, &mut out);
    }
    for (_, schema) in &schemas {
        let value = serde_json::to_value(schema).expect("a schema serializes");
        render_class(&payload_class(schema), &value, &mut out);
    }

    out.push_str(
        "\n--- Capabilities -------------------------------------------------------------------------------\n",
    );
    for (capability, schema) in &schemas {
        let class = capability_class(capability);
        let payload = payload_class(schema);
        let commands = CAPABILITY_COMMANDS.iter().find(|(name, _)| name == capability).map_or(&[][..], |(_, c)| *c);
        out.push_str(&format!("\n---@class {class}: Capability\nlocal {class} = {{}}\n"));
        out.push_str(&format!("---@return {payload}\nfunction {class}:get() end\n"));
        out.push_str(&format!(
            "---@param fn fun(value: {payload}): any\n---@return Signal\nfunction {class}:map(fn) end\n"
        ));
        if !commands.is_empty() {
            let union = commands.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join("|");
            out.push_str(&format!(
                "---@param command {union}\n---@param ... any\nfunction {class}:invoke(command, ...) end\n"
            ));
        }
    }

    out.push_str(RENDERER_SOURCED);

    out.push_str("\n---@class Oblisk\n");
    for capability in shared::CAPABILITIES {
        out.push_str(&format!("---@field {capability} {}\n", capability_class(capability)));
    }
    out.push_str(OBLISK_TAIL);
    out
}

const GENERATED_HEADER: &str = r#"---@meta
-- GENERATED by `supervisor/src/stubs.rs` for oblisk {VERSION}. Do not edit: run
-- `UPDATE_STUBS=1 cargo test -p supervisor stubs` and commit what changes.
--
-- The version above is what `stub_version` reads. Stubs describing a different engine than the one
-- installed are worse than no stubs, because they are wrong with authority, so `oblisk init` says
-- so and refreshes them.
--
-- The capability classes below are the supervisor's own `Serialize` types, which is what
-- `push_snapshot` sends, so a field here is a field that crosses the socket. Descriptions are the
-- Rust doc comments on those fields. To document a field for config authors, write the doc comment
-- in Rust and regenerate.
--
-- A capability reads `nil` until its first `StateSnapshot` arrives, which every config sees on
-- every boot. A signal resolving to `nil` means the property is absent, so a bound node renders its
-- default rather than failing the tree (ADR-0044). A JSON `null` arrives as an absent key rather
-- than a sentinel (ADR-0057), so `if item.icon_path then` is the right guard for an optional field.

---@class Capability: Signal
---A capability is a signal you can also command. `:get()` and `:map()` read the pushed payload;
---`:invoke()` sends a command the supervisor dispatches. Read-only otherwise: `:set()` refuses it,
---or a config could overwrite the SSID the supervisor just pushed.
local Capability = {}

---@param command string
---@param ... any
function Capability:invoke(command, ...) end
"#;

const RENDERER_SOURCED: &str = r#"
--- Renderer-sourced members ---------------------------------------------------------------------
-- Not capabilities and not in `shared::CAPABILITIES`: these come from the renderer's own state, so
-- they carry no commands and are written by hand rather than derived from a payload struct.

---@class Screen
---@field name string Matches a surface's `monitor`.
---@field width integer Logical pixels.
---@field height integer
---@field scale number
---@field refresh number Hz. `0` for an output with no current mode, such as a virtual one.

---@class RescueState
---@field is_rescue boolean
---@field error_log string

---@class ObliskVersion
---@field major integer
---@field minor integer
---@field patch integer
"#;

const OBLISK_TAIL: &str = r#"---@field screens Signal A `Screen[]`. Renderer-sourced, seeded to an empty list, and the one signal with a value at first evaluation (ADR-0041).
---@field rescue Signal A `RescueState`. Renderer-sourced, no commands (ADR-0046).
---@field version ObliskVersion Three integers a config can compare. Not a signal.
---@field config_dir string The directory `shell.lua` was loaded from, so a config can name a file it ships beside itself. Not a signal.
oblisk = {}
"#;

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    fn stub_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../lua-meta/oblisk.lua")
    }

    /// The golden-file check. `UPDATE_STUBS=1 cargo test -p supervisor stubs` rewrites the file;
    /// with the variable unset, a difference fails.
    ///
    /// Checked in rather than built on demand because the language server reads it directly from
    /// the working tree, with no build step between an editor opening and a completion appearing.
    #[test]
    fn the_generated_stub_matches_what_is_checked_in() {
        let rendered = super::render();
        let path = stub_path();
        if std::env::var_os("UPDATE_STUBS").is_some() {
            std::fs::write(&path, &rendered).expect("the stub file is writable");
            return;
        }
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(
            current, rendered,
            "lua-meta/oblisk.lua is stale. Rerun with UPDATE_STUBS=1 and commit the result. A version bump \
             alone does this, because the version is stamped into the file."
        );
    }

    /// Every roster name has a payload type, and no entry here names a capability that is gone.
    /// The stamp has to survive the round trip, or the mismatch warnings compare against `None`
    /// forever and never fire.
    #[test]
    fn the_generated_stub_stamps_a_version_that_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("oblisk.lua"), super::render()).unwrap();
        assert_eq!(crate::setup::stub_version(dir.path()).as_deref(), Some(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn a_directory_with_no_stubs_has_no_version() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(crate::setup::stub_version(dir.path()), None);
    }

    #[test]
    fn every_capability_has_a_schema() {
        let declared: BTreeSet<&str> = super::capability_schemas().into_iter().map(|(name, _)| name).collect();
        let expected: BTreeSet<&str> = shared::CAPABILITIES.iter().copied().collect();
        assert_eq!(declared, expected, "capability_schemas is out of step with shared::CAPABILITIES");
    }

    #[test]
    fn every_capability_has_a_command_list() {
        let declared: BTreeSet<&str> = super::CAPABILITY_COMMANDS.iter().map(|(name, _)| *name).collect();
        let expected: BTreeSet<&str> = shared::CAPABILITIES.iter().copied().collect();
        assert_eq!(declared, expected, "CAPABILITY_COMMANDS is out of step with shared::CAPABILITIES");
    }

    /// Reads the actions out of every `match params.action.as_str()` in the crate and compares them
    /// against [`super::CAPABILITY_COMMANDS`].
    ///
    /// This is the check that would have caught `notifications` claiming seven commands where
    /// `dispatch` has four. It is a source grep, and it is scoped to exactly one `match` expression
    /// so it cannot pick up an unrelated string arm the way a bare `grep '"x" =>'` did: brace depth
    /// is tracked from the `match` and only depth-1 arms count.
    #[test]
    fn commands_match_dispatch() {
        let mut found: std::collections::BTreeMap<String, BTreeSet<String>> = std::collections::BTreeMap::new();
        for source in rust_sources(Path::new(env!("CARGO_MANIFEST_DIR")).join("src")) {
            let text = std::fs::read_to_string(&source).expect("a source file this build compiled is readable");
            for actions in action_matches(&text) {
                // The capability a dispatch belongs to is its module path, which for every one of
                // these is the directory or file name: `dbus/network/mod.rs` -> `network`.
                let name = capability_of(&source);
                if let Some(name) = name {
                    found.entry(name).or_default().extend(actions);
                }
            }
        }
        for (capability, commands) in super::CAPABILITY_COMMANDS {
            let expected: BTreeSet<String> = commands.iter().map(|c| (*c).to_string()).collect();
            let actual = found.get(*capability).cloned().unwrap_or_default();
            assert_eq!(actual, expected, "{capability}'s dispatch arms and CAPABILITY_COMMANDS disagree");
        }
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
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        out
    }

    /// The `"name" =>` arms at depth 1 of every `match params.action.as_str()` block in `text`.
    fn action_matches(text: &str) -> Vec<Vec<String>> {
        let mut blocks = Vec::new();
        for (start, _) in text.match_indices("action.as_str() {") {
            let body = &text[start..];
            let mut depth = 0usize;
            let mut arms = Vec::new();
            for (i, c) in body.char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    '"' if depth == 1 => {
                        let rest = &body[i + 1..];
                        if let Some(end) = rest.find('"') {
                            let name = &rest[..end];
                            // Only a string that is the head of an arm, i.e. followed by `=>`.
                            let after = rest[end + 1..].trim_start();
                            if after.starts_with("=>") && name.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                                arms.push(name.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
            blocks.push(arms);
        }
        blocks
    }

    /// `src/dbus/network/mod.rs` -> `network`; `src/lock.rs` -> `lock`.
    fn capability_of(path: &Path) -> Option<String> {
        let stem = path.file_stem()?.to_str()?;
        let name = if stem == "mod" || stem == "controller" || stem == "registry" {
            path.parent()?.file_name()?.to_str()?
        } else {
            stem
        };
        shared::CAPABILITIES.iter().find(|c| **c == name).map(|c| (*c).to_string())
    }
}
