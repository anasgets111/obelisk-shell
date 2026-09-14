//! `obelisk.storage` owns JSON files declared with `persistent_table` (ADR-0136). Plain filesystem
//! reads/writes, sibling to `files`/`system`, with no D-Bus proxy or hardware thread.
//!
//! The config chooses each path, name, and defaults; "settings", "state", and "cache" are not
//! Supervisor concepts. This replaced `system:write_state` and its hardcoded `state.json`.

pub mod controller;

pub use controller::{StorageController, StorageSignal};

#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StorageAction {
    /// (path: string, defaults?: table) Declares an absolute JSON file; defaults fill missing keys.
    Open,
    /// (path: string, key: string, value?: any) Writes a key; `nil` deletes it.
    Set,
}

/// `storage:open(path, defaults)`'s `arguments: [path, defaults]`. The Renderer joins `path` and
/// `name` first, so one absolute path arrives and becomes the config's key.
pub fn parse_open_args(arguments: &[serde_json::Value]) -> Option<(String, serde_json::Value)> {
    let path = arguments.first()?.as_str()?.to_string();
    let defaults = match arguments.get(1) {
        None | Some(serde_json::Value::Null) => serde_json::Value::Object(serde_json::Map::new()),
        Some(value @ serde_json::Value::Object(_)) => value.clone(),
        Some(_) => return None,
    };
    Some((path, defaults))
}

/// `storage:set(path, key, value)`'s three arguments. A missing third is `null`, deleting the key
/// because Lua `nil` marshals as `null`.
pub fn parse_set_args(arguments: &[serde_json::Value]) -> Option<(String, String, serde_json::Value)> {
    let path = arguments.first()?.as_str()?.to_string();
    let key = arguments.get(1)?.as_str()?.to_string();
    let value = arguments.get(2).cloned().unwrap_or(serde_json::Value::Null);
    Some((path, key, value))
}

/// `obelisk.storage` action dispatch (ADR-0037). Synchronous: actions touch memory and schedule the
/// save task.
pub fn dispatch(controller: &StorageController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<StorageAction>(params) else { return };
    match action {
        StorageAction::Open => match parse_open_args(&params.arguments) {
            Some((path, defaults)) => controller.open(&path, &defaults),
            None => crate::log_malformed_command(params),
        },
        StorageAction::Set => match parse_set_args(&params.arguments) {
            Some((path, key, value)) => controller.set(&path, &key, value),
            None => crate::log_malformed_command(params),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn open_takes_a_path_and_an_optional_defaults_table() {
        assert_eq!(parse_open_args(&[json!("/s.json")]), Some(("/s.json".to_string(), json!({}))));
        assert_eq!(
            parse_open_args(&[json!("/s.json"), json!({ "theme": "mocha" })]),
            Some(("/s.json".to_string(), json!({ "theme": "mocha" })))
        );
        assert_eq!(parse_open_args(&[json!("/s.json"), json!([1, 2])]), None, "defaults is a table, not a list");
        assert_eq!(parse_open_args(&[]), None);
    }

    #[test]
    fn set_takes_three_arguments_and_a_missing_value_is_a_delete() {
        assert_eq!(
            parse_set_args(&[json!("/s.json"), json!("theme"), json!("latte")]),
            Some(("/s.json".to_string(), "theme".to_string(), json!("latte")))
        );
        assert_eq!(
            parse_set_args(&[json!("/s.json"), json!("theme")]),
            Some(("/s.json".to_string(), "theme".to_string(), serde_json::Value::Null)),
            "a Lua nil arrives as a missing argument and means delete"
        );
        assert_eq!(parse_set_args(&[json!("/s.json"), json!(7)]), None, "a key is a string");
    }
}
