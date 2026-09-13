//! Lua VM bootstrap and loader (`CONTEXT.md`, Loader): evaluates `shell.lua` into top-level `panel`
//! nodes and topology for a candidate's first evaluation and the authoritative generation's
//! re-evaluation on an in-place reload. `Loader::evaluate_file` reads `~/.config/obelisk/shell.lua`
//! (`shared::shell_lua_path`) and is the `renderer/src/socket.rs` entry point on startup and every
//! Supervisor-triggered `Reevaluate`.
pub mod action;
pub mod capability;
pub mod fonts;
pub mod fuzzy;
pub mod idle;
pub mod json;
pub mod marshal;
pub mod namespace;
pub mod nodes;
pub mod process;
pub mod session_process;
pub mod signal;
pub mod store;
pub mod surfaces;
pub mod timer;

pub use nodes::VirtualNode;
use std::cell::RefCell;

use mlua::{Lua, Table, Value};

/// Config VM libraries, explicit instead of `StdLib::ALL_SAFE` (ADR-0048). Lua runs on the Wayland
/// thread (ADR-0039), so `io.read` or `os.execute` would freeze every monitor; the 5ms instruction
/// cap cannot catch a parked syscall (ADR-0021). `IO` is absent; `OS` remains for [`restrict_os`]'s
/// four safe calls.
fn config_stdlib() -> mlua::StdLib {
    mlua::StdLib::COROUTINE
        | mlua::StdLib::TABLE
        | mlua::StdLib::STRING
        | mlua::StdLib::UTF8
        | mlua::StdLib::MATH
        | mlua::StdLib::PACKAGE
        | mlua::StdLib::OS
}

/// ADR-0048's four non-blocking, process-local `os` calls. Bars use `os.date`, so `OS` cannot be
/// removed wholesale.
const OS_CALLS_THAT_CANNOT_BLOCK: [&str; 4] = ["time", "date", "clock", "getenv"];

/// Replaces `os` with an allowlist of [`OS_CALLS_THAT_CANNOT_BLOCK`], copying the real functions so
/// `os.date` keeps its strftime surface. Also replace `package.loaded.os`: `require` reads its own
/// reference there, so changing only the global hands the library back through `require("os")`
/// (measured for `io` under the old `ALL_SAFE` VM). `io` is absent from [`config_stdlib`], so only
/// `os` needs this second replacement.
fn restrict_os(lua: &Lua) -> mlua::Result<()> {
    let full: Table = lua.globals().get("os")?;
    let kept = lua.create_table()?;
    for name in OS_CALLS_THAT_CANNOT_BLOCK {
        kept.set(name, full.get::<Value>(name)?)?;
    }
    lua.globals().set("os", &kept)?;
    lua.globals().get::<Table>("package")?.get::<Table>("loaded")?.set("os", &kept)
}

/// Points `require` only at the config directory (ADR-0047 decision 1), replacing Lua's default
/// `/usr/local/share/lua/5.4/?.lua;...;./?.lua;./?/init.lua`. System entries let installed modules
/// shadow config modules; `./` uses an unset Supervisor working directory, and a split config broke
/// under systemd despite passing by-hand runs. A user path list is the luarocks upgrade path, not a
/// silent restoration. `package.cpath` stays untouched: safe mode replaces C searchers and makes
/// `package.loadlib` raise, so stale `cpath` loads nothing.
///
/// `package.path` has no escape syntax: non-UTF-8 paths substitute lossily, `;` splits an entry,
/// and `?` is replaced along with the real marker. No startup check for `$XDG_CONFIG_HOME/obelisk`.
fn point_package_path_at(lua: &Lua, config_dir: &std::path::Path) -> mlua::Result<()> {
    let dir = config_dir.display();
    lua.globals().get::<Table>("package")?.set("path", format!("{dir}/?.lua;{dir}/?/init.lua"))
}

/// Owns one generation's Lua VM. Each `Loader::evaluate` call freshly evaluates `source`; it is not
/// incremental.
pub struct Loader {
    lua: Lua,
    /// `obelisk.idle` thresholds, cleared before every evaluation. `Option` covers `Loader::new`
    /// running before `lua::namespace::build` creates the registry (`None` in tests); `RefCell`
    /// permits the one post-construction registration.
    idle: RefCell<Option<idle::IdleRegistry>>,
    /// Standard-library keys in `package.loaded`, captured before config runs for
    /// [`Loader::forget_config_modules`] to subtract. Capturing tracks [`config_stdlib`] changes;
    /// hardcoding could evict a newly added standard module.
    standard_modules: std::collections::HashSet<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum LoaderError {
    /// `shell.lua` failed to parse or raised during evaluation.
    #[error("shell.lua failed to evaluate: {0}")]
    Eval(#[from] mlua::Error),
    /// Clean evaluation returned neither a § 6 surface nor an array. Empty array and no return are
    /// valid: a config may declare no surfaces (ADR-0070 decision 7).
    #[error("shell.lua's top-level return must be a `panel` node or an array of them: {0}")]
    InvalidTopLevelReturn(String),
    /// [`Loader::evaluate_file`] could not read `shell.lua` (missing file, permissions).
    #[error("failed to read shell.lua: {0}")]
    Io(#[from] std::io::Error),
    /// A valid top-level surface had a mistyped § 6 topology field
    /// (`id`/`layer`/`anchor`/`monitor`), distinct from [`Self::InvalidTopLevelReturn`].
    #[error("shell.lua's surface topology is invalid: {0}")]
    InvalidTopology(String),
}

impl From<nodes::DeserializeError> for LoaderError {
    fn from(err: nodes::DeserializeError) -> Self {
        LoaderError::InvalidTopLevelReturn(err.to_string())
    }
}

/// One `Loader::evaluate` result: top-level `panel` nodes with § 6 topology
/// (`id`/`layer`/`anchor`/`monitor`/`exclusive`) directly in `properties`, so the Watcher can diff
/// them without walking into `child`.
#[derive(Debug)]
pub struct LoadOutput {
    pub surfaces: Vec<VirtualNode>,
}

impl Loader {
    /// Threads the generation's one scene-dirty flag (ADR-0044 decision 2) into `state(name,
    /// initial)`, so `:set()` and capability pushes dirty the same scene. `require` searches only
    /// `config_dir` ([`point_package_path_at`]).
    pub fn new(dirty: signal::DirtyFlag, config_dir: &std::path::Path) -> mlua::Result<Self> {
        let lua = Lua::new_with(config_stdlib(), mlua::LuaOptions::default())?;
        restrict_os(&lua)?;
        point_package_path_at(&lua, config_dir)?;
        nodes::register_node_constructors(&lua)?;
        action::register(&lua)?;
        json::register(&lua)?;
        fonts::register(&lua)?;
        fuzzy::register(&lua)?;
        signal::register(&lua, dirty)?;
        store::register(&lua)?;
        session_process::register(&lua)?;
        timer::register(&lua)?;
        let standard_modules = loaded_module_names(&lua)?;
        Ok(Loader { lua, standard_modules, idle: RefCell::new(None) })
    }

    /// Test-only evaluation under the generic `shell.lua` chunk name; production has a real path.
    #[cfg(test)]
    pub fn evaluate(&self, source: &str) -> Result<LoadOutput, LoaderError> {
        self.evaluate_named(source, "shell.lua")
    }

    /// Reads and evaluates the real `shell.lua` path.
    pub fn evaluate_file(&self, path: &std::path::Path) -> Result<LoadOutput, LoaderError> {
        let source = std::fs::read_to_string(path)?;
        self.evaluate_named(&source, &path.display().to_string())
    }

    /// Names the chunk in config errors. Without it mlua names this Rust call site; a live session
    /// reported a `shell.lua` typo as `renderer/src/lua/mod.rs:74:127`. Leading `@` marks a file
    /// path (`lua_Debug.source`); without it Lua renders `[string "/mnt/.../dev-conf..."]` and
    /// truncates the path before the filename.
    fn evaluate_named(&self, source: &str, name: &str) -> Result<LoadOutput, LoaderError> {
        self.forget_config_modules()?;
        // Callbacks belong to the tree this evaluation replaces (`lua::idle`'s module doc).
        if let Some(idle) = self.idle.borrow().as_ref() {
            idle.forget_thresholds();
        }
        let value: Value = self.lua.load(source).set_name(format!("@{name}")).eval()?;
        Ok(LoadOutput { surfaces: collect_surfaces(value)? })
    }

    /// Fresh empty table on this VM, for [`signal::Signal::new_live`] initial values.
    pub fn create_table(&self) -> mlua::Result<Table> {
        self.lua.create_table()
    }

    /// The VM for resolving a `Signal` (ADR-0044 decision 1); `layout::Scene::apply` needs it for
    /// `signal::Signal::get_value`.
    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// Registers `value` as a global visible to later evaluations, exposing [`Loader::new`]'s
    /// constructor/computed mechanism to callers outside this module.
    pub fn set_global<T: mlua::IntoLua>(&self, name: &str, value: T) -> mlua::Result<()> {
        self.lua.globals().set(name, value)
    }

    /// Registers the `process` table (`process.run`/`ProcessHandle:kill()`); a table with a closure
    /// cannot go through [`Self::set_global`] and [`Self::create_table`] alone.
    pub fn register_process(&self, registry: process::ProcessRegistry) -> mlua::Result<()> {
        process::register(&self.lua, registry)
    }

    /// Gives the loader the `obelisk.idle` registry from `lua::namespace::build`, so
    /// [`Self::evaluate_file`] clears thresholds before re-running `shell.lua`. The member is a
    /// namespace field; `process` is a global.
    pub(crate) fn register_idle(&self, registry: idle::IdleRegistry) {
        *self.idle.borrow_mut() = Some(registry);
    }

    /// Converts JSON into a value tied to this VM, for `LiveSignalHandle::set` to store in a pushed
    /// `StateSnapshot`. Delegates to [`json::to_lua`], shared with `json.decode`, so `null` maps
    /// one way everywhere.
    pub fn to_lua_value(&self, json: &serde_json::Value) -> mlua::Result<Value> {
        json::to_lua(&self.lua, json)
    }

    /// Drops config-owned `package.loaded` modules, preserving the standard library (ADR-0047
    /// decision 2). One VM per generation (ADR-0044 decision 4) and name-based `require` caching
    /// would otherwise make an edited `widgets/clock.lua` silently stale. Collect names before
    /// removal: mlua's iterator holds the table, so clearing during traversal would mutate what it
    /// reads.
    fn forget_config_modules(&self) -> mlua::Result<()> {
        let loaded = self.lua.globals().get::<Table>("package")?.get::<Table>("loaded")?;
        let stale: Vec<String> = loaded
            .pairs::<String, Value>()
            .filter_map(Result::ok)
            .map(|(name, _)| name)
            .filter(|name| !self.standard_modules.contains(name))
            .collect();
        for name in stale {
            loaded.set(name, Value::Nil)?;
        }
        Ok(())
    }
}

/// `package.loaded` names at the end of [`Loader::new`], before config modules exist.
fn loaded_module_names(lua: &Lua) -> mlua::Result<std::collections::HashSet<String>> {
    lua.globals()
        .get::<Table>("package")?
        .get::<Table>("loaded")?
        .pairs::<String, Value>()
        .map(|pair| pair.map(|(name, _)| name))
        .collect()
}

/// Explains the "surface N is a X" case a config author cannot see by reading the file. Running
/// the shipped config after splitting it across 32 files found `return { require(a), require(b) }`
/// became a three-element list ending in a string.
const REQUIRE_RETURNS_TWO_VALUES: &str = ". If that element came from a `require` in the last \
position of this table, note that Lua 5.4's `require` returns the module *and* its file path, and a \
call in last position expands to both: bind it to a local first";

fn collect_surfaces(value: Value) -> Result<Vec<VirtualNode>, LoaderError> {
    let table = match value {
        Value::Table(t) => t,
        // Legal (ADR-0070 decision 7): no `return` is Lua's `nil`.
        Value::Nil => return Ok(Vec::new()),
        other => {
            return Err(LoaderError::InvalidTopLevelReturn(format!("expected a table, got {}", other.type_name())));
        }
    };

    if table.contains_key("kind")? {
        let node = nodes::deserialize_lua_table(&table)?;
        require_surface(&node)?;
        return Ok(vec![node]);
    }

    // Type-check each element as `Value`: `sequence_values::<Table>()` reports "error converting
    // Lua string to table" without naming the element or its value, and misclassifies a valid
    // evaluation as `LoaderError::Eval`.
    let mut surfaces = Vec::new();
    for (index, entry) in table.sequence_values::<Value>().enumerate() {
        let entry = entry.map_err(LoaderError::from)?;
        let Value::Table(entry) = entry else {
            return Err(LoaderError::InvalidTopLevelReturn(format!(
                "surface {} is a {}, not a node{}",
                index + 1,
                entry.type_name(),
                REQUIRE_RETURNS_TWO_VALUES
            )));
        };
        let node = nodes::deserialize_lua_table(&entry)?;
        require_surface(&node)?;
        surfaces.push(node);
    }
    // `return {}` declares no surfaces, not a mistake (ADR-0070 decision 7).
    Ok(surfaces)
}

/// Admits all four § 6 root roles (ADR-0040 decision 1). Declaration location and Wayland-object
/// lifetime are separate (ADR-0052 decision 2), as ADR-0049 already established for `window` (no
/// `xdg_toplevel`
/// until `visible` is true) and `popup`. `lock` owns no `ext_session_lock_surface_v1` until the
/// compositor sends `locked`; rejecting it would leave § 6's authored lock-screen `child` nowhere
/// legal to write.
fn require_surface(node: &VirtualNode) -> Result<(), LoaderError> {
    match node.kind.as_str() {
        "panel" | "window" | "popup" | "lock" => Ok(()),
        other => Err(LoaderError::InvalidTopLevelReturn(format!(
            "top-level node must be `panel`, `window`, `popup` or `lock`, got `{other}`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal loader for tests that do not `require`; evaluates `setup` above a `panel` and reads
    /// back a global. A node would reject a probe key not in `nodes::NODE_PROPERTIES`.
    fn probe<T: mlua::FromLua>(loader: &Loader, setup: &str, name: &str) -> T {
        loader.evaluate(&format!("{setup}\nreturn panel {{ id = \"bar\", layer = \"Top\" }}")).unwrap();
        loader.lua().globals().get(name).unwrap()
    }

    fn test_loader() -> Loader {
        Loader::new(signal::DirtyFlag::new(), &std::env::temp_dir()).unwrap()
    }

    /// ADR-0047 decision 1.
    #[test]
    fn require_resolves_a_module_inside_the_config_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("widgets")).unwrap();
        std::fs::write(dir.path().join("widgets/clock.lua"), r#"return { label = "tick" }"#).unwrap();
        let loader = Loader::new(signal::DirtyFlag::new(), dir.path()).unwrap();

        let label: String = loader.lua().load(r#"return require("widgets.clock").label"#).eval().unwrap();
        assert_eq!(label, "tick");
    }

    /// `?/init.lua`: `require "widgets"` resolves a directory as well as a file.
    #[test]
    fn require_resolves_a_directory_through_its_init_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("widgets")).unwrap();
        std::fs::write(dir.path().join("widgets/init.lua"), r#"return { label = "index" }"#).unwrap();
        let loader = Loader::new(signal::DirtyFlag::new(), dir.path()).unwrap();

        let label: String = loader.lua().load(r#"return require("widgets").label"#).eval().unwrap();
        assert_eq!(label, "index");
    }

    #[test]
    fn package_path_reaches_nowhere_but_the_config_directory() {
        let dir = tempfile::tempdir().unwrap();
        let loader = Loader::new(signal::DirtyFlag::new(), dir.path()).unwrap();

        let path: String = loader.lua().load("return package.path").eval().unwrap();
        let prefix = dir.path().display().to_string();
        for entry in path.split(';') {
            assert!(entry.starts_with(&prefix), "`{entry}` resolves outside the config directory");
        }
    }

    /// ADR-0047 decision 2 / ADR-0044 decision 4: without clearing module cache, reload runs
    /// `shell.lua` against a stale required module and silently does nothing.
    #[test]
    fn a_re_evaluation_sees_an_edited_required_module_rather_than_the_cached_one() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("theme.lua");
        std::fs::write(&module, r#"return { accent = "first" }"#).unwrap();
        let loader = Loader::new(signal::DirtyFlag::new(), dir.path()).unwrap();
        let source = r#"return panel { id = "bar", layer = "Top", background = require("theme").accent }"#;

        let first = loader.evaluate(source).unwrap();
        assert_eq!(accent(&first), "first");

        std::fs::write(&module, r#"return { accent = "second" }"#).unwrap();
        let second = loader.evaluate(source).unwrap();
        assert_eq!(
            accent(&second),
            "second",
            "the re-evaluation ran against the cached module, so the edit did nothing"
        );
    }

    /// The idle half: callbacks belong to the replaced tree, so retaining them would run the old
    /// config's `on_idle` once more per reload forever (`lua::idle`).
    #[test]
    fn a_re_evaluation_drops_the_idle_thresholds_the_previous_one_registered() {
        let dir = tempfile::tempdir().unwrap();
        let loader = Loader::new(signal::DirtyFlag::new(), dir.path()).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let commands = capability::CommandSender::new(0, tx);
        let (idle_state, _idle_handle) =
            capability::Capability::new("idle", signal::DirtyFlag::new(), commands.clone());
        let registry = idle::IdleRegistry::new(idle_state);
        loader.set_global("idle", registry.member()).unwrap();
        loader.register_idle(registry.clone());
        let source = r#"
            runs = (runs or 0)
            idle:register_threshold(30, function() runs = runs + 1 end, function() end)
            return panel { id = "bar", layer = "Top" }
        "#;

        loader.evaluate(source).unwrap();
        loader.evaluate(source).unwrap();
        registry.dispatch_event(30, shared::IdleState::Idled);

        assert_eq!(
            loader.lua().load("return runs").eval::<i64>().unwrap(),
            1,
            "the second evaluation stacked a second copy of the callback on the same threshold"
        );
    }

    /// Cache clearing must stop at config modules; evicting standard `string` would break
    /// `string.format`.
    #[test]
    fn clearing_the_module_cache_leaves_the_standard_library_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let loader = Loader::new(signal::DirtyFlag::new(), dir.path()).unwrap();
        loader.evaluate(r#"return panel { id = "bar", layer = "Top" }"#).unwrap();

        let intact: bool = loader
            .lua()
            .load(r#"return require("string").format == string.format and require("table") ~= nil"#)
            .eval()
            .unwrap();
        assert!(intact, "a standard module was evicted along with the config's own");
    }

    fn accent(output: &LoadOutput) -> String {
        output.surfaces[0].properties.get("background").unwrap().as_string().unwrap().to_string_lossy().to_string()
    }

    /// Because `forget_config_modules` reads `package.loaded` before every evaluation, deleting
    /// `package` affects the next reload. That failure must be reported, not silently ignored.
    #[test]
    fn a_config_that_deletes_the_package_table_fails_its_next_reload_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let loader = Loader::new(signal::DirtyFlag::new(), dir.path()).unwrap();
        loader.evaluate(r#"package = nil return panel { id = "bar", layer = "Top" }"#).unwrap();

        let second = loader.evaluate(r#"return panel { id = "bar", layer = "Top" }"#);
        assert!(
            matches!(second, Err(LoaderError::Eval(_))),
            "the next reload has to report, not quietly skip the cache clear: {second:?}"
        );
    }

    /// Required modules share `shell.lua`'s VM, seeing node constructors and engine globals without
    /// being handed them. A regression here would otherwise be silent.
    #[test]
    fn a_required_module_sees_the_same_globals_shell_lua_does() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("widget.lua"),
            r#"return { node = row { spacing = 4, children = { text { content = "hi" } } }, has_state = state ~= nil, has_json = json ~= nil }"#,
        )
        .unwrap();
        let loader = Loader::new(signal::DirtyFlag::new(), dir.path()).unwrap();

        let output = loader
            .evaluate(
                r#"local w = require("widget")
                   has_state, has_json = w.has_state, w.has_json
                   return panel { id = "bar", layer = "Top", child = w.node }"#,
            )
            .unwrap();
        assert!(loader.lua().globals().get::<bool>("has_state").unwrap());
        assert!(loader.lua().globals().get::<bool>("has_json").unwrap());
        assert!(
            output.surfaces[0].properties.contains_key("child"),
            "a node built in a required module has to survive into the tree"
        );
    }

    /// Shipped config once produced a seventh surface element that was a path string, reported as
    /// `shell.lua failed to evaluate: error converting Lua string to table`, naming neither element
    /// nor value. Lua 5.4 `require` returns module and path; in final table position
    /// `return { require(a), require(b) }` expands to three elements, the last a string.
    #[test]
    fn a_non_node_in_the_surface_list_names_which_element_and_what_it_was() {
        let loader = test_loader();
        let err = loader
            .evaluate(
                r#"return { panel { id = "bar", layer = "Top" }, "/home/me/.config/obelisk/modules/global/lock.lua" }"#,
            )
            .unwrap_err();

        assert!(
            matches!(err, LoaderError::InvalidTopLevelReturn(_)),
            "a bad element is a bad return, not an evaluation failure: {err:?}"
        );
        let message = err.to_string();
        assert!(message.contains("surface 2"), "the message has to say which element: {message}");
        assert!(message.contains("string"), "the message has to say what it got: {message}");
        assert!(message.contains("require"), "the message has to name the cause a config author cannot see: {message}");
    }

    /// Whether `expr` evaluates to `nil` in the config environment.
    fn absent(loader: &Loader, expr: &str) -> bool {
        loader.lua().load(format!("return ({expr}) == nil")).eval().unwrap()
    }

    /// ADR-0048: these block Wayland dispatch, and the 5ms CPU cap cannot catch a parked syscall.
    #[test]
    fn the_config_vm_has_no_blocking_stdlib_call_left_to_stall_wayland_dispatch() {
        let loader = test_loader();
        assert!(absent(&loader, "io"), "the whole of `io` goes, io.open and io.popen included");
        for call in ["os.execute", "os.exit", "os.remove", "os.rename", "os.tmpname", "os.setlocale"] {
            assert!(
                absent(&loader, call),
                "`{call}` blocks or kills the render thread and has no business in a config"
            );
        }
    }

    /// ADR-0048's four keeps; removing them breaks every bar's clock.
    #[test]
    fn the_config_vm_keeps_the_four_os_calls_that_cannot_block() {
        let loader = test_loader();
        for call in ["os.time", "os.date", "os.clock", "os.getenv"] {
            assert!(!absent(&loader, call), "`{call}` cannot block and a config needs it");
        }
        let year: i64 = loader.lua().load(r#"return tonumber(os.date("%Y"))"#).eval().unwrap();
        assert!(year >= 2024, "os.date has to be the real one, not a stub: got {year}");
    }

    /// `require` reads its own `package.loaded` reference, so removing only a global hands the
    /// library back through `local io = require("io")`.
    #[test]
    fn require_cannot_hand_back_the_stdlib_the_vm_just_dropped() {
        let loader = test_loader();
        let io_open: bool = loader
            .lua()
            .load(r#"local ok, m = pcall(require, "io") return not ok or type(m) ~= "table" or m.open == nil"#)
            .eval()
            .unwrap();
        assert!(io_open, "`require(\"io\")` gave back a working io.open");
        let os_execute: bool = loader
            .lua()
            .load(r#"local ok, m = pcall(require, "os") return not ok or type(m) ~= "table" or m.execute == nil"#)
            .eval()
            .unwrap();
        assert!(os_execute, "`require(\"os\")` gave back a working os.execute");
    }

    #[test]
    fn evaluate_rejects_a_lua_syntax_error_as_an_eval_error() {
        let loader = test_loader();
        let err = loader.evaluate("this is not lua").unwrap_err();
        assert!(matches!(err, LoaderError::Eval(_)));
    }

    #[test]
    fn evaluate_rejects_a_top_level_return_that_is_not_a_surface() {
        let loader = test_loader();
        let err = loader.evaluate(r#"return rect { background = "red" }"#).unwrap_err();
        assert!(matches!(err, LoaderError::InvalidTopLevelReturn(_)));
    }

    /// ADR-0070 decision 7: "run nothing" is the state where a config gates every capability off.
    #[test]
    fn a_config_may_declare_no_surfaces_at_all() {
        let loader = test_loader();
        assert!(loader.evaluate("return {}").unwrap().surfaces.is_empty());
    }

    /// An empty chunk has no `return`, so Lua yields `nil`, not a table.
    #[test]
    fn an_empty_shell_lua_declares_no_surfaces_rather_than_failing() {
        let loader = test_loader();
        assert!(loader.evaluate("").unwrap().surfaces.is_empty());
    }

    /// Empty cases must not widen the shape check: a wrong-kind top-level return remains an error
    /// naming what it got.
    #[test]
    fn a_top_level_return_that_is_neither_a_table_nor_nil_is_still_refused() {
        let loader = test_loader();
        let err = loader.evaluate(r#"return "bar""#).unwrap_err();
        assert!(matches!(err, LoaderError::InvalidTopLevelReturn(_)), "{err:?}");
        assert!(err.to_string().contains("string"), "the message has to say what it got: {err}");
    }

    #[test]
    fn evaluate_accepts_a_single_top_level_surface() {
        let loader = test_loader();
        let output = loader.evaluate(r#"return panel { id = "bar", layer = "Top" }"#).unwrap();
        assert_eq!(output.surfaces.len(), 1);
        assert_eq!(output.surfaces[0].kind, "panel");
    }

    #[test]
    fn evaluate_accepts_a_window_and_a_popup_at_the_top_level_beside_a_panel() {
        // ADR-0040 decision 1: all four § 6 roles may be declared at the root.
        let loader = test_loader();
        let output = loader
            .evaluate(
                r#"return {
                    panel { id = "bar", layer = "Top" },
                    window { id = "settings", title = "Settings" },
                    popup { id = "menu", parent = "bar", width = 200, height = 120, anchor_rect = { x = 0, y = 0, width = 86, height = 24 } },
                }"#,
            )
            .unwrap();
        let kinds: Vec<&str> = output.surfaces.iter().map(|s| s.kind.as_str()).collect();
        assert_eq!(kinds, ["panel", "window", "popup"]);
    }

    #[test]
    fn evaluate_accepts_a_top_level_lock_because_declaring_one_is_not_the_same_as_locking() {
        // ADR-0052 decision 2: the compositor decides when the lock surface exists; this only
        // admits its declaration location.
        let loader = test_loader();
        let output =
            loader.evaluate(r##"return lock { id = "screen", child = rect { background = "#000000FF" } }"##).unwrap();
        assert_eq!(output.surfaces.len(), 1);
        assert_eq!(output.surfaces[0].kind, "lock");
    }

    #[test]
    fn a_top_level_return_of_an_unknown_kind_names_all_four_roles_it_could_have_been() {
        // The catch-all teaches the roster; omitting `lock` reads as "not built yet".
        let loader = test_loader();
        let err = loader.evaluate(r#"return { kind = "rect" }"#).unwrap_err();
        let LoaderError::InvalidTopLevelReturn(message) = err else {
            panic!("a top-level `rect` must be a top-level-return error");
        };
        for role in ["panel", "window", "popup", "lock"] {
            assert!(message.contains(role), "the message must name `{role}`: {message}");
        }
    }

    #[test]
    fn set_global_registers_a_value_a_later_evaluate_can_see() {
        let loader = test_loader();
        let (signal, handle) = signal::Signal::new_live(Value::Integer(7), signal::DirtyFlag::new());
        loader.set_global("audio", signal).unwrap();

        assert_eq!(probe::<i64>(&loader, "reading = audio:get()", "reading"), 7);

        handle.set(Value::Integer(9));
        assert_eq!(probe::<i64>(&loader, "reading = audio:get()", "reading"), 9);
    }

    #[test]
    fn to_lua_value_converts_a_json_object_a_script_can_read_fields_from() {
        let loader = test_loader();
        let json = serde_json::json!({ "volume": 0.5, "muted": false });
        let value = loader.to_lua_value(&json).unwrap();
        loader.set_global("state", value).unwrap();

        assert_eq!(probe::<f64>(&loader, "volume = state.volume", "volume"), 0.5);
        assert!(!probe::<bool>(&loader, "muted = state.muted", "muted"));
    }

    /// JSON `null` must become Lua `nil` and erase the key. Checking `item.icon_path == nil` is
    /// insufficient because missing keys also read nil; count table keys to prove `icon_path` never
    /// landed.
    #[test]
    fn to_lua_value_maps_a_json_null_field_to_a_nil_that_is_absent_from_the_table() {
        let loader = test_loader();
        let json = serde_json::json!({
            "icon_name": "org.telegram.desktop-mute-symbolic",
            "icon_path": null,
        });
        let value = loader.to_lua_value(&json).unwrap();
        loader.set_global("item", value).unwrap();

        let setup = r#"
            key_count = 0
            for _ in pairs(item) do key_count = key_count + 1 end
            path_is_nil = item.icon_path == nil
        "#;
        assert_eq!(probe::<i64>(&loader, setup, "key_count"), 1);
        assert!(probe::<bool>(&loader, setup, "path_is_nil"));
    }

    /// Actual bug: mlua's default `serialize_none_to_null` maps JSON `null` to truthy
    /// lightuserdata, so a live Telegram tray item's `icon_path = null` made `if item.icon_path`
    /// assume a real path.
    /// Type checks still pass for that sentinel; only truthiness distinguishes it from nil.
    #[test]
    fn to_lua_value_a_null_field_is_falsy_not_a_truthy_lightuserdata_sentinel() {
        let loader = test_loader();
        let json = serde_json::json!({ "icon_path": null });
        let value = loader.to_lua_value(&json).unwrap();
        loader.set_global("payload", value).unwrap();

        let result: String =
            loader.lua().load(r#"if payload.icon_path then return "truthy" else return "falsy" end"#).eval().unwrap();
        assert_eq!(result, "falsy");
    }

    /// Live tray nulls are inside `items[1]`, not top-level; catches a fix that handles only the
    /// outer value and leaves the real one-level-deep sentinel truthy.
    #[test]
    fn to_lua_value_a_null_nested_inside_an_array_element_is_also_nil() {
        let loader = test_loader();
        let json = serde_json::json!({
            "items": [
                {
                    "icon_name": "org.telegram.desktop-mute-symbolic",
                    "icon_path": null,
                }
            ]
        });
        let value = loader.to_lua_value(&json).unwrap();
        loader.set_global("tray", value).unwrap();

        let result: String = loader
            .lua()
            .load(r#"if tray.items[1].icon_path then return "truthy" else return "falsy" end"#)
            .eval()
            .unwrap();
        assert_eq!(result, "falsy");
    }

    /// Trade-off: null-to-nil leaves an array hole; `ipairs` stops there, while `#` and direct
    /// indexing see past it. No capability payload has a null element today, only optional fields,
    /// but `dev-config/obelisk/shell.lua` iterates capability lists with `ipairs`.
    #[test]
    fn to_lua_value_a_null_array_element_leaves_a_hole_ipairs_stops_at() {
        let loader = test_loader();
        let json = serde_json::json!({ "xs": [1, null, 3] });
        let value = loader.to_lua_value(&json).unwrap();
        loader.set_global("state", value).unwrap();

        let ipairs_count: i64 =
            loader.lua().load("local n = 0 for _ in ipairs(state.xs) do n = n + 1 end return n").eval().unwrap();
        assert_eq!(ipairs_count, 1, "ipairs must stop at the hole the nil leaves");
        // Indexing reaches past the hole; this is a hole, not truncation.
        let third: i64 = loader.lua().load("return state.xs[3]").eval().unwrap();
        assert_eq!(third, 3);
    }

    /// Negative control: disabling `serialize_none_to_null`/`serialize_unit_to_null` changes only
    /// `Value::Null`, not ordinary numbers, strings, booleans, or arrays.
    #[test]
    fn to_lua_value_leaves_non_null_fields_unchanged() {
        let loader = test_loader();
        let json = serde_json::json!({
            "volume": 0.5,
            "muted": false,
            "label": "media",
            "tags": ["a", "b"],
        });
        let value = loader.to_lua_value(&json).unwrap();
        loader.set_global("state", value).unwrap();

        let setup = "volume, muted, label, second_tag = state.volume, state.muted, state.label, state.tags[2]";
        assert_eq!(probe::<f64>(&loader, setup, "volume"), 0.5);
        assert!(!probe::<bool>(&loader, setup, "muted"));
        assert_eq!(probe::<String>(&loader, setup, "label"), "media");
        assert_eq!(probe::<String>(&loader, setup, "second_tag"), "b");
    }

    #[test]
    fn evaluate_file_reads_and_evaluates_a_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shell.lua");
        std::fs::write(&path, r#"return panel { id = "bar", layer = "Top" }"#).unwrap();

        let loader = test_loader();
        let output = loader.evaluate_file(&path).unwrap();
        assert_eq!(output.surfaces.len(), 1);
        assert_eq!(output.surfaces[0].kind, "panel");
    }

    /// Rejection output must name the config file and line, not `renderer/src/lua/mod.rs:74:127`.
    #[test]
    fn an_evaluation_error_names_the_config_file_not_this_source_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shell.lua");
        std::fs::write(&path, "return panel { id = \"bar\" }\nthis is not lua\n").unwrap();

        let loader = test_loader();
        let message = loader.evaluate_file(&path).unwrap_err().to_string();

        assert!(message.contains(&path.display().to_string()), "expected the config path in: {message}");
        assert!(!message.contains("lua/mod.rs"), "expected no engine source path in: {message}");
        // Not `[string "/long/path/to/shell..."]`: Lua truncates non-`@` chunk names before the
        // filename.
        assert!(!message.contains("[string"), "expected a file-named chunk in: {message}");
        // Include the line to fix, not just the file.
        assert!(message.contains(":2:"), "expected the offending line number in: {message}");
    }

    #[test]
    fn evaluate_file_on_a_missing_path_is_an_io_error() {
        let loader = test_loader();
        let err = loader.evaluate_file(std::path::Path::new("/no/such/shell.lua")).unwrap_err();
        assert!(matches!(err, LoaderError::Io(_)));
    }

    #[test]
    fn evaluate_accepts_an_array_of_top_level_surfaces() {
        let loader = test_loader();
        let output = loader
            .evaluate(
                r#"
                return {
                    panel { id = "bar" },
                    panel { id = "overlay" },
                }
                "#,
            )
            .unwrap();
        assert_eq!(output.surfaces.len(), 2);
    }
}
