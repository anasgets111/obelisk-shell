//! XDG paths shared by `supervisor` and `renderer`, so both resolve the control socket, session
//! lock flag and config directory identically. In debug builds `config_dir` checks the tracked
//! `dev-config/oblisk/` first, but `$XDG_CONFIG_HOME` still wins.

use std::io;
use std::path::PathBuf;

/// Control socket under `$XDG_RUNTIME_DIR`, shared by the `supervisor` listener and `renderer`
/// client. Not `/tmp`: it is world-writable and unsuitable for secure textfield submissions
/// (ADR-0005).
pub fn control_socket_path() -> io::Result<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime_dir).join("oblisk-shell.sock"))
}

/// The "compositor is locked and nothing of ours holds it" marker (ADR-0060), beside the control
/// socket as per-login runtime state under `$XDG_RUNTIME_DIR`. Only `supervisor` reads or writes
/// it; the Renderer holds the protocol object but never the decision (ADR-0042).
pub fn session_locked_flag_path() -> io::Result<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime_dir).join("oblisk-session-locked"))
}

/// The tracked dev config, baked in so it resolves from any working directory. The workspace root
/// is one level above this crate's `CARGO_MANIFEST_DIR`.
#[cfg(debug_assertions)]
const DEV_CONFIG_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../dev-config/oblisk");

/// `oblisk -c <dir>` sets this highest-precedence config directory.
///
/// It is public because `-c` runs two configs side by side; hiding the inherited variable would
/// buy nothing.
pub const CONFIG_DIR_ENV: &str = "OBLISK_CONFIG_DIR";

/// Generation id stamped on every spawned Renderer.
///
/// Shared because both binaries read it. If absent, the Renderer treats that as "nobody spawned
/// me" and refuses to start; the Supervisor sets it on boot and every generation swap.
pub const GENERATION_ID_ENV: &str = "OBLISK_GENERATION_ID";

/// Set when `oblisk check` re-execs the Renderer to evaluate a config without a display.
pub const CHECK_ENV: &str = "OBLISK_CHECK";

/// Renderer exit code for a Wayland connection that is gone: a log out, a reboot, or a compositor
/// crash. Shared because the Supervisor reads it as "the session is over" and stops rather than
/// respawning into a compositor that is not there.
///
/// Distinct from `0` (clean), `1` (a `?` failure) and `101` (a panic), and from the Renderer's `70`
/// for a gone Supervisor.
pub const EXIT_COMPOSITOR_GONE: i32 = 71;

/// `~/.config/oblisk/` by precedence: `$OBLISK_CONFIG_DIR`, `$XDG_CONFIG_HOME/oblisk`, the
/// debug-only dev config, then `$HOME/.config/oblisk`.
///
/// Both binaries call this and agree through the environment. `-c` therefore sets
/// [`CONFIG_DIR_ENV`] in the Supervisor: every spawned Renderer, including after a generation
/// swap, inherits it. Passing a path through the handshake would require re-passing it on every
/// swap; a missed pass would silently load a different config than the watched one.
///
/// A `target/debug` binary can boot from the tracked `dev-config/oblisk/` with nothing configured.
/// Both environment variables beat it, so it is not a second source of truth; release builds do
/// not compile it in.
pub fn config_dir() -> io::Result<PathBuf> {
    config_dir_from(std::env::var_os(CONFIG_DIR_ENV), std::env::var_os("XDG_CONFIG_HOME"), std::env::var_os("HOME"))
}

/// [`config_dir`]'s precedence with its three lookups passed as parameters.
///
/// Tests pass values instead of calling `set_var`: `setenv` rewrites process-wide `environ` and
/// races every concurrent `getenv`, regardless of which variable each call names.
fn config_dir_from(
    explicit: Option<std::ffi::OsString>,
    xdg_config_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> io::Result<PathBuf> {
    // `-c` names the config directory itself; `$XDG_CONFIG_HOME` names its parent.
    if let Some(explicit) = explicit {
        return Ok(PathBuf::from(explicit));
    }

    if let Some(xdg_config_home) = xdg_config_home {
        return Ok(PathBuf::from(xdg_config_home).join("oblisk"));
    }

    // A debug binary run away from its build tree may have a nonexistent DEV_CONFIG_DIR.
    #[cfg(debug_assertions)]
    if std::fs::metadata(DEV_CONFIG_DIR).is_ok() {
        return Ok(PathBuf::from(DEV_CONFIG_DIR));
    }

    let home =
        home.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "neither XDG_CONFIG_HOME nor HOME is set"))?;
    Ok(PathBuf::from(home).join(".config").join("oblisk"))
}

/// `config_dir()` joined with the real config entry point, `shell.lua`.
pub fn shell_lua_path() -> io::Result<PathBuf> {
    Ok(config_dir()?.join("shell.lua"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No lock is needed: precedence tests use [`config_dir_from`] and no test writes the
    /// process-wide environment anymore.
    #[test]
    fn shell_lua_path_is_config_dir_joined_with_shell_lua() {
        let path = shell_lua_path().unwrap();
        assert_eq!(path, config_dir().unwrap().join("shell.lua"));
        assert!(path.ends_with("oblisk/shell.lua"));
    }

    /// Catches a crate or workspace move without a matching `DEV_CONFIG_DIR` change by reading
    /// the `shell.lua` that `config_dir` must find.
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

    /// `$XDG_CONFIG_HOME` must beat the debug branch, or that branch becomes a silent second source
    /// of truth.
    #[test]
    fn xdg_config_home_still_wins_over_the_dev_config_directory() {
        let resolved = config_dir_from(None, Some("/tmp/oblisk-config-dir-test".into()), None).unwrap();
        assert_eq!(resolved, PathBuf::from("/tmp/oblisk-config-dir-test/oblisk"));
    }

    /// `-c` must beat `$XDG_CONFIG_HOME` so a user can select a second config.
    #[test]
    fn the_explicit_config_dir_wins_over_xdg_config_home() {
        let resolved =
            config_dir_from(Some("/tmp/oblisk-explicit".into()), Some("/tmp/oblisk-xdg".into()), None).unwrap();
        // `-c` is the config directory itself, not a parent to join with `oblisk`.
        assert_eq!(resolved, PathBuf::from("/tmp/oblisk-explicit"));
    }

    /// The last rung, which old `set_var` tests could not reach without unsetting the developer's
    /// `$HOME`.
    #[test]
    fn home_is_the_last_resort_and_is_joined_with_dot_config() {
        // Release builds consult `$HOME`; debug builds find `DEV_CONFIG_DIR` first.
        #[cfg(not(debug_assertions))]
        assert_eq!(
            config_dir_from(None, None, Some("/home/someone".into())).unwrap(),
            PathBuf::from("/home/someone/.config/oblisk")
        );
    }

    /// In a release build, no variables means an error rather than a guess.
    #[test]
    #[cfg(not(debug_assertions))]
    fn no_variable_at_all_is_an_error() {
        assert_eq!(config_dir_from(None, None, None).unwrap_err().kind(), io::ErrorKind::NotFound);
    }
}
