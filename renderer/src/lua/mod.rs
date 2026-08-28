//! Lua VM bootstrap and the loader (build-steps.md Phase 10; `CONTEXT.md`, Loader): evaluates
//! `shell.lua` into the top-level `surface` node(s) and their topology, reused for both a
//! candidate's first evaluation and the authoritative generation's re-evaluation on an in-place
//! reload.
//!
//! [`Loader::evaluate_file`] reads the real `~/.config/oblisk/shell.lua` (`shared::shell_lua_path`)
//! and is `renderer/src/socket.rs`'s real entry point (build-steps.md Phase 13): both the
//! Renderer's own startup evaluation and every Supervisor-triggered `Reevaluate` round trip call
//! it, replacing Phase 11's hardcoded proof-of-wiring literal.

// `#[allow(dead_code)]` came off here per ADR-0044 decision 1: `layout::node`'s property parsers
// now run every resolved value through `check_number`/`check_integer`/`check_string` (build-steps.md
// Phase 19 item 1), so this module has a real production caller, not just its own tests.
pub mod marshal;
pub mod nodes;
pub mod process;
pub mod signal;

pub use nodes::VirtualNode;

use mlua::{Lua, LuaSerdeExt, Table, Value};

/// Owns the Lua VM for one generation. `Loader::evaluate` is stateless across calls beyond that
/// -- each call is a fresh evaluation of its `source` argument, not an incremental re-run.
pub struct Loader {
    lua: Lua,
}

#[derive(Debug, thiserror::Error)]
pub enum LoaderError {
    /// `shell.lua` failed to parse or raised a runtime error while evaluating.
    #[error("shell.lua failed to evaluate: {0}")]
    Eval(#[from] mlua::Error),
    /// The script evaluated cleanly, but its top-level return wasn't a `surface` node or a
    /// non-empty array of `surface` nodes (§ 6.1).
    #[error("shell.lua's top-level return must be a `surface` node or an array of them: {0}")]
    InvalidTopLevelReturn(String),
    /// [`Loader::evaluate_file`] couldn't read `shell.lua` off disk (missing file, permissions).
    #[error("failed to read shell.lua: {0}")]
    Io(#[from] std::io::Error),
    /// A surface evaluated cleanly and had a valid top-level shape, but one of its topology
    /// fields (`id`/`layer`/`anchor`/`monitor`, § 6.1) didn't type-check
    /// (`renderer/src/socket.rs`'s `surfaces_topology`). Distinct from [`Self::InvalidTopLevelReturn`]
    /// -- that variant's fixed message is about the *shape* of the top-level return, which is
    /// wrong for a field-level error inside an otherwise-valid surface.
    #[error("shell.lua's surface topology is invalid: {0}")]
    InvalidTopology(String),
}

impl From<nodes::DeserializeError> for LoaderError {
    fn from(err: nodes::DeserializeError) -> Self {
        LoaderError::InvalidTopLevelReturn(err.to_string())
    }
}

/// What one `Loader::evaluate` call produces: the top-level `surface` node(s), each still
/// carrying its own topology fields (`id`/`layer`/`anchor`/`monitor`/`exclusive`, § 6.1) directly
/// in its `properties` bag -- readable without walking into `child` (see `nodes.rs`'s doc
/// comment). This is the cheap-to-diff output Phase 13's Watcher will compare across reloads.
#[derive(Debug)]
pub struct LoadOutput {
    pub surfaces: Vec<VirtualNode>,
}

impl Loader {
    pub fn new() -> mlua::Result<Self> {
        let lua = Lua::new();
        nodes::register_node_constructors(&lua)?;
        signal::register(&lua)?;
        Ok(Loader { lua })
    }

    pub fn evaluate(&self, source: &str) -> Result<LoadOutput, LoaderError> {
        let value: Value = self.lua.load(source).eval()?;
        Ok(LoadOutput { surfaces: collect_surfaces(value)? })
    }

    /// Reads `path` and evaluates it exactly like [`Self::evaluate`] -- the real `shell.lua`
    /// entry point (build-steps.md Phase 13). See the module doc comment.
    pub fn evaluate_file(&self, path: &std::path::Path) -> Result<LoadOutput, LoaderError> {
        let source = std::fs::read_to_string(path)?;
        self.evaluate(&source)
    }

    /// Creates a fresh, empty Lua table on this `Loader`'s own VM -- lets a caller build an
    /// initial value for [`signal::Signal::new_live`] (e.g. `renderer/src/socket.rs`'s `rescue`
    /// signal) without reaching into a private `Lua` field.
    pub fn create_table(&self) -> mlua::Result<Table> {
        self.lua.create_table()
    }

    /// The `Lua` state itself, for a caller that needs to resolve a `Signal` (ADR-0044 decision
    /// 1). `layout::Scene::apply` and everything it calls need this to reach
    /// `signal::Signal::get_value`, which takes `&Lua` rather than recovering one from `self`
    /// (see that method's doc comment).
    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// Registers `value` as a global Lua name, visible to every later `evaluate` call on this
    /// `Loader`. The node constructors and `computed` already register their own globals
    /// internally at [`Loader::new`] time; this is the same mechanism exposed for a caller
    /// outside this module -- Phase 11 uses it to give a [`signal::Signal`] a name a `shell.lua`
    /// script can reference.
    pub fn set_global<T: mlua::IntoLua>(&self, name: &str, value: T) -> mlua::Result<()> {
        self.lua.globals().set(name, value)
    }

    /// Registers the `process` global table (`process.run`/`ProcessHandle:kill()`,
    /// build-steps.md Phase 15 item 1) onto this `Loader`'s own VM -- the same "expose a bit of
    /// Lua for a caller outside this module" shape as [`Self::set_global`]/[`Self::create_table`],
    /// needed here because registering a whole table-with-a-closure can't go through either of
    /// those alone.
    pub fn register_process(&self, registry: process::ProcessRegistry) -> mlua::Result<()> {
        process::register(&self.lua, registry)
    }

    /// Converts a JSON value into the equivalent Lua value, on this `Loader`'s own `Lua` state (a
    /// `Value` is tied to the state that created it). Turns a pushed `StateSnapshot`'s
    /// `serde_json::Value` payload into something a `LiveSignalHandle::set` call can store.
    pub fn to_lua_value(&self, json: &serde_json::Value) -> mlua::Result<Value> {
        self.lua.to_value(json)
    }
}

fn collect_surfaces(value: Value) -> Result<Vec<VirtualNode>, LoaderError> {
    let table = match value {
        Value::Table(t) => t,
        other => {
            return Err(LoaderError::InvalidTopLevelReturn(format!("expected a table, got {}", other.type_name())));
        }
    };

    if table.contains_key("kind")? {
        let node = nodes::deserialize_lua_table(&table)?;
        require_surface(&node)?;
        return Ok(vec![node]);
    }

    let mut surfaces = Vec::new();
    for entry in table.sequence_values::<Table>() {
        let entry = entry.map_err(LoaderError::from)?;
        let node = nodes::deserialize_lua_table(&entry)?;
        require_surface(&node)?;
        surfaces.push(node);
    }
    if surfaces.is_empty() {
        return Err(LoaderError::InvalidTopLevelReturn("the returned table has no `kind` field and no array elements".to_string()));
    }
    Ok(surfaces)
}

fn require_surface(node: &VirtualNode) -> Result<(), LoaderError> {
    if node.kind == "surface" {
        Ok(())
    } else {
        Err(LoaderError::InvalidTopLevelReturn(format!("top-level node must be `surface`, got `{}`", node.kind)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluate_rejects_a_lua_syntax_error_as_an_eval_error() {
        let loader = Loader::new().unwrap();
        let err = loader.evaluate("this is not lua").unwrap_err();
        assert!(matches!(err, LoaderError::Eval(_)));
    }

    #[test]
    fn evaluate_rejects_a_top_level_return_that_is_not_a_surface() {
        let loader = Loader::new().unwrap();
        let err = loader.evaluate(r#"return rect { background = "red" }"#).unwrap_err();
        assert!(matches!(err, LoaderError::InvalidTopLevelReturn(_)));
    }

    #[test]
    fn evaluate_accepts_a_single_top_level_surface() {
        let loader = Loader::new().unwrap();
        let output = loader.evaluate(r#"return surface { id = "bar", layer = "Top" }"#).unwrap();
        assert_eq!(output.surfaces.len(), 1);
        assert_eq!(output.surfaces[0].kind, "surface");
    }

    #[test]
    fn set_global_registers_a_value_a_later_evaluate_can_see() {
        let loader = Loader::new().unwrap();
        let (signal, handle) = signal::Signal::new_live(Value::Integer(7));
        loader.set_global("audio", signal).unwrap();

        let output = loader.evaluate(r#"return surface { id = "bar", layer = "Top", reading = audio:get() }"#).unwrap();
        assert_eq!(output.surfaces[0].properties.get("reading").unwrap().as_integer().unwrap(), 7);

        handle.set(Value::Integer(9));
        let output = loader.evaluate(r#"return surface { id = "bar", layer = "Top", reading = audio:get() }"#).unwrap();
        assert_eq!(output.surfaces[0].properties.get("reading").unwrap().as_integer().unwrap(), 9);
    }

    #[test]
    fn to_lua_value_converts_a_json_object_a_script_can_read_fields_from() {
        let loader = Loader::new().unwrap();
        let json = serde_json::json!({ "volume": 0.5, "muted": false });
        let value = loader.to_lua_value(&json).unwrap();
        loader.set_global("state", value).unwrap();

        let output = loader.evaluate(r#"return surface { id = "bar", layer = "Top", volume = state.volume, muted = state.muted }"#).unwrap();
        assert_eq!(output.surfaces[0].properties.get("volume").unwrap().as_f64().unwrap(), 0.5);
        assert_eq!(output.surfaces[0].properties.get("muted").unwrap(), &Value::Boolean(false));
    }

    #[test]
    fn evaluate_file_reads_and_evaluates_a_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shell.lua");
        std::fs::write(&path, r#"return surface { id = "bar", layer = "Top" }"#).unwrap();

        let loader = Loader::new().unwrap();
        let output = loader.evaluate_file(&path).unwrap();
        assert_eq!(output.surfaces.len(), 1);
        assert_eq!(output.surfaces[0].kind, "surface");
    }

    #[test]
    fn evaluate_file_on_a_missing_path_is_an_io_error() {
        let loader = Loader::new().unwrap();
        let err = loader.evaluate_file(std::path::Path::new("/no/such/shell.lua")).unwrap_err();
        assert!(matches!(err, LoaderError::Io(_)));
    }

    #[test]
    fn evaluate_accepts_an_array_of_top_level_surfaces() {
        let loader = Loader::new().unwrap();
        let output = loader
            .evaluate(
                r#"
                return {
                    surface { id = "bar" },
                    surface { id = "overlay" },
                }
                "#,
            )
            .unwrap();
        assert_eq!(output.surfaces.len(), 2);
    }
}
