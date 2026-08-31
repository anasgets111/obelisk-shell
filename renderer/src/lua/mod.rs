//! Lua VM bootstrap and the loader (`CONTEXT.md`, Loader): evaluates `shell.lua` into the
//! top-level `panel` node(s) and their topology, reused for both a candidate's first evaluation
//! and the authoritative generation's re-evaluation on an in-place reload.
//!
//! [`Loader::evaluate_file`] reads the real `~/.config/oblisk/shell.lua` (`shared::shell_lua_path`)
//! and is `renderer/src/socket.rs`'s real entry point: both the Renderer's own startup evaluation
//! and every Supervisor-triggered `Reevaluate` round trip call it.
pub mod capability;
pub mod marshal;
pub mod json;
pub mod namespace;
pub mod nodes;
pub mod process;
pub mod signal;
pub mod surfaces;

pub use nodes::VirtualNode;

use mlua::{Lua, Table, Value};

/// What a config's VM loads, spelled out rather than mlua's `StdLib::ALL_SAFE` (docs/adr/0048):
/// `ALL_SAFE` leaves `io` and `os` whole, and the Lua VM runs on the Wayland thread (docs/adr/0039),
/// so `io.read` or `os.execute` in a `computed` freezes every surface on every monitor until it
/// returns -- the 5ms CPU cap (docs/adr/0021) cannot catch it, since it's an instruction-count hook
/// and a thread parked in a syscall executes no instructions.
///
/// `IO` is absent outright. `OS` is loaded here only so [`restrict_os`] can lift the four calls
/// worth keeping out of it; nothing else in that library survives the next line of `Loader::new`.
fn config_stdlib() -> mlua::StdLib {
    mlua::StdLib::COROUTINE | mlua::StdLib::TABLE | mlua::StdLib::STRING | mlua::StdLib::UTF8 | mlua::StdLib::MATH | mlua::StdLib::PACKAGE | mlua::StdLib::OS
}

/// The four `os` calls ADR-0048 keeps, each of which reads process-local state and returns without
/// a syscall that waits. A bar's clock is `os.date`, so cutting the library whole was never an
/// option.
const OS_CALLS_THAT_CANNOT_BLOCK: [&str; 4] = ["time", "date", "clock", "getenv"];

/// Replaces the `os` global with a table holding only [`OS_CALLS_THAT_CANNOT_BLOCK`].
///
/// An allowlist rather than deleting the dangerous keys, so a call added to the library by a
/// future Lua or mlua is excluded by default instead of arriving unnoticed. Copying the real
/// functions rather than reimplementing them keeps `os.date`'s whole strftime surface exact.
///
/// `package.loaded` gets the same table, and that is the load-bearing half. `require` answers out
/// of `package.loaded`, which holds its own reference to the table a library was loaded into, so
/// replacing the global alone would hand the full library straight back to `local os =
/// require("os")`. Measured under the old `ALL_SAFE` VM, `require("io")` did return a working
/// `io.open`, which is what named the hole. `io` is never in [`config_stdlib`], so it has no
/// `package.loaded` entry to reclaim -- this matters for `os` and only `os`.
fn restrict_os(lua: &Lua) -> mlua::Result<()> {
    let full: Table = lua.globals().get("os")?;
    let kept = lua.create_table()?;
    for name in OS_CALLS_THAT_CANNOT_BLOCK {
        kept.set(name, full.get::<Value>(name)?)?;
    }
    lua.globals().set("os", &kept)?;
    lua.globals().get::<Table>("package")?.get::<Table>("loaded")?.set("os", &kept)
}

/// Points `require` at the config directory and nothing else (docs/adr/0047 decision 1).
///
/// Replaces Lua's compiled-in default (`/usr/local/share/lua/5.4/?.lua;...;./?.lua;./?/init.lua`)
/// rather than prepending to it. The system entries let a same-named module installed system
/// wide shadow the config's own. The `./` entries resolve against the process's working
/// directory, which nothing in the Supervisor sets, so a split config worked when started from
/// its own directory and failed from anywhere else -- the failure mode that passes every test run
/// by hand and breaks under systemd. Installed Lua libraries become unreachable as a result, which
/// is the right default for a shell config; a user-declared path list appended here is the
/// upgrade path for luarocks, not a silent restoration of the system default.
///
/// `package.cpath` is deliberately left alone: mlua's safe mode already replaces the C searchers
/// and makes `package.loadlib` raise, so a stale `cpath` loads nothing.
///
/// Three limits with no fix at this boundary, because `package.path` is a plain Lua string with no
/// escape syntax. A config directory path that isn't valid UTF-8 is substituted lossily. A `;` in
/// it splits the path into two wrong search entries. A `?` in it gets replaced by Lua along with
/// the real substitution marker, corrupting the search rather than failing to find anything. All
/// three are silent and none is expressible in `package.path`, so the only real fix is rejecting
/// such a directory at startup -- not worth building for a path that is `$XDG_CONFIG_HOME/oblisk`.
fn point_package_path_at(lua: &Lua, config_dir: &std::path::Path) -> mlua::Result<()> {
    let dir = config_dir.display();
    lua.globals().get::<Table>("package")?.set("path", format!("{dir}/?.lua;{dir}/?/init.lua"))
}

/// Owns the Lua VM for one generation. `Loader::evaluate` is stateless across calls beyond that
/// -- each call is a fresh evaluation of its `source` argument, not an incremental re-run.
pub struct Loader {
    lua: Lua,
    /// The `package.loaded` keys the standard library occupies, captured before any config has
    /// run -- the allowlist [`Loader::forget_config_modules`] subtracts from. Captured rather than
    /// hardcoded so a change to [`config_stdlib`] cannot leave a name behind to be evicted as if a
    /// config had loaded it.
    standard_modules: std::collections::HashSet<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum LoaderError {
    /// `shell.lua` failed to parse or raised a runtime error while evaluating.
    #[error("shell.lua failed to evaluate: {0}")]
    Eval(#[from] mlua::Error),
    /// The script evaluated cleanly, but its top-level return wasn't a `panel` node or a
    /// non-empty array of `panel` nodes (§ 6.1).
    #[error("shell.lua's top-level return must be a `panel` node or an array of them: {0}")]
    InvalidTopLevelReturn(String),
    /// [`Loader::evaluate_file`] couldn't read `shell.lua` off disk (missing file, permissions).
    #[error("failed to read shell.lua: {0}")]
    Io(#[from] std::io::Error),
    /// A surface evaluated cleanly and had a valid top-level shape, but one of its topology
    /// fields (`id`/`layer`/`anchor`/`monitor`, § 6.1) didn't type-check. Distinct from
    /// [`Self::InvalidTopLevelReturn`], whose fixed message is about the *shape* of the top-level
    /// return, which is wrong for a field-level error inside an otherwise-valid surface.
    #[error("shell.lua's surface topology is invalid: {0}")]
    InvalidTopology(String),
}

impl From<nodes::DeserializeError> for LoaderError {
    fn from(err: nodes::DeserializeError) -> Self {
        LoaderError::InvalidTopLevelReturn(err.to_string())
    }
}

/// What one `Loader::evaluate` call produces: the top-level `panel` node(s), each still
/// carrying its own topology fields (`id`/`layer`/`anchor`/`monitor`/`exclusive`, § 6.1) directly
/// in its `properties` bag -- readable without walking into `child`. This is the cheap-to-diff
/// output the Watcher compares across reloads.
#[derive(Debug)]
pub struct LoadOutput {
    pub surfaces: Vec<VirtualNode>,
}

impl Loader {
    /// `dirty` is the generation's one scene-dirty flag (ADR-0044 decision 2), threaded through to
    /// the `state(name, initial)` global so a config's own `:set()` marks the same flag every
    /// capability push does.
    ///
    /// `config_dir` is where `require` looks and nowhere else ([`point_package_path_at`]) -- the
    /// directory holding the `shell.lua` this loader will evaluate.
    pub fn new(dirty: signal::DirtyFlag, config_dir: &std::path::Path) -> mlua::Result<Self> {
        let lua = Lua::new_with(config_stdlib(), mlua::LuaOptions::default())?;
        restrict_os(&lua)?;
        point_package_path_at(&lua, config_dir)?;
        nodes::register_node_constructors(&lua)?;
        json::register(&lua)?;
        signal::register(&lua, dirty)?;
        let standard_modules = loaded_module_names(&lua)?;
        Ok(Loader { lua, standard_modules })
    }

    /// Evaluates `source` directly, under a generic `shell.lua` chunk name. Test-only: production
    /// always has a real path to name, and a test fixture never does.
    #[cfg(test)]
    pub fn evaluate(&self, source: &str) -> Result<LoadOutput, LoaderError> {
        self.evaluate_named(source, "shell.lua")
    }

    /// Reads `path` and evaluates it exactly like [`Self::evaluate`] -- the real `shell.lua`
    /// entry point.
    pub fn evaluate_file(&self, path: &std::path::Path) -> Result<LoadOutput, LoaderError> {
        let source = std::fs::read_to_string(path)?;
        self.evaluate_named(&source, &path.display().to_string())
    }

    /// `name` is the chunk name Lua prefixes onto every error raised out of `source`, so it is
    /// what a config author reads when their edit is rejected. Without it mlua names the chunk
    /// after *this* Rust call site: a live session reported a typo in `shell.lua` as
    /// "renderer/src/lua/mod.rs:74:127", pointing a reader at the engine's source for a line
    /// number that was theirs all along.
    ///
    /// The leading `@` is Lua's own marker for "this name is a file path" (`lua_Debug.source`),
    /// and it is load-bearing: without it Lua treats the name as inline source text and renders
    /// it as `[string "/mnt/Work/0Coding/1Rust/oblisk-shell/dev-conf..."]`, truncating the path
    /// right where the filename would be.
    fn evaluate_named(&self, source: &str, name: &str) -> Result<LoadOutput, LoaderError> {
        self.forget_config_modules()?;
        let value: Value = self.lua.load(source).set_name(format!("@{name}")).eval()?;
        Ok(LoadOutput { surfaces: collect_surfaces(value)? })
    }

    /// Creates a fresh, empty Lua table on this `Loader`'s own VM -- lets a caller build an
    /// initial value for [`signal::Signal::new_live`] without reaching into a private `Lua` field.
    pub fn create_table(&self) -> mlua::Result<Table> {
        self.lua.create_table()
    }

    /// The `Lua` state itself, for a caller that needs to resolve a `Signal` (ADR-0044 decision
    /// 1): `layout::Scene::apply` needs this to reach `signal::Signal::get_value`, which takes
    /// `&Lua` rather than recovering one from `self`.
    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// Registers `value` as a global Lua name, visible to every later `evaluate` call on this
    /// `Loader` -- the same mechanism the node constructors and `computed` use internally at
    /// [`Loader::new`] time, exposed for a caller outside this module.
    pub fn set_global<T: mlua::IntoLua>(&self, name: &str, value: T) -> mlua::Result<()> {
        self.lua.globals().set(name, value)
    }

    /// Registers the `process` global table (`process.run`/`ProcessHandle:kill()`) onto this
    /// `Loader`'s own VM -- needed here because registering a whole table-with-a-closure can't go
    /// through [`Self::set_global`]/[`Self::create_table`] alone.
    pub fn register_process(&self, registry: process::ProcessRegistry) -> mlua::Result<()> {
        process::register(&self.lua, registry)
    }

    /// Converts a JSON value into the equivalent Lua value, on this `Loader`'s own `Lua` state (a
    /// `Value` is tied to the state that created it). Turns a pushed `StateSnapshot`'s
    /// `serde_json::Value` payload into something a `LiveSignalHandle::set` call can store.
    ///
    /// Delegates to [`json::to_lua`], which is also what `json.decode` calls, so a config meets
    /// one `null` mapping everywhere rather than a different one per source.
    pub fn to_lua_value(&self, json: &serde_json::Value) -> mlua::Result<Value> {
        json::to_lua(&self.lua, json)
    }

    /// Drops every module a config's own `require` put in `package.loaded`, leaving the standard
    /// library alone (docs/adr/0047 decision 2).
    ///
    /// ADR-0044 decision 4 keeps one VM per generation and does not reset it on an in-place
    /// reload; `require` caches by module name. Together they mean an edited `widgets/clock.lua`
    /// would be re-required from cache, so `shell.lua` re-runs against the old copy and the
    /// screen does not change -- a reload that silently did nothing.
    ///
    /// Names are collected before any are removed, rather than cleared during the walk: mlua's
    /// iterator holds the table for the length of the traversal, and a config with two modules
    /// would otherwise be mutating what it is reading.
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

/// Every name currently in `package.loaded`. Called once, at the end of [`Loader::new`], when that
/// is exactly the standard library.
fn loaded_module_names(lua: &Lua) -> mlua::Result<std::collections::HashSet<String>> {
    lua.globals()
        .get::<Table>("package")?
        .get::<Table>("loaded")?
        .pairs::<String, Value>()
        .map(|pair| pair.map(|(name, _)| name))
        .collect()
}

/// Appended to the "surface N is a X" error, because a config author cannot see this cause by
/// reading their own file. Lua 5.4's `require` returns two values, the module and the loader data
/// (its file path), where 5.3 returned one. A call in the last position of a table constructor
/// expands to all of its values, so the natural entry point for a split config, `return {
/// require(a), require(b) }`, is a three-element list whose last element is a string. Found by
/// running `dev-config/oblisk/shell.lua` after it was split across 32 files.
const REQUIRE_RETURNS_TWO_VALUES: &str = ". If that element came from a `require` in the last \
position of this table, note that Lua 5.4's `require` returns the module *and* its file path, and a \
call in last position expands to both: bind it to a local first";

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

    // Read each element as a `Value` and type-check it here rather than letting
    // `sequence_values::<Table>()` convert: mlua's own failure, "error converting Lua string to
    // table", names neither the element nor what it held, and arrives as `LoaderError::Eval` even
    // though the file evaluated fine and returned the wrong thing.
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
    if surfaces.is_empty() {
        return Err(LoaderError::InvalidTopLevelReturn("the returned table has no `kind` field and no array elements".to_string()));
    }
    Ok(surfaces)
}

/// § 6's roles, at the root where § 6 puts them -- all four (docs/adr/0040 decision 1).
///
/// `lock` is admitted here even though its Wayland object's lifetime is the lock's, not the
/// config's: docs/adr/0052 decision 2 treats "where the declaration lives" and "when the Wayland
/// object exists" as separate questions, the same split docs/adr/0049 already made for `window`
/// (admitted here, owns no `xdg_toplevel` until `visible` resolves true) and `popup`. A `lock`
/// owns no `ext_session_lock_surface_v1` until the compositor sends `locked`; refusing it at the
/// root would leave § 6.4's `child` -- the whole authored lock screen -- with no legal place to
/// be written.
fn require_surface(node: &VirtualNode) -> Result<(), LoaderError> {
    match node.kind.as_str() {
        "panel" | "window" | "popup" | "lock" => Ok(()),
        other => Err(LoaderError::InvalidTopLevelReturn(format!("top-level node must be `panel`, `window`, `popup` or `lock`, got `{other}`"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A loader for the tests that never `require` anything, where the config directory only has
    /// to be a path rather than a populated tree.
    fn test_loader() -> Loader {
        Loader::new(signal::DirtyFlag::new(), &std::env::temp_dir()).unwrap()
    }

    /// docs/adr/0047 decision 1.
    #[test]
    fn require_resolves_a_module_inside_the_config_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("widgets")).unwrap();
        std::fs::write(dir.path().join("widgets/clock.lua"), r#"return { label = "tick" }"#).unwrap();
        let loader = Loader::new(signal::DirtyFlag::new(), dir.path()).unwrap();

        let label: String = loader.lua().load(r#"return require("widgets.clock").label"#).eval().unwrap();
        assert_eq!(label, "tick");
    }

    /// The `?/init.lua` half: `require "widgets"` resolves a directory, not just a file.
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

    /// docs/adr/0047 decision 2, the one place ADR-0044 and ADR-0047 interact: without a module
    /// cache clear, an edit to a required module would re-run `shell.lua` against the stale
    /// cached copy, a reload that silently does nothing.
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
        assert_eq!(accent(&second), "second", "the re-evaluation ran against the cached module, so the edit did nothing");
    }

    /// The other half: clearing has to stop at the config's own modules, or `require "string"`
    /// returning nil would break `string.format`.
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

    /// Against the config this repo actually ships rather than a fixture:
    /// `dev-config/oblisk/shell.lua` is split across directories and reaches them through
    /// `require`. Dotted names, not a bare one: `config.theme` only resolves if `?` is
    /// substituted into a path with a directory component, which a flat fixture never exercises.
    #[test]
    fn require_resolves_the_nested_modules_the_shipped_dev_config_actually_splits_out() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/oblisk");
        let loader = Loader::new(signal::DirtyFlag::new(), &dir).unwrap();

        let accent: String = loader.lua().load(r#"return require("config.theme").ACCENT"#).eval().unwrap();
        assert!(accent.starts_with('#') && accent.len() == 9, "the palette entry has to be a #rrggbbaa string, got `{accent}`");

        // Also proves a module can `require` a module of its own: `components.pill` reads
        // `config.theme` before it returns.
        let is_builder: bool = loader.lua().load(r#"return type(require("components.pill")) == "function""#).eval().unwrap();
        assert!(is_builder, "a component has to come back as the builder it returns");
    }

    /// A dev-config loader that can `require` the shipped `components/`, for the component tests
    /// below. Distinct from `test_loader()`: those never `require` anything, and a bare temp
    /// directory has no `components/panel_card.lua` to find.
    fn dev_config_loader() -> Loader {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/oblisk");
        Loader::new(signal::DirtyFlag::new(), &dir).unwrap()
    }

    fn child_table(output: &LoadOutput) -> Table {
        let Value::Table(child) = output.surfaces[0].properties.get("child").unwrap() else {
            panic!("surface's `child` did not come back as a table");
        };
        child.clone()
    }

    #[test]
    fn panel_card_wraps_its_children_in_a_column_with_the_popup_shapes_defaults() {
        let loader = dev_config_loader();
        let output = loader
            .evaluate(r#"local panel_card = require("components.panel_card") return panel { id = "p", child = panel_card({ text { content = "x" } }) }"#)
            .unwrap();
        let card = child_table(&output);
        assert_eq!(card.get::<String>("kind").unwrap(), "column");
        assert_eq!(card.get::<i64>("radius").unwrap(), 10);
        assert_eq!(card.get::<i64>("spacing").unwrap(), 6);
        let children: Table = card.get("children").unwrap();
        assert_eq!(children.raw_len(), 1);
    }

    /// `modules/bar/panels/settings.lua`'s own reason for `panel_card`: its window body wants
    /// `radius = 0` and its own padding, not the popup defaults above.
    #[test]
    fn panel_card_lets_a_caller_override_every_default() {
        let loader = dev_config_loader();
        let output = loader
            .evaluate(
                r##"local panel_card = require("components.panel_card")
                   return panel { id = "p", child = panel_card({}, { radius = 0, spacing = 2, background = "#000000ff" }) }"##,
            )
            .unwrap();
        let card = child_table(&output);
        assert_eq!(card.get::<i64>("radius").unwrap(), 0);
        assert_eq!(card.get::<i64>("spacing").unwrap(), 2);
        assert_eq!(card.get::<String>("background").unwrap(), "#000000ff");
    }

    /// `on_click` fires for right and middle clicks too (§ 5.2 item 6), so a close button has to
    /// filter to a left click itself the same way `modules/bar/indicators/session.lua`'s lock
    /// button does -- a right click landing on a settings window's close button must not close it.
    #[test]
    fn panel_header_calls_on_close_for_a_left_click_and_nothing_else() {
        use mlua::Function;
        let loader = dev_config_loader();
        let output = loader
            .evaluate(
                r#"local panel_header = require("components.panel_header")
                   closed = false
                   return panel { id = "p", child = panel_header("hi", function() closed = true end) }"#,
            )
            .unwrap();
        let header = child_table(&output);
        assert_eq!(header.get::<String>("kind").unwrap(), "row");
        let children: Table = header.get("children").unwrap();
        let title: Table = children.get(1).unwrap();
        assert_eq!(title.get::<String>("kind").unwrap(), "text");
        assert_eq!(title.get::<String>("content").unwrap(), "hi");
        let close_button: Table = children.get(2).unwrap();
        assert_eq!(close_button.get::<String>("kind").unwrap(), "button");

        let on_click: Function = close_button.get("on_click").unwrap();
        let rect = loader.lua().create_table().unwrap();
        on_click.call::<()>((rect.clone(), "right")).unwrap();
        let closed: bool = loader.lua().globals().get("closed").unwrap();
        assert!(!closed, "a right click on the close button must not call on_close");

        on_click.call::<()>((rect, "left")).unwrap();
        let closed: bool = loader.lua().globals().get("closed").unwrap();
        assert!(closed, "a left click on the close button has to call on_close");
    }

    /// The behavioral half of `components/toggle.lua`: a click reads the signal's current value
    /// through `read`, flips it, and hands the flip to `on_change` -- never the signal directly,
    /// so a capability-backed toggle (`oblisk.bluetooth`) and a `state()`-backed one both work.
    #[test]
    fn toggle_reads_the_current_value_through_read_and_flips_it_into_on_change() {
        use mlua::Function;
        let loader = dev_config_loader();
        let output = loader
            .evaluate(
                r#"local toggle = require("components.toggle")
                   local on = state("on", false)
                   last_change = nil
                   return panel { id = "p", child = toggle(on, function(v) return v end, function(new_value)
                       last_change = new_value
                       on:set(new_value)
                   end) }"#,
            )
            .unwrap();
        let track = child_table(&output);
        assert_eq!(track.get::<String>("kind").unwrap(), "button");
        assert_eq!(track.get::<i64>("width").unwrap(), 34);
        assert_eq!(track.get::<i64>("height").unwrap(), 18);

        let on_click: Function = track.get("on_click").unwrap();
        let rect = loader.lua().create_table().unwrap();
        on_click.call::<()>((rect.clone(), "left")).unwrap();
        let after_first: bool = loader.lua().globals().get("last_change").unwrap();
        assert!(after_first, "a click on an off toggle has to flip on_change to true");

        on_click.call::<()>((rect, "left")).unwrap();
        let after_second: bool = loader.lua().globals().get("last_change").unwrap();
        assert!(!after_second, "the next click has to read the new value back, not the one it started with");
    }

    #[test]
    fn toggle_ignores_a_right_or_middle_click() {
        use mlua::Function;
        let loader = dev_config_loader();
        let output = loader
            .evaluate(
                r#"local toggle = require("components.toggle")
                   local on = state("on", false)
                   changes = 0
                   return panel { id = "p", child = toggle(on, function(v) return v end, function(new_value)
                       changes = changes + 1
                       on:set(new_value)
                   end) }"#,
            )
            .unwrap();
        let track = child_table(&output);
        let on_click: Function = track.get("on_click").unwrap();
        let rect = loader.lua().create_table().unwrap();
        on_click.call::<()>((rect.clone(), "right")).unwrap();
        on_click.call::<()>((rect, "middle")).unwrap();
        let changes: i64 = loader.lua().globals().get("changes").unwrap();
        assert_eq!(changes, 0, "only a left click may flip a toggle");
    }

    /// `modules/bar/panels/settings.lua`'s `bluetooth.enabled` row: a label beside a
    /// `components/toggle.lua`, both driven by the same signal/read/on_change triple.
    #[test]
    fn panel_toggle_card_pairs_a_label_with_a_toggle_bound_to_the_same_signal() {
        let loader = dev_config_loader();
        let output = loader
            .evaluate(
                r#"local card = require("components.panel_toggle_card")
                   local on = state("on", true)
                   return panel { id = "p", child = card("enabled", on, function(v) return v end, function(new_value)
                       on:set(new_value)
                   end) }"#,
            )
            .unwrap();
        let row = child_table(&output);
        assert_eq!(row.get::<String>("kind").unwrap(), "row");
        let children: Table = row.get("children").unwrap();
        let label: Table = children.get(1).unwrap();
        assert_eq!(label.get::<String>("kind").unwrap(), "text");
        assert_eq!(label.get::<String>("content").unwrap(), "enabled");
        let toggle_node: Table = children.get(2).unwrap();
        assert_eq!(toggle_node.get::<String>("kind").unwrap(), "button");
    }

    /// `modules/bar/indicators/sys_tray.lua`'s tray count: a badge is a filled `row` around
    /// whatever `content` (a literal string or a `Signal`) its caller hands it.
    #[test]
    fn badge_wraps_content_in_a_small_filled_row() {
        let loader = dev_config_loader();
        let output = loader.evaluate(r#"local badge = require("components.badge") return panel { id = "p", child = badge("3") }"#).unwrap();
        let row = child_table(&output);
        assert_eq!(row.get::<String>("kind").unwrap(), "row");
        assert_eq!(row.get::<i64>("height").unwrap(), 16);
        let children: Table = row.get("children").unwrap();
        let text_node: Table = children.get(1).unwrap();
        assert_eq!(text_node.get::<String>("kind").unwrap(), "text");
        assert_eq!(text_node.get::<String>("content").unwrap(), "3");
    }

    /// `forget_config_modules` runs before every evaluation and reads `package.loaded`, so a
    /// config that removes `package` reaches into the *next* reload rather than only its own. The
    /// answer must be a reported error, not a silent no-op.
    #[test]
    fn a_config_that_deletes_the_package_table_fails_its_next_reload_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let loader = Loader::new(signal::DirtyFlag::new(), dir.path()).unwrap();
        loader.evaluate(r#"package = nil return panel { id = "bar", layer = "Top" }"#).unwrap();

        let second = loader.evaluate(r#"return panel { id = "bar", layer = "Top" }"#);
        assert!(matches!(second, Err(LoaderError::Eval(_))), "the next reload has to report, not quietly skip the cache clear: {second:?}");
    }

    /// The contract every split config rests on: a required module runs in the same VM as
    /// `shell.lua`, so it sees the node constructors and the engine's globals without being
    /// handed them. Nothing in the engine would complain if this quietly stopped being true.
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
            .evaluate(r#"local w = require("widget") return panel { id = "bar", layer = "Top", has_state = w.has_state, has_json = w.has_json, child = w.node }"#)
            .unwrap();
        assert_eq!(output.surfaces[0].properties.get("has_state").unwrap(), &Value::Boolean(true));
        assert_eq!(output.surfaces[0].properties.get("has_json").unwrap(), &Value::Boolean(true));
        assert!(output.surfaces[0].properties.contains_key("child"), "a node built in a required module has to survive into the tree");
    }

    /// Found by running the shipped config: the surface list held a seventh element that was a
    /// file path string, and the engine reported `shell.lua failed to evaluate: error converting
    /// Lua string to table` -- naming neither the element nor what it was, and blaming evaluation
    /// for a problem in the returned value. Not a typo a config author would spot: Lua 5.4's
    /// `require` returns two values, and a call in the last position of a table constructor
    /// expands to both, so `return { require(a), require(b) }` naturally becomes a three-element
    /// list whose last element is a string.
    #[test]
    fn a_non_node_in_the_surface_list_names_which_element_and_what_it_was() {
        let loader = test_loader();
        let err = loader
            .evaluate(r#"return { panel { id = "bar", layer = "Top" }, "/home/me/.config/oblisk/modules/global/lock.lua" }"#)
            .unwrap_err();

        assert!(matches!(err, LoaderError::InvalidTopLevelReturn(_)), "a bad element is a bad return, not an evaluation failure: {err:?}");
        let message = err.to_string();
        assert!(message.contains("surface 2"), "the message has to say which element: {message}");
        assert!(message.contains("string"), "the message has to say what it got: {message}");
        assert!(message.contains("require"), "the message has to name the cause a config author cannot see: {message}");
    }

    /// Whether `expr` evaluates to `nil` in a config's own environment.
    fn absent(loader: &Loader, expr: &str) -> bool {
        loader.lua().load(format!("return ({expr}) == nil")).eval().unwrap()
    }

    /// docs/adr/0048: each of these blocks the Wayland dispatch thread, and the 5ms CPU cap can't
    /// catch it since a thread parked in a syscall executes no instructions.
    #[test]
    fn the_config_vm_has_no_blocking_stdlib_call_left_to_stall_wayland_dispatch() {
        let loader = test_loader();
        assert!(absent(&loader, "io"), "the whole of `io` goes, io.open and io.popen included");
        for call in ["os.execute", "os.exit", "os.remove", "os.rename", "os.tmpname", "os.setlocale"] {
            assert!(absent(&loader, call), "`{call}` blocks or kills the render thread and has no business in a config");
        }
    }

    /// The four ADR-0048 keeps. Cutting them would break the clock every bar has.
    #[test]
    fn the_config_vm_keeps_the_four_os_calls_that_cannot_block() {
        let loader = test_loader();
        for call in ["os.time", "os.date", "os.clock", "os.getenv"] {
            assert!(!absent(&loader, call), "`{call}` cannot block and a config needs it");
        }
        let year: i64 = loader.lua().load(r#"return tonumber(os.date("%Y"))"#).eval().unwrap();
        assert!(year >= 2024, "os.date has to be the real one, not a stub: got {year}");
    }

    /// The hole a denylist leaves: `require` reads `package.loaded`, which keeps its own reference
    /// to the loaded table, so removing a global alone hands the full library back to `local io =
    /// require("io")`.
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

    #[test]
    fn evaluate_accepts_a_single_top_level_surface() {
        let loader = test_loader();
        let output = loader.evaluate(r#"return panel { id = "bar", layer = "Top" }"#).unwrap();
        assert_eq!(output.surfaces.len(), 1);
        assert_eq!(output.surfaces[0].kind, "panel");
    }

    #[test]
    fn evaluate_accepts_a_window_and_a_popup_at_the_top_level_beside_a_panel() {
        // docs/adr/0040 decision 1: all four § 6 roles are a config's to declare at the root.
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
        // docs/adr/0052 decision 2: the compositor still decides when the lock surface exists;
        // this function only ever decided where the declaration may be written.
        let loader = test_loader();
        let output = loader.evaluate(r##"return lock { id = "screen", child = rect { background = "#000000FF" } }"##).unwrap();
        assert_eq!(output.surfaces.len(), 1);
        assert_eq!(output.surfaces[0].kind, "lock");
    }

    #[test]
    fn a_top_level_return_of_an_unknown_kind_names_all_four_roles_it_could_have_been() {
        // The catch-all is the only place a config author learns the roster, so it must list
        // `lock` -- an omission here reads as "not built yet".
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

        let output = loader.evaluate(r#"return panel { id = "bar", layer = "Top", reading = audio:get() }"#).unwrap();
        assert_eq!(output.surfaces[0].properties.get("reading").unwrap().as_integer().unwrap(), 7);

        handle.set(Value::Integer(9));
        let output = loader.evaluate(r#"return panel { id = "bar", layer = "Top", reading = audio:get() }"#).unwrap();
        assert_eq!(output.surfaces[0].properties.get("reading").unwrap().as_integer().unwrap(), 9);
    }

    #[test]
    fn to_lua_value_converts_a_json_object_a_script_can_read_fields_from() {
        let loader = test_loader();
        let json = serde_json::json!({ "volume": 0.5, "muted": false });
        let value = loader.to_lua_value(&json).unwrap();
        loader.set_global("state", value).unwrap();

        let output = loader.evaluate(r#"return panel { id = "bar", layer = "Top", volume = state.volume, muted = state.muted }"#).unwrap();
        assert_eq!(output.surfaces[0].properties.get("volume").unwrap().as_f64().unwrap(), 0.5);
        assert_eq!(output.surfaces[0].properties.get("muted").unwrap(), &Value::Boolean(false));
    }

    /// A JSON `null` must reach Lua as `nil`, erasing the key rather than leaving a
    /// typed-but-absent value. Checking only `item.icon_path == nil` wouldn't catch this --
    /// indexing a missing key also returns `nil` -- so this counts the table's own keys to prove
    /// `icon_path` never landed in it at all.
    #[test]
    fn to_lua_value_maps_a_json_null_field_to_a_nil_that_is_absent_from_the_table() {
        let loader = test_loader();
        let json = serde_json::json!({
            "icon_name": "org.telegram.desktop-mute-symbolic",
            "icon_path": null,
        });
        let value = loader.to_lua_value(&json).unwrap();
        loader.set_global("item", value).unwrap();

        let output = loader
            .evaluate(
                r#"
                local key_count = 0
                for _ in pairs(item) do key_count = key_count + 1 end
                return panel {
                    id = "bar", layer = "Top",
                    key_count = key_count,
                    path_is_nil = item.icon_path == nil,
                }
                "#,
            )
            .unwrap();
        assert_eq!(output.surfaces[0].properties.get("key_count").unwrap().as_integer().unwrap(), 1);
        assert_eq!(output.surfaces[0].properties.get("path_is_nil").unwrap(), &Value::Boolean(true));
    }

    /// The actual bug: mlua's default `serialize_none_to_null` maps JSON `null` to a
    /// lightuserdata sentinel, and lightuserdata is truthy in Lua, so a live Telegram tray item's
    /// `icon_path = null` made `if item.icon_path then` take the branch that assumes a real path.
    /// Checking the Lua *type* of the converted value would still pass for that sentinel; only a
    /// truthiness check like this one distinguishes it from real `nil`.
    #[test]
    fn to_lua_value_a_null_field_is_falsy_not_a_truthy_lightuserdata_sentinel() {
        let loader = test_loader();
        let json = serde_json::json!({ "icon_path": null });
        let value = loader.to_lua_value(&json).unwrap();
        loader.set_global("payload", value).unwrap();

        let result: String = loader
            .lua()
            .load(r#"if payload.icon_path then return "truthy" else return "falsy" end"#)
            .eval()
            .unwrap();
        assert_eq!(result, "falsy");
    }

    /// The live tray payload's nulls sit inside `items[1]`, not at the top level. Catches a fix
    /// that only handles a top-level null and still leaves the truthy sentinel one table level
    /// down, which is where the real bug lived.
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

    /// The trade this fix accepts: mapping `null` to `nil` leaves a hole when the null is an
    /// array *element*, and `ipairs` stops at a hole, while `#` and direct indexing see past it.
    /// No capability payload produces a null array element today (their nulls are all optional
    /// *fields*), but `dev-config/oblisk/shell.lua` iterates capability lists with `ipairs`.
    #[test]
    fn to_lua_value_a_null_array_element_leaves_a_hole_ipairs_stops_at() {
        let loader = test_loader();
        let json = serde_json::json!({ "xs": [1, null, 3] });
        let value = loader.to_lua_value(&json).unwrap();
        loader.set_global("state", value).unwrap();

        let ipairs_count: i64 = loader.lua().load("local n = 0 for _ in ipairs(state.xs) do n = n + 1 end return n").eval().unwrap();
        assert_eq!(ipairs_count, 1, "ipairs must stop at the hole the nil leaves");
        // Reachable past the hole by index: a hole, not a truncation.
        let third: i64 = loader.lua().load("return state.xs[3]").eval().unwrap();
        assert_eq!(third, 3);
    }

    /// Negative control: turning off `serialize_none_to_null`/`serialize_unit_to_null` should
    /// only change how `Value::Null` maps, not ordinary numbers, strings, booleans or arrays.
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

        let output = loader
            .evaluate(
                r#"return panel {
                    id = "bar", layer = "Top",
                    volume = state.volume, muted = state.muted, label = state.label, second_tag = state.tags[2],
                }"#,
            )
            .unwrap();
        assert_eq!(output.surfaces[0].properties.get("volume").unwrap().as_f64().unwrap(), 0.5);
        assert_eq!(output.surfaces[0].properties.get("muted").unwrap(), &Value::Boolean(false));
        assert_eq!(
            output.surfaces[0].properties.get("label").unwrap().as_string().unwrap().to_string_lossy(),
            "media"
        );
        assert_eq!(
            output.surfaces[0].properties.get("second_tag").unwrap().as_string().unwrap().to_string_lossy(),
            "b"
        );
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

    /// A config author reads this string and nothing else when their edit is rejected, so it has
    /// to name their file and their line, not "renderer/src/lua/mod.rs:74:127".
    #[test]
    fn an_evaluation_error_names_the_config_file_not_this_source_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shell.lua");
        std::fs::write(&path, "return panel { id = \"bar\" }\nthis is not lua\n").unwrap();

        let loader = test_loader();
        let message = loader.evaluate_file(&path).unwrap_err().to_string();

        assert!(message.contains(&path.display().to_string()), "expected the config path in: {message}");
        assert!(!message.contains("lua/mod.rs"), "expected no engine source path in: {message}");
        // Not `[string "/long/path/to/shell..."]` -- Lua truncates a non-`@` chunk name, and a
        // truncated absolute path loses the filename, which is the part worth printing.
        assert!(!message.contains("[string"), "expected a file-named chunk in: {message}");
        // The line the author has to go and fix, not just the file.
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
