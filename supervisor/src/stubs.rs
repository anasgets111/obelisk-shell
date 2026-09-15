//! Generates `lua-meta/obelisk.lua` from the types that actually cross the socket.
//!
//! Hand-written stubs were wrong twice in one session: `process.run` callback arity, and
//! `obelisk.notifications` claiming seven commands while `dispatch` has four (`low`/`normal`/
//! `critical` are urgency arms). A copied schema describes belief, not running code.
//!
//! Payloads are derived: each `*State` already has `Serialize`; adding `JsonSchema` makes each
//! field's Rust doc comment its LuaCATS description.
//!
//! Commands work the same way: `#[derive(Deserialize, JsonSchema)]` action enums sit beside
//! `dispatch`, socket-boundary `parse_action` decodes `(action, arguments)` into their variants,
//! and each variant's fields become one typed `invoke` overload. Mismatches fail the build rather
//! than the golden test.

use std::collections::BTreeMap;

use schemars::{Schema, schema_for};

/// The sole capability-to-payload/action mapping. `push_snapshot` takes `&impl Serialize`, so
/// payload types are inferred at each call site; action enums are named only by their `dispatch`.
/// `every_capability_has_a_schema` checks `shared::Capability::ALL`. `None` means no `invoke`.
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
        ("idle", schema_for!(crate::capabilities::idle::IdleState), None),
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
            "files",
            schema_for!(crate::capabilities::files::controller::FilesState),
            Some(schema_for!(crate::capabilities::files::FilesAction)),
        ),
        (
            "processes",
            schema_for!(crate::capabilities::processes::controller::ProcessesState),
            Some(schema_for!(crate::capabilities::processes::ProcessesAction)),
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
            "storage",
            schema_for!(crate::capabilities::storage::controller::StorageState),
            Some(schema_for!(crate::capabilities::storage::StorageAction)),
        ),
        (
            "polkit",
            schema_for!(crate::capabilities::polkit::PolkitState),
            Some(schema_for!(crate::capabilities::polkit::PolkitAction)),
        ),
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

/// Lua payload class name, e.g. `audio` -> `AudioState`, taken from schemars' `title` so renaming
/// the Rust struct renames Lua without another edit.
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

/// A schema's `enum` as its string variants, or `None` if it has none.
fn enum_strings(fragment: &serde_json::Value) -> Option<Vec<&str>> {
    let variants = fragment.get("enum")?.as_array()?;
    let names: Vec<&str> = variants.iter().filter_map(|v| v.as_str()).collect();
    (!names.is_empty()).then_some(names)
}

/// A schema's `enum` as a LuaCATS string union, or `None` if it has none.
fn string_enum(fragment: &serde_json::Value) -> Option<String> {
    Some(enum_strings(fragment)?.into_iter().map(|v| format!("\"{v}\"")).collect::<Vec<_>>().join("|"))
}

/// Fieldless enum form with one `oneOf`/`const` branch per documented variant. schemars switches to
/// it when a variant has a doc comment; `BatteryStatus` first needed it because `PendingCharge` and
/// `PendingDischarge` do not explain themselves.
///
/// Returns each variant with its description; preserving those is the point of documenting them.
fn const_enum(body: &serde_json::Value) -> Option<Vec<(&str, Option<&str>)>> {
    let branches = body.get("oneOf")?.as_array()?;
    let mut variants: Vec<(&str, Option<&str>)> = Vec::new();
    for branch in branches {
        let description = branch.get("description").and_then(|d| d.as_str());
        // schemars uses a `const` branch for documented variants and pools undocumented ones in an
        // `enum` branch; both are strings belonging in the alias.
        if let Some(name) = branch.get("const").and_then(|c| c.as_str()) {
            variants.push((name, description));
            continue;
        }
        // Neither form is an object branch, so this is a tagged union rendered as a class.
        let pooled = branch.get("enum").and_then(|e| e.as_array())?;
        for name in pooled.iter().filter_map(|v| v.as_str()) {
            variants.push((name, description));
        }
    }
    (!variants.is_empty()).then_some(variants)
}

/// JSON Schema fragment as a LuaCATS type expression.
///
/// Only five payload shapes occur: `$defs` `$ref`, array, `BTreeMap<String, T>` object with
/// `additionalProperties`, string enum, and scalar. No `type` means `serde_json::Value`, or `any`.
fn lua_type(fragment: &serde_json::Value) -> String {
    if let Some(reference) = fragment.get("$ref").and_then(|r| r.as_str()) {
        return reference.rsplit('/').next().unwrap_or("table").to_string();
    }
    if let Some(union) = string_enum(fragment) {
        return union;
    }
    // `Option<SomeStruct>` uses `anyOf: [{$ref}, {"type": "null"}]`, not a type array: `$ref` has
    // no type to widen. Missing this made optional structs `any`, losing `ActiveClient` from
    // `WorkspacesState.active_client`.
    if let Some(branches) = fragment.get("anyOf").and_then(|a| a.as_array())
        && let Some(concrete) = branches.iter().find(|b| b.get("type").and_then(|t| t.as_str()) != Some("null"))
    {
        return lua_type(concrete);
    }
    // `Option<T>` also widens to `["T", "null"]` and removes the field from `required`.
    let type_name = match fragment.get("type") {
        Some(serde_json::Value::String(name)) => name.clone(),
        Some(serde_json::Value::Array(names)) => {
            names.iter().filter_map(|n| n.as_str()).find(|n| *n != "null").unwrap_or("any").to_string()
        }
        _ => return "any".to_string(),
    };
    match type_name.as_str() {
        "string" | "integer" | "number" | "boolean" => type_name.to_string(),
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

/// Rust doc comment as one LuaCATS trailing description. Collapse newlines because
/// `---@field name type description` is single-line and wrapping ends the annotation.
fn one_line(description: Option<&serde_json::Value>) -> String {
    match description.and_then(|d| d.as_str()) {
        Some(text) => {
            let joined = unlink(text).split_whitespace().collect::<Vec<_>>().join(" ");
            format!(" {joined}")
        }
        None => String::new(),
    }
}

/// Rustdoc intra-links as plain code spans; LuaLS would print the brackets.
fn unlink(text: &str) -> String {
    regex_lite::Regex::new(r"\[(`[^`]+`)\](\([^)]*\))?").expect("a valid pattern").replace_all(text, "$1").into_owned()
}

/// Renders an object schema as `---@class` plus one `---@field` per property.
///
/// A schema's own `description`, one `---` line each. One of the three callers guarded the empty
/// line with `if line.is_empty() { "" } else { line }`, which is the identity.
fn append_description(body: &serde_json::Value, out: &mut String) {
    if let Some(description) = body.get("description").and_then(|d| d.as_str()) {
        for line in unlink(description).lines() {
            out.push_str(&format!("---{line}\n"));
        }
    }
}

/// `oneOf` tagged enums flatten. `NotificationSpan` is `#[serde(tag = "kind")]`: one variant's
/// fields plus `kind`. LuaCATS lacks tagged unions, so emit one class with all variant fields
/// optional; configs check `kind` before using them.
fn render_class(name: &str, body: &serde_json::Value, out: &mut String) {
    // Fieldless enums such as `Urgency` (`low`/`normal`/`critical`) are strings, not objects.
    // A fieldless `---@class` would type readers as empty tables and lose all three values.
    if body.get("properties").is_none()
        && body.get("oneOf").is_none()
        && let Some(union) = string_enum(body)
    {
        out.push_str(&format!("\n---@alias {name} {union}\n"));
        append_description(body, out);
        return;
    }
    // Documented enum form: one line per variant; the flat union has nowhere for descriptions.
    if body.get("properties").is_none()
        && let Some(variants) = const_enum(body)
    {
        out.push_str(&format!("\n---@alias {name}\n"));
        for (variant, description) in variants {
            match description.and_then(|d| d.lines().next()) {
                Some(first) => out.push_str(&format!("---| \"{variant}\" # {}\n", unlink(first))),
                None => out.push_str(&format!("---| \"{variant}\"\n")),
            }
        }
        append_description(body, out);
        return;
    }
    out.push_str(&format!("\n---@class {name}\n"));
    append_description(body, out);

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
            // Flattened variant fields are optional because other variants omit them; the
            // discriminator is the exception because every variant has it.
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

/// One `---@field invoke` per action, which LuaLS reads as overloads. External tagging makes a
/// fieldless action a string branch and any other a one-key object holding its fields.
///
/// ponytail: `properties` come back alphabetical, so arguments follow `required` order, then at
/// most one optional; a second needs serde's field order.
fn render_invoke(class: &str, actions: &serde_json::Value, out: &mut String) {
    append_description(actions, out);
    let branches =
        actions.get("oneOf").and_then(|o| o.as_array()).map_or_else(|| vec![actions], |b| b.iter().collect());
    for branch in branches {
        let description = one_line(branch.get("description"));
        if let Some(names) = enum_strings(branch).or_else(|| Some(vec![branch.get("const")?.as_str()?])) {
            for name in names {
                out.push_str(&format!("---@field invoke fun(self: {class}, command: \"{name}\"){description}\n"));
            }
            continue;
        }
        let (name, fields) = branch["properties"].as_object().and_then(|p| p.iter().next()).expect("a one-key object");
        let properties = fields["properties"].as_object().expect("a struct variant");
        let required: Vec<&str> = fields
            .get("required")
            .and_then(|r| r.as_array())
            .map_or_else(Vec::new, |r| r.iter().filter_map(|v| v.as_str()).collect());
        let mut params = format!("self: {class}, command: \"{name}\"");
        for field in &required {
            params.push_str(&format!(", {field}: {}", lua_type(&properties[*field])));
        }
        let optional: Vec<_> = properties.iter().filter(|(field, _)| !required.contains(&field.as_str())).collect();
        assert!(optional.len() <= 1, "{name} has {} optional arguments; see the ponytail above", optional.len());
        for (field, fragment) in optional {
            params.push_str(&format!(", {field}?: {}", lua_type(fragment)));
        }
        out.push_str(&format!("---@field invoke fun({params}){description}\n"));
    }
}

/// The generated file.
pub fn render() -> String {
    let mut out = String::new();
    out.push_str(&GENERATED_HEADER.replace("{VERSION}", env!("CARGO_PKG_VERSION")));

    let schemas = capability_schemas();

    // Deduplicate and sort every capability's `$defs`, making output independent of roster order.
    let mut defs: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for schema in schemas.iter().flat_map(|(_, payload, actions)| std::iter::once(payload).chain(actions)) {
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
        // Inherit `get`/`map`/`on_change`: repeating them would need a class-specific `self`, and
        // an unbound `---@field` would check nothing.
        let base = if actions.is_none() { "ReadOnlyCapability" } else { "Capability" };
        out.push_str(&format!("\n---@class {class}: {base}<{payload}>\n"));
        out.push_str(hand_written_methods(capability));
        match actions {
            Some(actions) => render_invoke(&class, actions.as_value(), &mut out),
            None => out.push_str(&format!("local {class} = {{}}\n")),
        }
    }

    out.push_str(RENDERER_SOURCED);

    out.push_str("\n---@class Obelisk\n");
    for capability in shared::Capability::ALL {
        let name = capability.as_str();
        out.push_str(&format!("---@field {name} {} {}\n", capability_class(name), capability.blurb()));
    }
    out.push_str(OBELISK_TAIL);
    out
}

const GENERATED_HEADER: &str = r#"---@meta
-- GENERATED by `supervisor/src/stubs.rs` for obelisk {VERSION}. Do not edit: run `just stubs` and
-- commit what changes.
--
-- The only generated file in `lua-meta`. `nodes.lua`, `surfaces.lua`, `globals.lua` and
-- `signals.lua` are hand-written and `just stubs` does not touch them; each says so at its own top
-- and names what checks it. This one is derivable because a capability payload is a real
-- `Serialize` struct, and a node's schema is scattered `properties.get("...")` calls.
--
-- `obelisk init` rewrites installed stubs that differ from its own copy. Stubs describing another
-- engine are worse than none, because they are wrong with authority.
--
-- The capability classes below are the supervisor's own `Serialize` types, which is what
-- `push_snapshot` sends, so a field here is a field that crosses the socket. Descriptions are the
-- Rust doc comments on those fields. To document a field for config authors, write the doc comment
-- in Rust and regenerate.
--
-- A capability reads `nil` until its first `StateSnapshot` arrives, which every config sees on
-- every boot. A signal resolving to `nil` means the property is absent, so a bound node renders its
-- default rather than failing the tree (ADR-0044). A JSON `null` arrives as an absent key rather
-- than a sentinel (ADR-0057), so `if item.app_icon then` is the right guard for an optional field.

---@class ReadOnlyCapability<T>: Signal<T>
---`:get()` and `:map()` read the pushed payload. `:set()` refuses it, or a config could overwrite
---the SSID the supervisor just pushed.
---
---Generic over the payload, and inherited as `Capability<NetworkState>` and so on below, which is
---what types the callback: the `n` in `obelisk.network:map(function(n) ... end)` is a `NetworkState`,
---so a misspelled field is an `undefined-field` here rather than a `nil` at runtime.
---
---`:on_change(handler)` is the one place a config reacts to a push instead of rendering it
---(ADR-0115): the handler runs once per pushed snapshot with the new payload and the one it
---replaced (`nil` on the first push), outside any layout pass, under a `map` callback's 5ms CPU
---budget. It may do what an input callback may do: `invoke`, `process.run`, write a `state`
---signal. Compare the two payloads to find the edge you want; the engine hands over every push.
---@field on_change fun(self: ReadOnlyCapability<T>, handler: fun(current: T, previous: T?))

---@class Capability<T>: ReadOnlyCapability<T>
"#;

/// Methods no action schema can describe, appended to the generated class. Only `idle` has them:
/// three Lua callbacks never cross the wire, so no `IdleAction` signature exists (ADR-0032,
/// ADR-0141). Its actions stay `None`: `register` without the local callbacks fires into nothing,
/// and `forget_thresholds` would drop the config's own thresholds (ADR-0158).
///
/// Use `---@field`, not `function IdleCapability:...`: a class with `---@field invoke` has no local
/// binding for a later function, so calls read `undefined-field`. The first version did this;
/// `just types` caught it.
///
/// Every emitted line starts at column zero. `lua-meta` is formatted by `just fmt`, which strips
/// leading whitespace from a comment line, so an indented one here made `just stubs` and
/// `just fmt-check` undo each other on every run.
fn hand_written_methods(capability: &str) -> &'static str {
    match capability {
        "idle" => {
            "---@field register_threshold fun(self: IdleCapability, seconds: integer, on_idle: fun(), on_resume: fun()) Runs `on_idle` after `seconds` without input on the seat, and `on_resume` when input returns. Registrations do not survive a config reload, so register at the top level rather than inside a callback that fires more than once.\n---@field inhibit fun(self: IdleCapability, reason: string) Holds off idle actions system-wide (logind `Inhibit`, `what=\"idle\"`) until a matching `release_inhibit`. Counted, so two holders need two releases. While any hold is out -- this one or another application's -- no threshold fires and `inhibited` says so.\n---@field release_inhibit fun(self: IdleCapability) Releases one `inhibit` hold. A release with no matching `inhibit` is a no-op.\n"
        }
        _ => "",
    }
}

const RENDERER_SOURCED: &str = r#"
--- Off-roster members ---------------------------------------------------------------------------
-- Not capabilities and not in `shared::Capability::ALL`, so they have no payload struct to derive
-- from and are written by hand: `Screen` and `RescueState` come from the renderer's own state.
--
-- `obelisk.idle` used to be here too. It joined the roster with ADR-0141, because there turned out
-- to be idle state worth reading after all -- whether anything is holding the session awake, and
-- which application it is. It is the one member that is both: `IdleCapability` above is generated
-- from `IdleState` like any other, and its three methods are declared onto it by hand,
-- because their callbacks are Lua values that never cross the wire and so have no action schema to
-- derive from.

---@class Screen
---@field name string The connector name, e.g. `"eDP-1"`. What a surface's `monitor` takes, and what an `obelisk.workspaces` output entry is keyed by.
---@field width integer Logical pixels, already divided by `scale`. Not the mode's pixel count.
---@field height integer Logical pixels, on the same terms as `width`.
---@field scale integer The compositor's integer scale factor for this output, `1` on an ordinary display and `2` on a HiDPI one. Not a divisor: `width` and `height` above are already logical. Read it to pick sizes.
---@field refresh number Hz. `0` for an output with no current mode, such as a virtual one.

---@class RescueState
---@field is_rescue boolean A reload failed and the scene from before the edit is still on screen. Covers reloads only: a config that fails its very first evaluation has no tree to render a banner through (ADR-0046).
---@field error_log string The Lua error that caused it, ready to draw. Empty while `is_rescue` is false.

---@class ObeliskVersion
---@field major integer Breaking IDL changes.
---@field minor integer The crate's minor version. Nothing bumps it for an IDL change, so `obelisk.version.minor >= n` gates on releases, not fields.
---@field patch integer Everything else. Never affects what a config may use.
"#;

const OBELISK_TAIL: &str = r#"---@field screens Signal<Screen[]> Renderer-sourced, seeded to an empty list, and the one signal with a value at first evaluation (ADR-0041).
---@field rescue Signal<RescueState> Renderer-sourced, no commands (ADR-0046).
---@field version ObeliskVersion Three integers a config can compare. Not a signal.
---@field config_dir string The directory `shell.lua` was loaded from, so a config can name a file it ships beside itself. Not a signal.
obelisk = {}
"#;

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    fn stub_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../lua-meta/obelisk.lua")
    }

    /// Golden-file check. `just stubs` rewrites; without `UPDATE_STUBS`, differences fail.
    ///
    /// Deliberately a test, not a build step: `render` reads derives here, but `build.rs`
    /// runs before that crate exists. Writing source during builds would break read-only checkouts
    /// and dirty the tree on every `cargo build`.
    ///
    /// Checked in because the language server reads the working tree with no build step between
    /// editor open and completion. A fresh clone has usable stubs before compilation.
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
            "lua-meta/obelisk.lua is stale. Run `just stubs` and commit the result. A version bump alone \
             does this, because the version is stamped into the file."
        );
    }

    /// The supervisor drops a read-only capability's commands, so `invoke` type-checks a no-op.
    #[test]
    fn read_only_classes_offer_no_invoke() {
        let generated = super::render();
        for (capability, schema, actions) in super::capability_schemas() {
            if actions.is_some() {
                continue;
            }
            let header = format!(
                "---@class {}: ReadOnlyCapability<{}>",
                super::capability_class(capability),
                super::payload_class(&schema)
            );
            let class: Vec<&str> = generated
                .lines()
                .skip_while(|line| *line != header)
                .take_while(|line| line.starts_with("---"))
                .collect();
            assert!(!class.is_empty(), "{header} is missing");
            assert!(!class.iter().any(|line| line.contains("invoke")), "{class:#?}");
        }
    }

    #[test]
    fn every_capability_has_a_schema() {
        let declared: BTreeSet<&str> = super::capability_schemas().into_iter().map(|(name, ..)| name).collect();
        let expected: BTreeSet<&str> = shared::Capability::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(declared, expected, "capability_schemas is out of step with shared::Capability::ALL");
    }
}
