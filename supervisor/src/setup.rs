//! `obelisk init`: writes `.luarc.json` pointing lua-language-server at stubs, and a starter
//! `shell.lua` only when absent. It never writes the config.

use std::io;
use std::path::{Path, PathBuf};

/// Five embedded stubs. lua-language-server reads disk, not the binary; embedding lets `cargo
/// install`, which places no data files, write them to `$XDG_DATA_HOME`.
const EMBEDDED_STUBS: &[(&str, &str)] = &[
    ("globals.lua", include_str!("../../lua-meta/globals.lua")),
    ("nodes.lua", include_str!("../../lua-meta/nodes.lua")),
    ("obelisk.lua", include_str!("../../lua-meta/obelisk.lua")),
    ("signals.lua", include_str!("../../lua-meta/signals.lua")),
    ("surfaces.lua", include_str!("../../lua-meta/surfaces.lua")),
];

/// Starter file `init` writes when absent; `just lua` parses it with `luac -p` like other Lua.
const STARTER_SHELL_LUA: &str = include_str!("../../share/starter/shell.lua");

/// Embedded stubs missing from `dir` or differing from it. Compares bytes, not the version stamp:
/// stubs change without a version bump.
fn stale_stubs(dir: &Path) -> impl Iterator<Item = &'static (&'static str, &'static str)> + '_ {
    EMBEDDED_STUBS
        .iter()
        .filter(move |(name, contents)| std::fs::read(dir.join(name)).ok().as_deref() != Some(contents.as_bytes()))
}

/// Where an installed copy of the stubs lives, resolved from the running binary the same way
/// `renderer_binary_path` resolves its sibling: `$PREFIX/lib/obelisk/obelisk` implies
/// `$PREFIX/share/obelisk/lua-meta`. `None` is the normal answer for `cargo install` and `target/`.
pub fn packaged_stub_dir() -> Option<PathBuf> {
    packaged_stub_dir_from(std::env::current_exe().ok()?.parent()?)
}

/// The layout probe itself, taking the directory rather than reading `current_exe`, so the tests
/// below exercise this resolver instead of restating it.
///
/// Try `just install`'s `$PREFIX/lib/obelisk` (two levels up), then flat `$PREFIX/bin` (one up).
/// Check rather than assume; a wrong path silently falls back to embedded stubs.
fn packaged_stub_dir_from(exe_dir: &Path) -> Option<PathBuf> {
    ["../..", ".."]
        .iter()
        .map(|up| exe_dir.join(up).join("share").join("obelisk").join("lua-meta"))
        .find(|dir| dir.is_dir())
        .and_then(|dir| dir.canonicalize().ok())
}

/// Embedded-stub destination without a package: `$XDG_DATA_HOME/obelisk/lua-meta`, then
/// `$HOME/.local/share`. It stays outside the config directory; see its call site.
fn user_stub_dir() -> io::Result<PathBuf> {
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(data_home).join("obelisk").join("lua-meta"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "neither XDG_DATA_HOME nor HOME is set"))?;
    Ok(PathBuf::from(home).join(".local").join("share").join("obelisk").join("lua-meta"))
}

/// Writes `path` unless present; `force` overwrites. Reports the result.
fn write_unless_present(path: &Path, contents: &str, force: bool) -> io::Result<()> {
    if path.exists() && !force {
        println!("  kept    {} (exists)", path.display());
        return Ok(());
    }
    std::fs::write(path, contents)?;
    println!("  wrote   {}", path.display());
    Ok(())
}

/// `.luarc.json` body, pointing `workspace.library` at `stub_dir`.
///
/// `workspace.library` must be absolute: relative paths resolve under the config workspace, not
/// the stub directory, and a symlink cannot supply the unknown init-time path. `runtime.path`
/// mirrors the engine's `package.path`; `runtime.builtin` removes libraries ADR-0048 cut, so the
/// editor rejects `io.open` and `os.execute` like the VM.
///
/// The two `diagnostics` blocks matter: mismatch diagnostics default to **Hint**, so `--check` at
/// `Warning` (the `just types` and editor level) filters every stub error. Promoting them makes a
/// wrong payload property a red squiggle instead of a frozen shell.
fn luarc_json(stub_dir: &Path) -> String {
    format!(
        r#"{{
  "$schema": "https://raw.githubusercontent.com/LuaLS/vscode-lua/master/setting/schema.json",
  "runtime.version": "Lua 5.4",
  "runtime.path": ["?.lua", "?/init.lua"],
  "runtime.pathStrict": true,
  "runtime.builtin": {{
    "io": "disable",
    "debug": "disable",
    "os": "disable",
    "jit": "disable",
    "ffi": "disable"
  }},
  "workspace.library": ["{}"],
  "workspace.checkThirdParty": false,
  "diagnostics.severity": {{
    "param-type-mismatch": "Warning",
    "assign-type-mismatch": "Warning",
    "return-type-mismatch": "Warning",
    "cast-local-type": "Warning",
    "undefined-field": "Warning"
  }},
  "diagnostics.neededFileStatus": {{
    "param-type-mismatch": "Any",
    "assign-type-mismatch": "Any",
    "return-type-mismatch": "Any",
    "cast-local-type": "Any",
    "undefined-field": "Any"
  }}
}}
"#,
        stub_dir.display()
    )
}

/// `obelisk check`: evaluates the config and reports its declarations.
///
/// Re-execs the Renderer because the Supervisor has no `mlua` or loader; only real evaluation
/// catches `require` and property errors. Check and boot share this path.
pub fn check(config_dir: &Path) -> Result<String, String> {
    let renderer =
        crate::generation::renderer_binary_path().map_err(|err| format!("cannot find the renderer: {err}"))?;
    let output = std::process::Command::new(&renderer)
        .env(shared::CHECK_ENV, "1")
        .env(shared::CONFIG_ARG_ENV, config_dir)
        .output()
        .map_err(|err| format!("cannot run {}: {err}", renderer.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        Ok(stdout)
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim_end().to_string())
    }
}

pub fn run(config_dir: &Path, force: bool) -> Result<(), Box<dyn std::error::Error>> {
    run_into(config_dir, force, None)
}

/// [`run`] with an injectable stub directory: production uses `None` and [`user_stub_dir`], tests
/// use a `tempdir` to avoid `set_var`. Resolve lazily so packaged installs need neither env var.
fn run_into(config_dir: &Path, force: bool, user_stubs: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(config_dir)?;
    println!("obelisk init: {}", config_dir.display());

    // Prefer packaged stubs: the package upgrades them, while a copied schema for another engine
    // is worse than none.
    let stub_dir = match packaged_stub_dir() {
        Some(dir) => {
            // Package-managed drift is reported, not rewritten under `$PREFIX/share`.
            let stale: Vec<_> = stale_stubs(&dir).map(|(name, _)| *name).collect();
            if stale.is_empty() {
                println!("  stubs   {} (packaged)", dir.display());
            } else {
                println!(
                    "  stubs   {} (packaged, {} differ from this obelisk -- the install is inconsistent)",
                    dir.display(),
                    stale.join(", ")
                );
            }
            dir
        }
        None => {
            // Keep `$XDG_DATA_HOME/obelisk/lua-meta` outside config: `watcher.rs` reloads every
            // `.lua` below config, so stubs there would restyle a running bar on each refresh.
            let dir = match user_stubs {
                Some(dir) => dir.to_path_buf(),
                None => user_stub_dir()?,
            };
            // Rewrite any stub that differs, edits included. These files are ours, not config; an
            // old stub describes removed fields and misses new ones, worse than none.
            let stale: Vec<_> = stale_stubs(&dir).collect();
            if stale.is_empty() {
                println!("  stubs   {} (current)", dir.display());
            } else {
                std::fs::create_dir_all(&dir)?;
                for (name, contents) in stale {
                    write_unless_present(&dir.join(name), contents, true)?;
                }
            }
            dir
        }
    };

    write_unless_present(&config_dir.join(".luarc.json"), &luarc_json(&stub_dir), force)?;
    write_unless_present(&config_dir.join("shell.lua"), STARTER_SHELL_LUA, force)?;
    println!("\nOpen {} in an editor with lua-language-server.", config_dir.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // `run_into` injects the stub directory, so tests neither touch `$XDG_DATA_HOME` nor the real
    // `~/.local/share`; no environment mutex is needed.

    /// `packaged_stub_dir` fails silently when wrong: `init` falls back to embedded stubs, leaving
    /// a stale schema with no error. It was once wrong by exactly one `parent()`.
    /// Build both layouts directly so this runs in normal `cargo test`.
    #[test]
    fn both_installed_layouts_resolve_to_the_packaged_stub_directory() {
        let root = tempfile::tempdir().unwrap();
        for (exe_rel, label) in [("lib/obelisk/obelisk", "just install"), ("bin/obelisk", "flat prefix")] {
            let prefix = root.path().join(label.replace(' ', "-"));
            let exe = prefix.join(exe_rel);
            std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
            std::fs::write(&exe, b"").unwrap();
            let stubs = prefix.join("share/obelisk/lua-meta");
            std::fs::create_dir_all(&stubs).unwrap();

            let found = packaged_stub_dir_from(exe.parent().unwrap());
            assert_eq!(found, Some(stubs.canonicalize().unwrap()), "{label} layout must resolve its stubs");
        }
    }

    /// Stubs change while the version stays put, so a same-version edit must still be rewritten.
    #[test]
    fn stubs_that_differ_at_the_same_version_are_refreshed() {
        let data = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let stubs = data.path().join("obelisk/lua-meta");
        run_into(config.path(), false, Some(&stubs)).unwrap();

        let nodes_lua = stubs.join("nodes.lua");
        std::fs::write(&nodes_lua, "---@meta\n-- hand-written, once\n").unwrap();
        assert!(stale_stubs(&stubs).any(|(name, _)| *name == "nodes.lua"));

        run_into(config.path(), false, Some(&stubs)).unwrap();
        assert_eq!(stale_stubs(&stubs).count(), 0, "every stub must match the embedded copy again");
    }

    /// Without adjacent packaged stubs, return `None` instead of an editor path that completes
    /// nothing silently.
    #[test]
    fn a_prefix_without_stubs_resolves_to_nothing() {
        let root = tempfile::tempdir().unwrap();
        let exe_dir = root.path().join("lib/obelisk");
        std::fs::create_dir_all(&exe_dir).unwrap();
        let found = packaged_stub_dir_from(&exe_dir);
        assert_eq!(found, None);
    }

    #[test]
    fn init_writes_a_luarc_and_a_shell_lua_and_keeps_an_existing_one() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("obelisk");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("shell.lua"), "-- mine\n").unwrap();

        run_into(&config, false, Some(&dir.path().join("stubs"))).unwrap();

        assert!(config.join(".luarc.json").is_file());
        assert_eq!(
            std::fs::read_to_string(config.join("shell.lua")).unwrap(),
            "-- mine\n",
            "init must never overwrite a config someone already wrote"
        );
    }

    #[test]
    fn force_overwrites_the_starter_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().to_path_buf();
        std::fs::write(config.join("shell.lua"), "-- mine\n").unwrap();
        run_into(&config, true, Some(&config.join("stubs"))).unwrap();
        assert_ne!(std::fs::read_to_string(config.join("shell.lua")).unwrap(), "-- mine\n");
    }

    /// The `workspace.library` path has to be absolute, or the editor resolves it against the
    /// config directory and finds nothing, with no error anywhere.
    #[test]
    fn the_generated_luarc_points_at_an_absolute_stub_directory() {
        let dir = tempfile::tempdir().unwrap();
        run_into(dir.path(), false, Some(&dir.path().join("stubs"))).unwrap();
        let luarc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(".luarc.json")).unwrap()).unwrap();
        let library = luarc["workspace.library"][0].as_str().unwrap();
        assert!(Path::new(library).is_absolute(), "workspace.library must be absolute, got {library}");
        assert!(Path::new(library).join("obelisk.lua").is_file(), "the stub directory must actually hold the stubs");
    }
}
