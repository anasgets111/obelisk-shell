//! Where `state.json` lives on disk (docs/oblisk-idl-api-specs.md §2.11: `system.state` is
//! "loaded from `$XDG_STATE_HOME/oblisk/state.json`"). Split out from `controller.rs`: a thin
//! env-reading wrapper the caller owns, and this pure resolver over plain `Path` arguments
//! (no `std::env` call inside it), which is what makes it directly testable.

use std::path::{Path, PathBuf};

/// `home`/`xdg_state_home` are both roots the caller already resolved from the real environment
/// -- this function never reads `std::env` itself, so every case is reachable from a test.
///
/// `xdg_state_home` present wins outright, per the spec: `$XDG_STATE_HOME/oblisk/state.json`.
/// Absent, it falls back to `<home>/.local/state/oblisk/state.json`.
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
