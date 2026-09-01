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

/// The environment variable `oblisk -c <dir>` sets, and the highest-precedence answer below.
///
/// It is public rather than an implementation detail of the spawn path, because running two
/// configs side by side is the reason `-c` exists and hiding the variable buys nothing.
pub const CONFIG_DIR_ENV: &str = "OBLISK_CONFIG_DIR";

/// The generation id the Supervisor stamps on every Renderer it spawns.
///
/// Here rather than in either binary because both read it and its absence means something to both:
/// the Renderer treats it as "nobody spawned me" and refuses to start, and the Supervisor sets it
/// on the boot spawn and on every generation swap.
pub const GENERATION_ID_ENV: &str = "OBLISK_GENERATION_ID";

/// Set on the Renderer when `oblisk check` re-execs it to evaluate a config without a display.
pub const CHECK_ENV: &str = "OBLISK_CHECK";

/// `~/.config/oblisk/`, in precedence order: `$OBLISK_CONFIG_DIR`, then `$XDG_CONFIG_HOME/oblisk`,
/// then the dev config in a debug build, then `$HOME/.config/oblisk`.
///
/// Both binaries call this and neither tells the other what it got. They agree because they share
/// an environment, which is exactly why `-c` sets [`CONFIG_DIR_ENV`] in the Supervisor's own
/// process rather than passing a path down: every Renderer the Supervisor spawns inherits it, and
/// so does every Renderer a generation swap spawns later. A path passed through the handshake
/// instead would have to be re-passed on every swap, and a swap that forgot would silently read a
/// different config than the one being watched.
///
/// A debug build looks in the workspace's `dev-config/oblisk/` before `$HOME`, so a binary run out
/// of `target/debug` boots against the tracked dev config with nothing set up. Both variables win
/// over it, so it is not a second source of truth, and a release build never compiles it in.
pub fn config_dir() -> io::Result<PathBuf> {
    config_dir_from(std::env::var_os(CONFIG_DIR_ENV), std::env::var_os("XDG_CONFIG_HOME"), std::env::var_os("HOME"))
}

/// The precedence [`config_dir`] applies, with the three lookups hoisted into parameters.
///
/// Split out so the precedence tests can pass values instead of calling `set_var`. `setenv`
/// rewrites the process-wide `environ` block, so it races every concurrent `getenv` in the test
/// binary regardless of which variable either one names; a test that mutates the environment to
/// describe precedence was buying one assertion with a data race across the whole suite.
fn config_dir_from(
    explicit: Option<std::ffi::OsString>,
    xdg_config_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> io::Result<PathBuf> {
    // Taken as the directory itself, not joined with `oblisk`: `-c` names the config, where
    // `$XDG_CONFIG_HOME` names the directory configs live in.
    if let Some(explicit) = explicit {
        return Ok(PathBuf::from(explicit));
    }

    if let Some(xdg_config_home) = xdg_config_home {
        return Ok(PathBuf::from(xdg_config_home).join("oblisk"));
    }

    // A debug binary run away from the tree it was built in has a DEV_CONFIG_DIR pointing at
    // nothing, so this is checked rather than assumed.
    #[cfg(debug_assertions)]
    if std::fs::metadata(DEV_CONFIG_DIR).is_ok() {
        return Ok(PathBuf::from(DEV_CONFIG_DIR));
    }

    let home =
        home.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "neither XDG_CONFIG_HOME nor HOME is set"))?;
    Ok(PathBuf::from(home).join(".config").join("oblisk"))
}

/// `config_dir()` joined with `shell.lua`, the real config entry point.
pub fn shell_lua_path() -> io::Result<PathBuf> {
    Ok(config_dir()?.join("shell.lua"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No lock guards these any more. The `ENV` mutex that used to live here serialized the one
    /// test that called `set_var` against the ones that only read; with the precedence tests
    /// moved onto [`config_dir_from`], nothing in this binary writes the environment, so there is
    /// nothing left to serialize against.
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
    #[test]
    fn xdg_config_home_still_wins_over_the_dev_config_directory() {
        let resolved = config_dir_from(None, Some("/tmp/oblisk-config-dir-test".into()), None).unwrap();
        assert_eq!(resolved, PathBuf::from("/tmp/oblisk-config-dir-test/oblisk"));
    }

    /// `-c` has to beat `$XDG_CONFIG_HOME`, or a user with the variable set could not point the
    /// shell at a second config at all.
    #[test]
    fn the_explicit_config_dir_wins_over_xdg_config_home() {
        let resolved =
            config_dir_from(Some("/tmp/oblisk-explicit".into()), Some("/tmp/oblisk-xdg".into()), None).unwrap();
        // Taken whole, not joined with `oblisk`: this names the config directory itself.
        assert_eq!(resolved, PathBuf::from("/tmp/oblisk-explicit"));
    }

    /// The last rung of the ladder, which the previous `set_var` tests could not reach without
    /// unsetting the developer's own `$HOME`.
    #[test]
    fn home_is_the_last_resort_and_is_joined_with_dot_config() {
        // `$HOME` is only consulted in a release build; a debug build finds `DEV_CONFIG_DIR`
        // first, which is the branch asserted by the dev-config test above.
        #[cfg(not(debug_assertions))]
        assert_eq!(
            config_dir_from(None, None, Some("/home/someone".into())).unwrap(),
            PathBuf::from("/home/someone/.config/oblisk")
        );
    }

    /// Nothing set at all is an error rather than a guess, in a release build.
    #[test]
    #[cfg(not(debug_assertions))]
    fn no_variable_at_all_is_an_error() {
        assert_eq!(config_dir_from(None, None, None).unwrap_err().kind(), io::ErrorKind::NotFound);
    }
}
