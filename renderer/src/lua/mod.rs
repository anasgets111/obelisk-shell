//! Lua VM bootstrap and the loader (build-steps.md Phase 10; `CONTEXT.md`, Loader): evaluates
//! `shell.lua` into the top-level `panel` node(s) and their topology, reused for both a
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
pub mod capability;
pub mod marshal;
pub mod json;
pub mod nodes;
pub mod process;
pub mod signal;

pub use nodes::VirtualNode;

use mlua::{Lua, Table, Value};

/// What a config's VM loads, and the reason it is spelled out rather than left as mlua's
/// `StdLib::ALL_SAFE` (docs/adr/0048). `ALL_SAFE` is everything but `debug` and `ffi`, which leaves
/// `io` and `os` whole. ADR-0039 put the Lua VM on the Wayland thread, so `io.read` or
/// `os.execute` in a `computed` freezes every surface on every monitor until it returns, and
/// ADR-0021's 5ms cap does not catch it: the cap is an instruction-count hook and a thread parked
/// in a syscall executes no instructions.
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
/// An allowlist rather than deleting the dangerous keys, so a call added to the library by a future
/// Lua or mlua is excluded by default instead of arriving unnoticed. Copying the real functions
/// rather than reimplementing them keeps `os.date`'s whole strftime surface exact; a hand-written
/// substitute would drift from what a config author reads in the Lua manual.
///
/// `package.loaded` gets the same table, and that is the load-bearing half. `require` answers out
/// of `package.loaded`, which holds its own reference to the table a library was loaded into, so
/// replacing the global alone would hand the full library straight back to `local os =
/// require("os")`. This matters for `os` and only `os`: `io` is never in [`config_stdlib`], so it
/// has no `package.loaded` entry to reclaim. Measured under the old `ALL_SAFE` VM, where both were
/// loaded, `require("io")` did return a working `io.open`, which is what named the hole.
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
/// Replacing Lua's compiled-in default rather than prepending to it. Measured before this existed,
/// the default was `/usr/local/share/lua/5.4/?.lua;...;./?.lua;./?/init.lua`, and both halves are
/// wrong here. The system entries let a same-named module installed system wide shadow the config's
/// own. The `./` entries resolve against the process's working directory, which nothing in the
/// Supervisor sets, so a split config worked when started from its own directory and failed from
/// anywhere else: the failure mode that passes every test run by hand and breaks under systemd.
///
/// Installed Lua libraries become unreachable, which is the right default for a shell config. The
/// upgrade if someone genuinely wants luarocks is a user-declared path list appended here, not a
/// silent restoration of the system default.
///
/// `package.cpath` is deliberately left alone. mlua's safe mode already replaces the C searchers
/// and makes `package.loadlib` raise, so a stale `cpath` loads nothing; ADR-0047 records this so
/// nobody reaches for `Lua::unsafe_new` to fix an unrelated problem, since loading a `.so` into the
/// Renderer would discard the memory-safety argument the whole process boundary exists for.
///
/// Three limits with no fix at this boundary, because `package.path` is a plain Lua string with no
/// escape syntax at all. A config directory whose path is not valid UTF-8 is substituted lossily. A
/// `;` in it reads as the separator between two search entries, splitting the path into two wrong
/// ones. A `?` in it is a substitution marker, and Lua replaces *every* `?` in an entry with the
/// module name, not only the trailing one, so the search runs against a corrupted directory rather
/// than failing to find anything. All three are silent. None is expressible in `package.path`, so
/// the only real fix is rejecting such a directory at startup, which is not worth building for a
/// path that is `$XDG_CONFIG_HOME/oblisk` or `~/.config/oblisk`.
fn point_package_path_at(lua: &Lua, config_dir: &std::path::Path) -> mlua::Result<()> {
    let dir = config_dir.display();
    lua.globals().get::<Table>("package")?.set("path", format!("{dir}/?.lua;{dir}/?/init.lua"))
}

/// Owns the Lua VM for one generation. `Loader::evaluate` is stateless across calls beyond that
/// -- each call is a fresh evaluation of its `source` argument, not an incremental re-run.
pub struct Loader {
    lua: Lua,
    /// The `package.loaded` keys the standard library occupies, captured before any config has run.
    /// Everything else in that table got there through a config's own `require`, which is what makes
    /// this the allowlist [`Loader::forget_config_modules`] subtracts from. Captured rather than
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

/// What one `Loader::evaluate` call produces: the top-level `panel` node(s), each still
/// carrying its own topology fields (`id`/`layer`/`anchor`/`monitor`/`exclusive`, § 6.1) directly
/// in its `properties` bag -- readable without walking into `child` (see `nodes.rs`'s doc
/// comment). This is the cheap-to-diff output Phase 13's Watcher will compare across reloads.
#[derive(Debug)]
pub struct LoadOutput {
    pub surfaces: Vec<VirtualNode>,
}

impl Loader {
    /// `dirty` is the generation's one scene-dirty flag (ADR-0044 decision 2), threaded through to
    /// the `state(name, initial)` global so a config's own `:set()` marks the same flag every
    /// capability push does. `renderer/src/socket.rs`'s `RendererClient::start` creates it before
    /// the loader for exactly this reason; see `signal::register`.
    ///
    /// `config_dir` is where `require` looks and nowhere else (see [`point_package_path_at`]). It
    /// is the directory holding the `shell.lua` this loader will evaluate, so the two are passed
    /// from one place rather than each resolving the environment separately.
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

    /// Evaluates `source` directly, under a generic `shell.lua` chunk name. Test-only since
    /// `evaluate_file` started naming the chunk after the real path: production always has a path
    /// to name, and a test fixture never does.
    #[cfg(test)]
    pub fn evaluate(&self, source: &str) -> Result<LoadOutput, LoaderError> {
        self.evaluate_named(source, "shell.lua")
    }

    /// Reads `path` and evaluates it exactly like [`Self::evaluate`] -- the real `shell.lua`
    /// entry point (build-steps.md Phase 13). See the module doc comment.
    pub fn evaluate_file(&self, path: &std::path::Path) -> Result<LoadOutput, LoaderError> {
        let source = std::fs::read_to_string(path)?;
        self.evaluate_named(&source, &path.display().to_string())
    }

    /// `name` is the chunk name Lua prefixes onto every error raised out of `source`, so it is
    /// what a config author reads when their edit is rejected. Without it mlua names the chunk
    /// after *this* Rust call site: a live session reported a typo in `shell.lua` as
    /// "renderer/src/lua/mod.rs:74:127", which points a reader at the engine's source instead of
    /// their own file for a line number (127) that was theirs all along.
    ///
    /// The leading `@` is Lua's own marker for "this name is a file path" (`lua_Debug.source`),
    /// and it is load-bearing rather than cosmetic: without it Lua treats the name as inline
    /// source text and renders it as `[string "/mnt/Work/0Coding/1Rust/oblisk-shell/dev-conf..."]`,
    /// truncating the path at 60-odd characters right where the filename would be.
    fn evaluate_named(&self, source: &str, name: &str) -> Result<LoadOutput, LoaderError> {
        self.forget_config_modules()?;
        let value: Value = self.lua.load(source).set_name(format!("@{name}")).eval()?;
        Ok(LoadOutput { surfaces: collect_surfaces(value)? })
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
    ///
    /// Delegates to [`json::to_lua`], which is also what `json.decode` calls: a config must not
    /// meet one `null` mapping on `oblisk.tray.items` and a different one on the output of
    /// `lsblk --json`, with nothing in the language to tell the two apart. See that function for
    /// what the mapping is and what it costs.
    pub fn to_lua_value(&self, json: &serde_json::Value) -> mlua::Result<Value> {
        json::to_lua(&self.lua, json)
    }

    /// Drops every module a config's own `require` put in `package.loaded`, leaving the standard
    /// library alone (docs/adr/0047 decision 2).
    ///
    /// This is the one place ADR-0044 and ADR-0047 interact, and neither is wrong on its own.
    /// ADR-0044 decision 4 keeps one VM per generation and does not reset it on an in-place reload;
    /// `require` caches by module name. Together they mean an edited `widgets/clock.lua` would be
    /// re-required from cache, so `shell.lua` re-runs against the old copy and the screen does not
    /// change. That reads as a reload that silently did nothing, which is why it is worth its own
    /// step rather than being left to be discovered.
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

/// § 6's roles, at the root where § 6 puts them -- all four of them (docs/adr/0040 decision 1),
/// since gating on `panel` alone was correct only while `panel` was the only role that existed and
/// rejected a `window` or `popup` here, before the scene or any spec parser ever saw it
/// (build-steps.md Phase 22).
///
/// `lock` had its own rejecting arm through Phase 22, on the argument that a lock surface's
/// lifetime is the lock's rather than the config's, so the root of `shell.lua` was the wrong
/// *place* to write one. docs/adr/0052 decision 2 reverses that, and the reversal is not a change
/// of mind about lifetimes: the lifetime claim was true and stays true. What the arm got wrong was
/// treating "where the declaration lives" and "when the Wayland object exists" as one question,
/// which docs/adr/0049 had already split for the two roles sitting beside it. A `window` is
/// admitted here and owns no `xdg_toplevel` until `visible` resolves true; a `lock` is admitted
/// here and owns no `ext_session_lock_surface_v1` until the compositor sends `locked`. Refusing it
/// left § 6.4's `child` -- the whole authored lock screen -- with no legal place to be written, so
/// the rejection cost the feature rather than protecting the lifetime.
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

    /// docs/adr/0047 decision 1. Before this, `package.path` was Lua's compiled-in default, so a
    /// `require` searched `/usr/local/share/lua/5.4/` and then `./`, which is whatever directory
    /// the Supervisor happened to be launched from. A config split across files worked when
    /// started from its own directory and broke from anywhere else.
    #[test]
    fn require_resolves_a_module_inside_the_config_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("widgets")).unwrap();
        std::fs::write(dir.path().join("widgets/clock.lua"), r#"return { label = "tick" }"#).unwrap();
        let loader = Loader::new(signal::DirtyFlag::new(), dir.path()).unwrap();

        let label: String = loader.lua().load(r#"return require("widgets.clock").label"#).eval().unwrap();
        assert_eq!(label, "tick");
    }

    /// The `?/init.lua` half of the same decision, which is what makes `require "widgets"` resolve
    /// a directory rather than only `require "widgets.clock"` resolving a file.
    #[test]
    fn require_resolves_a_directory_through_its_init_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("widgets")).unwrap();
        std::fs::write(dir.path().join("widgets/init.lua"), r#"return { label = "index" }"#).unwrap();
        let loader = Loader::new(signal::DirtyFlag::new(), dir.path()).unwrap();

        let label: String = loader.lua().load(r#"return require("widgets").label"#).eval().unwrap();
        assert_eq!(label, "index");
    }

    /// Replacing the default rather than prepending to it, so a same-named module installed system
    /// wide can never shadow the config's own. The `./?.lua` entry is the one that matters most:
    /// it resolves against the process's working directory, which nothing in the Supervisor sets.
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

    /// docs/adr/0047 decision 2, and the one place ADR-0044 and ADR-0047 interact. ADR-0044
    /// decision 4 keeps one VM alive across an in-place reload, and `require` caches by module name
    /// in `package.loaded`. Together those two mean an edit to a required module would re-run
    /// `shell.lua` against the stale cached copy and change nothing on screen, which looks exactly
    /// like a reload that silently did not happen.
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

    /// The other half of the same decision: clearing has to stop at the config's own modules. A
    /// `require "string"` returning nil would break `string.format`, which every config uses.
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

    /// Phase 26's acceptance shape, against the config this repo actually ships rather than a
    /// fixture: `dev-config/oblisk/shell.lua` reads its palette out of a sibling `theme.lua`
    /// through `require`. Same precedent as `crate::image`'s resvg test, which renders the real
    /// `dev-config/oblisk/wallpaper.svg`.
    #[test]
    fn require_resolves_the_module_the_shipped_dev_config_actually_splits_out() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dev-config/oblisk");
        let loader = Loader::new(signal::DirtyFlag::new(), &dir).unwrap();

        let accent: String = loader.lua().load(r#"return require("theme").ACCENT"#).eval().unwrap();
        assert!(accent.starts_with('#') && accent.len() == 9, "the palette entry has to be a #rrggbbaa string, got `{accent}`");
    }

    /// `forget_config_modules` runs before every evaluation and reads `package.loaded`, so a config
    /// that removes `package` reaches into the *next* reload rather than only its own. Pinned
    /// because a reload that stops working is the exact failure Phase 26 exists to remove, and
    /// because the answer should be a reported error rather than a silent no-op.
    #[test]
    fn a_config_that_deletes_the_package_table_fails_its_next_reload_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let loader = Loader::new(signal::DirtyFlag::new(), dir.path()).unwrap();
        loader.evaluate(r#"package = nil return panel { id = "bar", layer = "Top" }"#).unwrap();

        let second = loader.evaluate(r#"return panel { id = "bar", layer = "Top" }"#);
        assert!(matches!(second, Err(LoaderError::Eval(_))), "the next reload has to report, not quietly skip the cache clear: {second:?}");
    }

    /// Whether `expr` evaluates to `nil` in a config's own environment.
    fn absent(loader: &Loader, expr: &str) -> bool {
        loader.lua().load(format!("return ({expr}) == nil")).eval().unwrap()
    }

    /// docs/adr/0048: every one of these blocks the thread that dispatches Wayland events, and
    /// ADR-0021's 5ms cap cannot catch it, because the cap is an instruction-count hook and a
    /// thread parked in a syscall executes no instructions. One `os.execute` in a `computed`
    /// freezes every surface on every monitor until the child exits.
    #[test]
    fn the_config_vm_has_no_blocking_stdlib_call_left_to_stall_wayland_dispatch() {
        let loader = test_loader();
        assert!(absent(&loader, "io"), "the whole of `io` goes, io.open and io.popen included");
        for call in ["os.execute", "os.exit", "os.remove", "os.rename", "os.tmpname", "os.setlocale"] {
            assert!(absent(&loader, call), "`{call}` blocks or kills the render thread and has no business in a config");
        }
    }

    /// The four ADR-0048 keeps: they read process-local state and return without a syscall that
    /// waits. Cutting them would break the clock every bar has.
    #[test]
    fn the_config_vm_keeps_the_four_os_calls_that_cannot_block() {
        let loader = test_loader();
        for call in ["os.time", "os.date", "os.clock", "os.getenv"] {
            assert!(!absent(&loader, call), "`{call}` cannot block and a config needs it");
        }
        let year: i64 = loader.lua().load(r#"return tonumber(os.date("%Y"))"#).eval().unwrap();
        assert!(year >= 2024, "os.date has to be the real one, not a stub: got {year}");
    }

    /// The hole a denylist leaves. `require` reads `package.loaded`, which keeps its own reference
    /// to the table a library was loaded into, so removing a global alone hands the full library
    /// straight back to `local io = require("io")`.
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
        // § 6 returns all four roles at the root and docs/adr/0040 decision 1 makes three of them
        // a config's to declare, so gating on `panel` alone rejected two thirds of § 6 before the
        // scene ever saw them (build-steps.md Phase 22).
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
        // Replaces the Phase 22 test that asserted the opposite, per docs/adr/0052 decision 2.
        // The lifetime claim that test rested on is still true -- the compositor decides when a
        // lock surface exists -- but it is a claim about the Wayland object, and this function
        // only ever decided where the declaration may be written.
        let loader = test_loader();
        let output = loader.evaluate(r##"return lock { id = "screen", child = rect { background = "#000000FF" } }"##).unwrap();
        assert_eq!(output.surfaces.len(), 1);
        assert_eq!(output.surfaces[0].kind, "lock");
    }

    #[test]
    fn a_top_level_return_of_an_unknown_kind_names_all_four_roles_it_could_have_been() {
        // The catch-all is the only place a config author learns the roster, so it has to list
        // `lock` now that `lock` is admitted -- an omission here reads as "not built yet", which
        // is exactly the wrong impression the Phase 22 arm used to give.
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

    /// docs/build-steps.md Phase 19 item 16: a JSON `null` must reach Lua as `nil`, which erases
    /// the key from the table rather than leaving a typed-but-absent value in it. Checking only
    /// `item.icon_path == nil` would not catch this -- indexing a table for a missing key also
    /// returns `nil` in Lua -- so this counts the table's own keys to prove `icon_path` never
    /// landed in it at all.
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

    /// This is the actual bug from docs/build-steps.md Phase 19 item 16, not a stand-in for it:
    /// mlua's default `serialize_none_to_null` maps JSON `null` to a lightuserdata sentinel, and
    /// lightuserdata is truthy in Lua, so a live Telegram tray item's `icon_path = null` made
    /// `if item.icon_path then` take the branch that assumes a real path. Checking the Lua *type*
    /// of the converted value would still pass for that sentinel; only a truthiness check like
    /// this one distinguishes it from real `nil`.
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

    /// The live tray payload's nulls sit inside `items[1]`, not at the top level (a Telegram item
    /// arrived as `icon_name = "org.telegram.desktop-mute-symbolic", icon_path = null`). This
    /// catches a fix that only handles a top-level null and still leaves the truthy sentinel one
    /// table level down, which is where the real bug actually lived.
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

    /// The trade this fix accepts, pinned so it stays a known quantity. Mapping `null` to `nil`
    /// leaves a hole when the null is an array *element* rather than a field value, and `ipairs`
    /// stops at a hole, while `#` and direct indexing still see past it. The old lightuserdata
    /// sentinel filled the hole, so `ipairs` walked the whole array. No capability payload
    /// produces a null array element today (their nulls are all optional *fields*), but
    /// `dev-config/oblisk/shell.lua` does iterate capability lists with `ipairs`, so this is the
    /// shape a config would meet first if one ever did.
    #[test]
    fn to_lua_value_a_null_array_element_leaves_a_hole_ipairs_stops_at() {
        let loader = test_loader();
        let json = serde_json::json!({ "xs": [1, null, 3] });
        let value = loader.to_lua_value(&json).unwrap();
        loader.set_global("state", value).unwrap();

        let ipairs_count: i64 = loader.lua().load("local n = 0 for _ in ipairs(state.xs) do n = n + 1 end return n").eval().unwrap();
        assert_eq!(ipairs_count, 1, "ipairs must stop at the hole the nil leaves");
        // Reachable past the hole by index, which is what makes this a hole rather than a
        // truncation: a config that indexes directly still sees the third element.
        let third: i64 = loader.lua().load("return state.xs[3]").eval().unwrap();
        assert_eq!(third, 3);
    }

    /// Negative control: turning off `serialize_none_to_null`/`serialize_unit_to_null` should only
    /// change how `Value::Null` maps. This guards against the same edit disturbing how ordinary
    /// numbers, strings, booleans, and array elements round-trip.
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
    /// to name their file and their line. Before the chunk name was set, a live session reported a
    /// syntax error on line 127 of `shell.lua` as "renderer/src/lua/mod.rs:74:127", which sends
    /// the reader into the engine's source for a line number that was theirs.
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
