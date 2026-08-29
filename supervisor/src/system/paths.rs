//! Where `state.json` lives on disk (docs/oblisk-idl-api-specs.md §2.11: `system.state` is
//! "loaded from `$XDG_STATE_HOME/oblisk/state.json`"). Split out from `controller.rs` because
//! it is the one seam in this capability that touches the filesystem layout rather than time or
//! JSON shape, and keeping it a free function over plain `Path` arguments (no `std::env` call
//! inside it) is what makes it directly testable -- the same shape
//! `dbus::notifications::icon::default_trusted_icon_roots` / `validate_trusted_path` split into:
//! a thin env-reading wrapper the caller owns, and a pure resolver this module owns.

use std::path::{Path, PathBuf};

/// `home`/`xdg_state_home` are both roots the caller already resolved from the real environment
/// (`SystemController::new`'s job, mirroring `PrivacyController::new`'s `proc_root`/
/// `video4linux_root` parameters) -- this function never reads `std::env` itself, so every case
/// the base-directory spec distinguishes is reachable from a test without touching the process
/// environment.
///
/// `xdg_state_home` present wins outright, per the spec: `$XDG_STATE_HOME/oblisk/state.json`.
/// Absent, it falls back to `<home>/.local/state/oblisk/state.json`, the base-directory spec's
/// own default for `XDG_STATE_HOME` when unset.
pub fn resolve_state_path(home: &Path, xdg_state_home: Option<&Path>) -> PathBuf {
    match xdg_state_home {
        Some(dir) => dir.join("oblisk").join("state.json"),
        None => home.join(".local").join("state").join("oblisk").join("state.json"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_state_path_prefers_xdg_state_home_when_set() {
        let path = resolve_state_path(Path::new("/home/testuser"), Some(Path::new("/custom/state")));
        assert_eq!(path, PathBuf::from("/custom/state/oblisk/state.json"));
    }

    #[test]
    fn resolve_state_path_falls_back_to_home_local_state_when_unset() {
        let path = resolve_state_path(Path::new("/home/testuser"), None);
        assert_eq!(path, PathBuf::from("/home/testuser/.local/state/oblisk/state.json"));
    }
}
