//! Loading `system.state` (docs/oblisk-idl-api-specs.md §2.11: "a reactive, read-only dictionary
//! of persistent states"). Read once, at [`SystemController::new`](super::controller::
//! SystemController::new) time, and never again -- this codebase has no write path to
//! `state.json` yet (only `system:write_state`'s IDL row exists, §3.2, unbuilt), so re-reading
//! a file nothing in this process ever changes on a timer would be pure waste, the same
//! resolve-once argument `SysinfoController::new` makes for its hwmon chip lookup.

use std::path::Path;

/// Every failure mode collapses to an empty JSON object rather than an error or a panic:
///
/// - **Missing file**: the ordinary first-run case. A config that has never called
///   `system:write_state` has no `state.json` yet, and that is not a fault.
/// - **Unreadable file** (permissions, not-a-regular-file, ...): `read_to_string`'s `Err` is
///   folded into the same case as missing. Nothing downstream distinguishes "absent" from
///   "present but inaccessible" -- both mean the dictionary this signal promises has no content
///   to serve, and a lock screen's clock must still be able to tick either way (§2.11's `time`
///   is a separate field on the same struct and does not depend on this succeeding).
/// - **Malformed JSON**: same degrade. A syntax error in a file this process did not write
///   itself (or a hand-edit) must not turn a persistent-state read into a boot-blocking error.
/// - **Well-formed JSON that isn't a top-level object** (an array, a string, a number, `null`):
///   §2.11 promises `system.state` as a dictionary (Lua `table` keyed by string), so a top-level
///   JSON array or scalar is not a shape this signal can honor, and is treated the same as
///   malformed rather than passed through as something Lua would see as a numerically-indexed
///   table instead of a dictionary.
pub fn load_state(path: &Path) -> serde_json::Value {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return empty_object();
    };
    match serde_json::from_str::<serde_json::Value>(&contents) {
        Ok(value @ serde_json::Value::Object(_)) => value,
        _ => empty_object(),
    }
}

fn empty_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_state_parses_a_valid_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, r#"{"theme": "dark", "count": 3}"#).unwrap();

        let state = load_state(&path);

        assert_eq!(state, serde_json::json!({"theme": "dark", "count": 3}));
    }

    #[test]
    fn load_state_degrades_to_an_empty_object_when_the_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");

        assert_eq!(load_state(&path), empty_object(), "no state.json is the normal first-run case, not an error");
    }

    #[test]
    fn load_state_degrades_to_an_empty_object_on_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{not valid json").unwrap();

        assert_eq!(load_state(&path), empty_object());
    }

    #[test]
    fn load_state_degrades_to_an_empty_object_when_the_top_level_value_is_not_an_object() {
        let dir = tempfile::tempdir().unwrap();
        for (name, contents) in [("array.json", "[1, 2, 3]"), ("scalar.json", "42")] {
            let path = dir.path().join(name);
            std::fs::write(&path, contents).unwrap();
            assert_eq!(load_state(&path), empty_object(), "{name}: §2.11 promises a dictionary, not an array or a scalar");
        }
    }
}
