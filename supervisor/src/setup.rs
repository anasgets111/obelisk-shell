//! `oblisk init`: makes a config directory an editable project.
//!
//! Writes two things, and neither is the config. `.luarc.json` points lua-language-server at the
//! stubs, and the stubs themselves are copied out when no packaged copy exists. A starter
//! `shell.lua` lands only if the directory has none, because overwriting someone's shell on a
//! re-run to fix a path would be indefensible.

use std::io;
use std::path::{Path, PathBuf};

/// The five stub files, compiled in.
///
/// lua-language-server reads from disk and cannot read out of a binary, so embedding them does not
/// remove the need to write them somewhere. What it buys is `cargo install`, which places no data
/// files at all: a packaged install finds them under `$PREFIX/share` and never touches these.
const EMBEDDED_STUBS: &[(&str, &str)] = &[
    ("globals.lua", include_str!("../../lua-meta/globals.lua")),
    ("nodes.lua", include_str!("../../lua-meta/nodes.lua")),
    ("oblisk.lua", include_str!("../../lua-meta/oblisk.lua")),
    ("signals.lua", include_str!("../../lua-meta/signals.lua")),
    ("surfaces.lua", include_str!("../../lua-meta/surfaces.lua")),
];

/// The config `init` writes when a directory has none.
///
/// A file rather than a string literal, so `just lua` parses it with `luac -p` alongside every
/// other Lua in the tree. A starter config with a syntax error in it is the worst possible first
/// impression, and a literal here would be checked by nothing.
const STARTER_SHELL_LUA: &str = include_str!("../../share/starter/shell.lua");

/// Where an installed copy of the stubs lives, resolved from the running binary the same way
/// `renderer_binary_path` resolves its sibling: `$PREFIX/lib/oblisk/oblisk` implies
/// `$PREFIX/share/oblisk/lua-meta`. Returns `None` when there is no such directory, which is the
/// normal answer for `cargo install` and for a binary run out of `target/`.
pub fn packaged_stub_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    // Two layouts, tried in order. `just install` puts the binary in `$PREFIX/lib/oblisk`, so the
    // prefix is two levels up; a flat install with the binary directly in `$PREFIX/bin` is one.
    // Checked rather than assumed, because getting it wrong is silent: `init` falls through to the
    // embedded copy and writes stubs the package would have kept up to date.
    ["../..", ".."]
        .iter()
        .map(|up| exe_dir.join(up).join("share").join("oblisk").join("lua-meta"))
        .find(|dir| dir.is_dir())
        .and_then(|dir| dir.canonicalize().ok())
}

/// Where `init` writes the embedded stubs when no packaged copy exists.
///
/// `$XDG_DATA_HOME/oblisk/lua-meta`, falling back to `$HOME/.local/share`. Outside the config
/// directory on purpose: see the note at its one call site.
fn user_stub_dir() -> io::Result<PathBuf> {
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(data_home).join("oblisk").join("lua-meta"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "neither XDG_DATA_HOME nor HOME is set"))?;
    Ok(PathBuf::from(home).join(".local").join("share").join("oblisk").join("lua-meta"))
}

/// Writes `path` unless it exists, reporting which it did. `force` overwrites.
fn write_unless_present(path: &Path, contents: &str, force: bool) -> io::Result<bool> {
    if path.exists() && !force {
        println!("  kept    {} (exists)", path.display());
        return Ok(false);
    }
    std::fs::write(path, contents)?;
    println!("  wrote   {}", path.display());
    Ok(true)
}

/// The `.luarc.json` body, pointing `workspace.library` at `stub_dir`.
///
/// An absolute path, because `workspace.library` resolves a relative one against the workspace
/// root, which is the config directory, not wherever the stubs happen to live. That is also why
/// this is generated rather than shipped as a static file, and why a symlink would not have helped:
/// the path has to be known at init time either way.
///
/// `runtime.path` mirrors the engine's own `package.path` exactly, and `runtime.builtin` removes
/// the libraries ADR-0048 cut, so the editor refuses `io.open` and `os.execute` the same way the
/// VM does.
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
  "workspace.checkThirdParty": false
}}
"#,
        stub_dir.display()
    )
}

/// `oblisk check`: evaluates the config and reports what it declares.
///
/// Re-execs the Renderer rather than evaluating here. The Supervisor has no `mlua` and no loader,
/// and a second, weaker check that only proved `shell.lua` is readable would be worse than none:
/// the errors worth catching are `require` failures and property errors, which only an evaluation
/// finds. One evaluation path, used by both the check and the boot.
pub fn check(config_dir: &Path) -> Result<String, String> {
    let renderer =
        crate::generation::renderer_binary_path().map_err(|err| format!("cannot find the renderer: {err}"))?;
    let output = std::process::Command::new(&renderer)
        .env(shared::CHECK_ENV, "1")
        .env(shared::CONFIG_DIR_ENV, config_dir)
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
    std::fs::create_dir_all(config_dir)?;
    println!("oblisk init: {}", config_dir.display());

    // A packaged install upgrades its stubs with the package, so pointing at them beats copying
    // them: a copied stub describing a different engine than the one running is worse than none.
    let stub_dir = match packaged_stub_dir() {
        Some(dir) => {
            println!("  stubs   {} (packaged)", dir.display());
            dir
        }
        None => {
            // `$XDG_DATA_HOME/oblisk/lua-meta`, deliberately not inside the config directory.
            // `watcher.rs` reloads the shell on any `.lua` file under the config directory, so
            // stubs living there would make `oblisk init` restyle a running bar, and a future
            // stub refresh would do it again. They are also not config.
            let dir = user_stub_dir()?;
            std::fs::create_dir_all(&dir)?;
            for (name, contents) in EMBEDDED_STUBS {
                write_unless_present(&dir.join(name), contents, true)?;
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

    /// `packaged_stub_dir` fails silently when it is wrong: `init` falls through to the embedded
    /// copy and writes stubs that the package would otherwise have kept current, so the user ends
    /// up with a stale schema and no error anywhere. It was wrong once, by exactly one `parent()`.
    ///
    /// Builds both installed layouts as directory trees and resolves against them directly, rather
    /// than re-execing an installed binary, so this runs in a normal `cargo test`.
    #[test]
    fn both_installed_layouts_resolve_to_the_packaged_stub_directory() {
        let root = tempfile::tempdir().unwrap();
        for (exe_rel, label) in [("lib/oblisk/oblisk", "just install"), ("bin/oblisk", "flat prefix")] {
            let prefix = root.path().join(label.replace(' ', "-"));
            let exe = prefix.join(exe_rel);
            std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
            std::fs::write(&exe, b"").unwrap();
            let stubs = prefix.join("share/oblisk/lua-meta");
            std::fs::create_dir_all(&stubs).unwrap();

            let exe_dir = exe.parent().unwrap();
            let found = ["../..", ".."]
                .iter()
                .map(|up| exe_dir.join(up).join("share").join("oblisk").join("lua-meta"))
                .find(|dir| dir.is_dir())
                .and_then(|dir| dir.canonicalize().ok());
            assert_eq!(found, Some(stubs.canonicalize().unwrap()), "{label} layout must resolve its stubs");
        }
    }

    /// A binary with no packaged stubs beside it must say so rather than pointing at a directory
    /// that does not exist, which would leave the editor silently completing nothing.
    #[test]
    fn a_prefix_without_stubs_resolves_to_nothing() {
        let root = tempfile::tempdir().unwrap();
        let exe_dir = root.path().join("lib/oblisk");
        std::fs::create_dir_all(&exe_dir).unwrap();
        let found = ["../..", ".."]
            .iter()
            .map(|up| exe_dir.join(up).join("share").join("oblisk").join("lua-meta"))
            .find(|dir| dir.is_dir());
        assert_eq!(found, None);
    }

    #[test]
    fn init_writes_a_luarc_and_a_shell_lua_and_keeps_an_existing_one() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("oblisk");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("shell.lua"), "-- mine\n").unwrap();

        run(&config, false).unwrap();

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
        run(&config, true).unwrap();
        assert_ne!(std::fs::read_to_string(config.join("shell.lua")).unwrap(), "-- mine\n");
    }

    /// The `workspace.library` path has to be absolute, or the editor resolves it against the
    /// config directory and finds nothing, with no error anywhere.
    #[test]
    fn the_generated_luarc_points_at_an_absolute_stub_directory() {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), false).unwrap();
        let luarc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(".luarc.json")).unwrap()).unwrap();
        let library = luarc["workspace.library"][0].as_str().unwrap();
        assert!(Path::new(library).is_absolute(), "workspace.library must be absolute, got {library}");
        assert!(Path::new(library).join("oblisk.lua").is_file(), "the stub directory must actually hold the stubs");
    }
}
