//! XDG path resolution shared by `supervisor` and `renderer`, so both resolve the control
//! socket, the session-lock flag and the user config directory identically. `config_dir` is
//! the one with a wrinkle: a debug build checks the workspace's tracked `dev-config/oblisk/`
//! first, but `$XDG_CONFIG_HOME` still wins over it when set.

use std::io;
use std::path::PathBuf;

/// Where the control socket lives, derived from `$XDG_RUNTIME_DIR`. Shared so `supervisor`
/// (listener) and `renderer` (client) resolve it identically. Not `/tmp`, which is
/// world-writable and unsuitable for a socket that carries secure textfield submissions
/// (ADR-0005).
pub fn control_socket_path() -> io::Result<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime_dir).join("oblisk-shell.sock"))
}

/// Where the "the compositor is locked and nothing of ours holds it" marker lives (docs/adr/0060).
/// Beside the control socket deliberately: both are per-login runtime state bounded by
/// `$XDG_RUNTIME_DIR` going away with the session. Only `supervisor` reads or writes it -- the
/// Renderer holds the protocol object but never the decision (docs/adr/0042).
pub fn session_locked_flag_path() -> io::Result<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime_dir).join("oblisk-session-locked"))
}

/// The workspace's tracked dev config, baked in at compile time so it resolves the same from any
/// working directory. `CARGO_MANIFEST_DIR` is this crate's own directory, so the workspace root
/// is one level up.
#[cfg(debug_assertions)]
const DEV_CONFIG_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../dev-config/oblisk");

/// `~/.config/oblisk/`, resolved via `$XDG_CONFIG_HOME` falling back to `$HOME/.config` (XDG
/// Base Directory order). Both `supervisor` (watches this directory) and `renderer` (reads
/// `shell.lua` from it) resolve it identically.
///
/// A debug build looks in the workspace's `dev-config/oblisk/` first, so `cargo run -p
/// supervisor` boots against the tracked dev config with no environment set up. `$XDG_CONFIG_HOME`
/// still wins in both builds, so the dev branch is not a second source of truth. Release builds
/// never see it at all: `debug_assertions` is off, so `DEV_CONFIG_DIR` isn't compiled in.
pub fn config_dir() -> io::Result<PathBuf> {
    if let Some(xdg_config_home) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg_config_home).join("oblisk"));
    }

    // A debug binary run away from the tree it was built in has a DEV_CONFIG_DIR pointing at
    // nothing, so this is checked rather than assumed.
    #[cfg(debug_assertions)]
    if std::fs::metadata(DEV_CONFIG_DIR).is_ok() {
        return Ok(PathBuf::from(DEV_CONFIG_DIR));
    }

    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "neither XDG_CONFIG_HOME nor HOME is set"))?;
    Ok(PathBuf::from(home).join(".config").join("oblisk"))
}

/// `config_dir()` joined with `shell.lua`, the real config entry point.
pub fn shell_lua_path() -> io::Result<PathBuf> {
    Ok(config_dir()?.join("shell.lua"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_lua_path_is_config_dir_joined_with_shell_lua() {
        let path = shell_lua_path().unwrap();
        assert_eq!(path, config_dir().unwrap().join("shell.lua"));
        assert!(path.ends_with("oblisk/shell.lua"));
    }

    /// Catches the crate being moved or the workspace restructured without `DEV_CONFIG_DIR`
    /// following: reads the file `config_dir` exists to find, rather than asserting on the string.
    #[cfg(debug_assertions)]
    #[test]
    fn the_baked_in_dev_config_path_holds_a_real_shell_lua() {
        let shell_lua = PathBuf::from(DEV_CONFIG_DIR).join("shell.lua");
        assert!(
            std::fs::read_to_string(&shell_lua).is_ok(),
            "DEV_CONFIG_DIR points at {DEV_CONFIG_DIR:?}, which has no readable shell.lua -- \
             a debug build resolves its config through this constant"
        );
    }

    /// `$XDG_CONFIG_HOME` must keep winning in a debug build, or the dev branch would be a second
    /// source of truth that silently overrides the documented one.
    ///
    /// Sets a process-global for the duration, so it is deliberately the only test here that
    /// touches the environment.
    #[test]
    fn xdg_config_home_still_wins_over_the_dev_config_directory() {
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", "/tmp/oblisk-config-dir-test") };
        let resolved = config_dir().unwrap();
        match previous {
            Some(value) => unsafe { std::env::set_var("XDG_CONFIG_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        assert_eq!(resolved, PathBuf::from("/tmp/oblisk-config-dir-test/oblisk"));
    }
}
