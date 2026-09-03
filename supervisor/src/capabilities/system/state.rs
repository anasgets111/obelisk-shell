//! Loading and saving `system.state` (docs/oblisk-idl-api-specs.md §2.11: "a reactive dictionary
//! of persistent states"). Read once, at [`SystemController::new`](super::controller::
//! SystemController::new) time, and rewritten whole by every `system:write_state` (§3.2).

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

/// Whether `key` may name a slot in `state.json`.
///
/// §3.2 says "alphanumeric", which would forbid `updates_last_check` and force every config with
/// two modules to write `updateslastcheck`. Widened to also allow `_`, `-` and `.` (ADR-0113
/// amendment): namespacing is the actual use, and none of the three is any less safe as a JSON
/// object key. Still no path separators, no whitespace, no empty key -- the point of the rule is
/// that a key stays a key and can never be read as a path or a fragment of one.
pub fn key_is_writable(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Whether `value` is one of §3.2's three: a string, a number, or a boolean.
///
/// Not an object or an array, deliberately. A config wanting structure has JSON of its own
/// (`json.encode`) and a string to put it in, which keeps this file something a person can still
/// read and hand-edit -- the reason it is JSON on disk rather than a binary blob.
pub fn value_is_writable(value: &serde_json::Value) -> bool {
    matches!(value, serde_json::Value::String(_) | serde_json::Value::Number(_) | serde_json::Value::Bool(_))
}

/// Writes `state` to `path` as pretty JSON, creating the directory if this is the first write.
///
/// Through a temporary file in the same directory and a rename, which is atomic on any single
/// filesystem: a config that writes on every keystroke must not be able to leave a half-written
/// `state.json` behind a crash or a power cut, since the next boot reads whatever is there and a
/// truncated file degrades to an empty dictionary -- silently losing everything else in it.
pub fn save_state(path: &Path, state: &serde_json::Value) -> std::io::Result<()> {
    let Some(dir) = path.parent() else {
        return Err(std::io::Error::other(format!("{} has no parent directory", path.display())));
    };
    std::fs::create_dir_all(dir)?;
    let mut serialized = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    serialized.push(b'\n');

    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, &serialized)?;
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_may_be_namespaced_but_never_a_path() {
        assert!(key_is_writable("theme"));
        assert!(key_is_writable("updates.last_check"), "namespacing is the reason the rule was widened");
        assert!(key_is_writable("launcher-frecency"));
        assert!(!key_is_writable(""));
        assert!(!key_is_writable("../escape"), "a key must never be readable as a path");
        assert!(!key_is_writable("with space"));
        assert!(!key_is_writable("sl/ash"));
    }

    #[test]
    fn a_value_is_a_scalar_and_structure_goes_through_a_string() {
        assert!(value_is_writable(&serde_json::json!("dark")));
        assert!(value_is_writable(&serde_json::json!(3)));
        assert!(value_is_writable(&serde_json::json!(1.5)));
        assert!(value_is_writable(&serde_json::json!(true)));
        assert!(!value_is_writable(&serde_json::json!(null)));
        assert!(!value_is_writable(&serde_json::json!({ "nested": 1 })));
        assert!(!value_is_writable(&serde_json::json!([1, 2])));
    }

    #[test]
    fn a_saved_state_reads_back_through_the_loader() {
        // The round trip is the contract: what `write_state` stores is what the next boot loads.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oblisk").join("state.json");
        let stored = serde_json::json!({ "theme": "dark", "count": 3 });

        save_state(&path, &stored).expect("the directory is created on the first write");

        assert_eq!(load_state(&path), stored);
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        save_state(&path, &serde_json::json!({ "a": 1 })).unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "state.json")
            .collect();
        assert!(leftovers.is_empty(), "the rename is the write; nothing else may survive it: {leftovers:?}");
    }

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
            assert_eq!(
                load_state(&path),
                empty_object(),
                "{name}: §2.11 promises a dictionary, not an array or a scalar"
            );
        }
    }
}
