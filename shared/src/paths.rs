//! XDG paths shared by `supervisor` and `renderer`, so both resolve the control socket, session
//! lock flag and config directory identically. In debug builds `config_dir` prefers the tracked
//! `dev-config/obelisk/` over every configured directory but `-c`.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// Control socket under `$XDG_RUNTIME_DIR`, shared by the `supervisor` listener and `renderer`
/// client. Not `/tmp`: it is world-writable and unsuitable for secure textfield submissions
/// (ADR-0005).
pub fn control_socket_path() -> io::Result<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime_dir).join("obelisk-shell.sock"))
}

/// The "compositor is locked and nothing of ours holds it" marker (ADR-0060), beside the control
/// socket as per-login runtime state under `$XDG_RUNTIME_DIR`. Only `supervisor` reads or writes
/// it; the Renderer holds the protocol object but never the decision (ADR-0042).
pub fn session_locked_flag_path() -> io::Result<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime_dir).join("obelisk-session-locked"))
}

/// Where `Command::Run` parks stdout and stderr when no terminal is reading them, and where
/// `obelisk log` reads them back (ADR-0199). Beside the control socket, and per-login like it: the
/// only run worth reading is the current one.
pub fn log_path() -> io::Result<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime_dir).join("obelisk-shell.log"))
}

/// The tracked dev config, baked in so it resolves from any working directory. The workspace root
/// is one level above this crate's `CARGO_MANIFEST_DIR`.
#[cfg(debug_assertions)]
const DEV_CONFIG_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../dev-config/obelisk");

/// `obelisk -c <dir>`, apart from [`CONFIG_DIR_ENV`] because only `-c` beats the debug dev config.
pub const CONFIG_ARG_ENV: &str = "OBELISK_CONFIG_ARG";

/// A config directory named by the session, below `-c` and the debug dev config.
pub const CONFIG_DIR_ENV: &str = "OBELISK_CONFIG_DIR";

/// Generation id stamped on every spawned Renderer.
///
/// Shared because both binaries read it. If absent, the Renderer treats that as "nobody spawned
/// me" and refuses to start; the Supervisor sets it on boot and every respawn.
pub const GENERATION_ID_ENV: &str = "OBELISK_GENERATION_ID";

/// Set when `obelisk check` re-execs the Renderer to evaluate a config without a display.
pub const CHECK_ENV: &str = "OBELISK_CHECK";

/// `obelisk --profile[=SECS]`, set by the Supervisor so every Renderer generation inherits it. One
/// switch for the idle, heap and PSS/GPU reports, so their lines share a clock.
pub const PROFILE_ENV: &str = "OBELISK_PROFILE";

/// The report interval [`PROFILE_ENV`] carries; the CLI already refused a bad value.
pub fn profile_interval() -> Option<Duration> {
    let secs = std::env::var(PROFILE_ENV).ok()?.parse::<u64>().ok().filter(|secs| *secs > 0)?;
    Some(Duration::from_secs(secs))
}

/// Renderer exit code for a Wayland connection that is gone: a log out, a reboot, or a compositor
/// crash. Shared because the Supervisor reads it as "the session is over" and stops rather than
/// respawning into a compositor that is not there.
///
/// Distinct from `0` (clean), `1` (a `?` failure) and `101` (a panic), and from the Renderer's `70`
/// for a gone Supervisor.
pub const EXIT_COMPOSITOR_GONE: i32 = 71;

/// `~/.config/obelisk/` by precedence: `-c` ([`CONFIG_ARG_ENV`]), the dev config in debug builds,
/// `$OBELISK_CONFIG_DIR`, `$XDG_CONFIG_HOME/obelisk`, then `$HOME/.config/obelisk`.
///
/// Both binaries call this and agree through the environment. `-c` therefore sets
/// [`CONFIG_ARG_ENV`] in the Supervisor: every spawned Renderer, including a replacement,
/// inherits it. Passing a path through the handshake would require re-passing it on every
/// respawn; a missed pass would silently load a different config than the watched one.
pub fn config_dir() -> io::Result<PathBuf> {
    // A debug binary run away from its build tree may have a nonexistent DEV_CONFIG_DIR.
    #[cfg(debug_assertions)]
    let dev = std::fs::metadata(DEV_CONFIG_DIR).is_ok().then(|| DEV_CONFIG_DIR.into());
    #[cfg(not(debug_assertions))]
    let dev = None;
    let var = std::env::var_os;
    config_dir_from(var(CONFIG_ARG_ENV), dev, var(CONFIG_DIR_ENV), var("XDG_CONFIG_HOME"), var("HOME"))
}

/// [`config_dir`]'s precedence with its lookups passed as parameters.
///
/// Tests pass values instead of calling `set_var`: `setenv` rewrites process-wide `environ` and
/// races every concurrent `getenv`, regardless of which variable each call names.
fn config_dir_from(
    arg: Option<OsString>,
    dev: Option<OsString>,
    explicit: Option<OsString>,
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> io::Result<PathBuf> {
    // These name the config directory itself; `$XDG_CONFIG_HOME` names its parent.
    if let Some(dir) = arg.or(dev).or(explicit) {
        return Ok(PathBuf::from(dir));
    }

    if let Some(xdg_config_home) = xdg_config_home {
        return Ok(PathBuf::from(xdg_config_home).join("obelisk"));
    }

    let home =
        home.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "neither XDG_CONFIG_HOME nor HOME is set"))?;
    Ok(PathBuf::from(home).join(".config").join("obelisk"))
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
        assert!(path.ends_with("shell.lua"));
    }

    /// `-c` must beat the dev config, so a debug build can still run a second config.
    #[test]
    fn the_config_argument_wins_over_the_dev_config() {
        let resolved = config_dir_from(Some("/tmp/arg".into()), Some("/tmp/dev".into()), None, None, None).unwrap();
        // `-c` is the config directory itself, not a parent to join with `obelisk`.
        assert_eq!(resolved, PathBuf::from("/tmp/arg"));
    }

    /// A debug build boots the tracked config even when the session names another.
    #[test]
    fn the_dev_config_wins_over_the_session_environment() {
        let resolved =
            config_dir_from(None, Some("/tmp/dev".into()), Some("/tmp/env".into()), Some("/tmp/xdg".into()), None)
                .unwrap();
        assert_eq!(resolved, PathBuf::from("/tmp/dev"));
    }

    /// The last rung, which old `set_var` tests could not reach without unsetting the developer's
    /// `$HOME`.
    #[test]
    fn home_is_the_last_resort_and_is_joined_with_dot_config() {
        assert_eq!(
            config_dir_from(None, None, None, None, Some("/home/someone".into())).unwrap(),
            PathBuf::from("/home/someone/.config/obelisk")
        );
    }

    /// No variables means an error rather than a guess.
    #[test]
    fn no_variable_at_all_is_an_error() {
        assert_eq!(config_dir_from(None, None, None, None, None).unwrap_err().kind(), io::ErrorKind::NotFound);
    }
}
