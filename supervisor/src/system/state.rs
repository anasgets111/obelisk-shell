//! Loading `system.state` (docs/oblisk-idl-api-specs.md §2.11: "a reactive, read-only dictionary
//! of persistent states"). Read once, at [`SystemController::new`](super::controller::
//! SystemController::new) time, and never again -- this codebase has no write path to
//! `state.json` yet (only `system:write_state`'s IDL row exists, §3.2, unbuilt).

use std::path::Path;

/// Every failure mode collapses to an empty JSON object rather than an error or a panic:
///
/// - **Missing file**: the ordinary first-run case -- no `state.json` yet is not a fault.
/// - **Unreadable file**: folded into the same case as missing. Nothing downstream
///   distinguishes "absent" from "present but inaccessible".
/// - **Malformed JSON**: same degrade -- a hand-edit syntax error must not block boot.
/// - **Well-formed JSON that isn't a top-level object**: §2.11 promises a dictionary (Lua
///   `table` keyed by string), so an array or scalar is treated the same as malformed.
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
