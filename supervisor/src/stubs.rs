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
//! The command half is derived the same way. Each capability's actions are a
//! `#[derive(Deserialize, JsonSchema)]` enum next to its `dispatch`, and `parse_action` at the
//! socket boundary is what turns the wire string into one. The names Lua may pass to `invoke` are
//! that enum's variants, so an action with no dispatch arm, or an arm with no action, fails the
//! build rather than the golden test.
//!
//! ponytail: argument types stay `...`. The next step is a payload enum (`Set(u32)`) carrying its
//! parsed arguments, which would delete all 19 `parse_*_args` functions, but
//! `CommandParams.arguments` is a positional array and serde reads a single-field variant as a
//! newtype, which wants the bare value rather than a one-element array. Typed arguments cost an
//! IDL change to named arguments (docs/oblisk-idl-api-specs.md § 7.2), not a derive.

use std::collections::BTreeMap;

use schemars::{Schema, schema_for};

/// The one place a capability name is tied to the type it pushes and to the enum of actions it
/// accepts. Neither mapping exists anywhere else in the tree: `push_snapshot` takes
/// `&impl Serialize`, so the payload type is inferred at each of the 17 call sites, and an action
/// enum is named only by its own `dispatch`. `every_capability_has_a_schema` keeps this honest
/// against `shared::Capability::ALL`. `None` is a read-only capability, which gets the plain `invoke`
/// it inherits from `Capability`.
fn capability_schemas() -> Vec<(&'static str, Schema, Option<Schema>)> {
    vec![
        (
            "applications",
            schema_for!(crate::capabilities::applications::controller::ApplicationsState),
            Some(schema_for!(crate::capabilities::applications::ApplicationsAction)),
        ),
        (
            "audio",
            schema_for!(crate::capabilities::audio::mixer::AudioState),
            Some(schema_for!(crate::capabilities::audio::AudioAction)),
        ),
        ("battery", schema_for!(crate::capabilities::battery::controller::BatteryState), None),
        (
            "bluetooth",
            schema_for!(crate::capabilities::bluetooth::BluetoothState),
            Some(schema_for!(crate::capabilities::bluetooth::BluetoothAction)),
        ),
        (
            "brightness",
            schema_for!(crate::capabilities::brightness::controller::BrightnessState),
            Some(schema_for!(crate::capabilities::brightness::BrightnessAction)),
        ),
        (
            "keyboard",
            schema_for!(crate::capabilities::keyboard::controller::KeyboardState),
            Some(schema_for!(crate::capabilities::keyboard::KeyboardAction)),
        ),
        (
            "lock",
            schema_for!(crate::capabilities::lock::LockState),
            Some(schema_for!(crate::capabilities::lock::LockAction)),
        ),
        (
            "mpris",
            schema_for!(crate::capabilities::mpris::controller::MprisState),
            Some(schema_for!(crate::capabilities::mpris::MprisAction)),
        ),
        (
            "network",
            schema_for!(crate::capabilities::network::NetworkState),
            Some(schema_for!(crate::capabilities::network::NetworkAction)),
        ),
        (
            "notifications",
            schema_for!(crate::capabilities::notifications::NotificationsState),
            Some(schema_for!(crate::capabilities::notifications::NotificationsAction)),
        ),
        (
            "power",
            schema_for!(crate::capabilities::power::controller::PowerState),
            Some(schema_for!(crate::capabilities::power::PowerAction)),
        ),
        ("privacy", schema_for!(crate::capabilities::privacy::controller::PrivacyState), None),
        (
            "sysinfo",
            schema_for!(crate::capabilities::sysinfo::controller::SysinfoState),
            Some(schema_for!(crate::capabilities::sysinfo::SysinfoAction)),
        ),
        ("system", schema_for!(crate::capabilities::system::controller::SystemState), None),
        (
            "tray",
            schema_for!(crate::capabilities::tray::TrayState),
            Some(schema_for!(crate::capabilities::tray::TrayAction)),
        ),
        (
            "updates",
            schema_for!(crate::capabilities::updates::controller::UpdatesState),
            Some(schema_for!(crate::capabilities::updates::UpdatesAction)),
        ),
        (
            "workspaces",
            schema_for!(crate::capabilities::workspaces::controller::WorkspacesState),
            Some(schema_for!(crate::capabilities::workspaces::WorkspacesAction)),
        ),
    ]
}

/// The Lua class name for a capability's payload, e.g. `audio` -> `AudioState`. Taken from the
/// schema's own `title`, which schemars fills in with the Rust type name, so a renamed struct
/// renames the Lua class without a second edit here.
fn payload_class(schema: &Schema) -> String {
    schema.get("title").and_then(|t| t.as_str()).unwrap_or("table").to_string()
}

/// An action enum's variants as the wire strings a config passes to `invoke`, in declaration
/// order. schemars renders a fieldless enum as a bare `enum` array of its `rename_all` spellings,
/// which is exactly the list `parse_action` accepts.
fn action_names(schema: &Schema) -> Vec<String> {
    let value = serde_json::to_value(schema).expect("a schema serializes");
    value
        .get("enum")
        .and_then(|e| e.as_array())
        .map_or_else(Vec::new, |v| v.iter().filter_map(|n| n.as_str()).map(str::to_string).collect())
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
    for (_, schema, _) in &schemas {
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
    for (_, schema, _) in &schemas {
        let value = serde_json::to_value(schema).expect("a schema serializes");
        render_class(&payload_class(schema), &value, &mut out);
    }

    out.push_str(
        "\n--- Capabilities -------------------------------------------------------------------------------\n",
    );
    for (capability, schema, actions) in &schemas {
        let class = capability_class(capability);
        let payload = payload_class(schema);
        let commands = actions.as_ref().map_or_else(Vec::new, action_names);
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
    for capability in shared::Capability::ALL {
        let name = capability.as_str();
        out.push_str(&format!("---@field {name} {}\n", capability_class(name)));
    }
    out.push_str(OBLISK_TAIL);
    out
}

const GENERATED_HEADER: &str = r#"---@meta
-- GENERATED by `supervisor/src/stubs.rs` for oblisk {VERSION}. Do not edit: run `just stubs` and
-- commit what changes.
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
--- Off-roster members ---------------------------------------------------------------------------
-- Not capabilities and not in `shared::Capability::ALL`, so they have no payload struct to derive
-- from and are written by hand. `Screen` and `RescueState` come from the renderer's own state.
-- `Idle` is the other direction: a supervisor service that pushes no state at all, because an idle
-- threshold crossing is an event, not something to read (ADR-0032).

---@class Idle
local Idle = {}

---Runs `on_idle` after `seconds` without input on the seat, and `on_resume` when input returns.
---Registrations do not survive a config reload, which re-runs `shell.lua` and drops them, so
---register at the top level rather than inside a callback that fires more than once.
---@param seconds integer
---@param on_idle fun()
---@param on_resume fun()
function Idle:register_threshold(seconds, on_idle, on_resume) end

---Holds off idle actions system-wide (logind `Inhibit`, `what="idle"`) until a matching
---`release_inhibit`. Counted, so two holders need two releases and neither cancels the other.
---@param reason string Shown by `loginctl list-inhibitors`.
function Idle:inhibit(reason) end

---Releases one `inhibit` hold.
function Idle:release_inhibit() end

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

const OBLISK_TAIL: &str = r#"---@field idle Idle Idle thresholds and the inhibit pair. Methods only, no state to read (ADR-0032).
---@field screens Signal A `Screen[]`. Renderer-sourced, seeded to an empty list, and the one signal with a value at first evaluation (ADR-0041).
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

    /// The golden-file check. `just stubs` rewrites the file; with `UPDATE_STUBS` unset, a
    /// difference fails.
    ///
    /// This is the whole automation, and it is deliberately a test rather than a build step.
    /// `render` reads `schemars::JsonSchema` derives on types inside this crate, so a `build.rs`
    /// could not call it: a build script compiles and runs before the crate it belongs to exists.
    /// Even if it could, a build that writes into the source tree breaks a read-only checkout and
    /// dirties the working tree on every `cargo build`.
    ///
    /// Checked in rather than built on demand because the language server reads it directly from
    /// the working tree, with no build step between an editor opening and a completion appearing.
    /// A fresh clone has working stubs before anything is compiled.
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
            "lua-meta/oblisk.lua is stale. Run `just stubs` and commit the result. A version bump alone \
             does this, because the version is stamped into the file."
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
        let declared: BTreeSet<&str> = super::capability_schemas().into_iter().map(|(name, ..)| name).collect();
        let expected: BTreeSet<&str> = shared::Capability::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(declared, expected, "capability_schemas is out of step with shared::Capability::ALL");
    }
}
