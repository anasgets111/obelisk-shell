//! `obelisk.files` keeps a config-requested folder listing current through inotify (ADR-0120).
//! Plain filesystem reads and one kernel watch per folder, with no D-Bus proxy or hardware thread.
//!
//! The config VM has no `io` (ADR-0048); `process.run("ls")` would parse lines for a table the
//! Supervisor can provide. The first caller was a wallpaper picker, but screenshot trays, download
//! shelves, and file browsers need the same folder watcher.

pub mod controller;

pub use controller::{FilesController, FilesSignal};

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FilesAction {
    /// Lists an absolute folder into `folders[path]`. `extensions` are matched case-insensitively,
    /// with or without the dot; none means every file.
    Watch {
        #[serde(deserialize_with = "absolute")]
        path: String,
        #[serde(default, deserialize_with = "crate::capabilities::lua_list")]
        extensions: Vec<String>,
    },
    /// Stops a `watch` on this path.
    Unwatch {
        #[serde(deserialize_with = "absolute")]
        path: String,
    },
}

fn absolute<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let path = <String as serde::Deserialize>::deserialize(deserializer)?;
    if path.starts_with('/') { Ok(path) } else { Err(serde::de::Error::custom("expected an absolute path")) }
}

/// `obelisk.files`'s action dispatch (ADR-0037). Synchronous: `watch` spawns the listing and the
/// inotify loop as a task and returns, and `unwatch` aborts that task.
pub fn dispatch(controller: &FilesController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<FilesAction>(&envelope.params) else { return };
    match action {
        FilesAction::Watch { path, extensions } => controller
            .watch(&path, extensions.iter().map(|ext| ext.trim_start_matches('.').to_ascii_lowercase()).collect()),
        FilesAction::Unwatch { path } => controller.unwatch(&path),
    }
}
