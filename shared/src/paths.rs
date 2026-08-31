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
    // Taken as the directory itself, not joined with `oblisk`: `-c` names the config, where
    // `$XDG_CONFIG_HOME` names the directory configs live in.
    if let Some(explicit) = std::env::var_os(CONFIG_DIR_ENV) {
        return Ok(PathBuf::from(explicit));
    }

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

    /// Serializes every test that reads or writes `$XDG_CONFIG_HOME`.
    ///
    /// `config_dir` resolves through a process-global environment variable, and one test here has
    /// to set it to prove it still wins. Without this lock that write lands in the middle of
    /// another test's two reads, and the two disagree: the first read sees the injected value and
    /// the second sees the dev-config fallback. That failed about a third of the time, in a test
    /// whose subject is a `join` -- so the noise pointed at the wrong function entirely.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Kept even when a previous holder panicked: the guarded state is the environment, which the
    /// holder restores itself, so a poisoned lock carries no broken invariant worth failing on.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn shell_lua_path_is_config_dir_joined_with_shell_lua() {
        let _guard = env_lock();
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
        let _guard = env_lock();
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", "/tmp/oblisk-config-dir-test") };
        let resolved = config_dir().unwrap();
        match previous {
            Some(value) => unsafe { std::env::set_var("XDG_CONFIG_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        assert_eq!(resolved, PathBuf::from("/tmp/oblisk-config-dir-test/oblisk"));
    }

    /// `-c` has to beat `$XDG_CONFIG_HOME`, or a user with the variable set could not point the
    /// shell at a second config at all.
    #[test]
    fn the_explicit_config_dir_wins_over_xdg_config_home() {
        let _guard = env_lock();
        let previous_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let previous_explicit = std::env::var_os(CONFIG_DIR_ENV);
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "/tmp/oblisk-xdg");
            std::env::set_var(CONFIG_DIR_ENV, "/tmp/oblisk-explicit");
        }
        let resolved = config_dir().unwrap();
        unsafe {
            match previous_xdg {
                Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match previous_explicit {
                Some(value) => std::env::set_var(CONFIG_DIR_ENV, value),
                None => std::env::remove_var(CONFIG_DIR_ENV),
            }
        }
        // Taken whole, not joined with `oblisk`: this names the config directory itself.
        assert_eq!(resolved, PathBuf::from("/tmp/oblisk-explicit"));
    }
}
