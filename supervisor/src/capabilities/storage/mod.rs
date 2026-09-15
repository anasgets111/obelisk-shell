//! `obelisk.storage` owns JSON files declared with `persistent_table` (ADR-0136). Plain filesystem
//! reads/writes, sibling to `files`/`system`, with no D-Bus proxy or hardware thread.
//!
//! The config chooses each path, name, and defaults; "settings", "state", and "cache" are not
//! Supervisor concepts. This replaced `system:write_state` and its hardcoded `state.json`.

pub mod controller;

pub use controller::{StorageController, StorageSignal};

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum StorageAction {
    /// Declares an absolute JSON file, the Renderer having joined `path` and `name`; `defaults`
    /// fill missing keys.
    Open {
        path: String,
        #[serde(default)]
        defaults: Option<serde_json::Map<String, serde_json::Value>>,
    },
    /// Writes a key; `nil` deletes it.
    Set {
        path: String,
        key: String,
        #[serde(default)]
        value: serde_json::Value,
    },
}

/// `obelisk.storage` action dispatch (ADR-0037). Synchronous: actions touch memory and schedule the
/// save task.
pub fn dispatch(controller: &StorageController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<StorageAction>(&envelope.params) else { return };
    match action {
        StorageAction::Open { path, defaults } => {
            controller.open(&path, &serde_json::Value::Object(defaults.unwrap_or_default()))
        }
        StorageAction::Set { path, key, value } => controller.set(&path, &key, value),
    }
}
