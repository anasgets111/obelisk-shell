//! `oblisk.files` keeps a config-requested folder listing current through inotify (ADR-0120).
//! Plain filesystem reads and one kernel watch per folder, with no D-Bus proxy or hardware thread.
//!
//! The config VM has no `io` (ADR-0048); `process.run("ls")` would parse lines for a table the
//! Supervisor can provide. The first caller was a wallpaper picker, but screenshot trays, download
//! shelves, and file browsers need the same folder watcher.

pub mod controller;

pub use controller::{FilesController, FilesSignal};

/// Every action `oblisk.files:invoke(...)` accepts. `dispatch` matches this rather than a string,
/// so a variant with no arm (or an arm with no variant) fails the build.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FilesAction {
    Watch,
    Unwatch,
}

/// `files:watch(path, extensions?)`'s `arguments: [path, extensions?]`: an absolute folder path,
/// then an optional array of extensions without the dot (`{ "jpg", "png" }`), matched
/// case-insensitively. No list means every file. Anything in the list that is not a string is
/// the whole command being malformed, not one entry being skipped.
pub fn parse_watch_args(arguments: &[serde_json::Value]) -> Option<(String, Vec<String>)> {
    let path = arguments.first()?.as_str()?;
    if !path.starts_with('/') {
        return None;
    }
    let extensions = match arguments.get(1) {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(|ext| ext.trim_start_matches('.').to_ascii_lowercase()))
            .collect::<Option<Vec<String>>>()?,
        Some(_) => return None,
    };
    Some((path.to_string(), extensions))
}

/// `files:unwatch(path)`'s one argument, on `watch`'s terms.
pub fn parse_unwatch_args(arguments: &[serde_json::Value]) -> Option<String> {
    let path = arguments.first()?.as_str()?;
    path.starts_with('/').then(|| path.to_string())
}

/// `oblisk.files`'s action dispatch (ADR-0037). Synchronous: `watch` spawns the listing and the
/// inotify loop as a task and returns, and `unwatch` aborts that task.
pub fn dispatch(controller: &FilesController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<FilesAction>(params) else { return };
    match action {
        FilesAction::Watch => match parse_watch_args(&params.arguments) {
            Some((path, extensions)) => controller.watch(&path, extensions),
            None => crate::log_malformed_command(params),
        },
        FilesAction::Unwatch => match parse_unwatch_args(&params.arguments) {
            Some(path) => controller.unwatch(&path),
            None => crate::log_malformed_command(params),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn watch_args_take_a_path_and_an_optional_extension_list_lowercased_without_dots() {
        assert_eq!(parse_watch_args(&[json!("/walls")]), Some(("/walls".to_string(), vec![])));
        assert_eq!(
            parse_watch_args(&[json!("/walls"), json!(["JPG", ".png"])]),
            Some(("/walls".to_string(), vec!["jpg".to_string(), "png".to_string()]))
        );
        assert_eq!(parse_watch_args(&[json!("/walls"), json!(null)]), Some(("/walls".to_string(), vec![])));
    }

    #[test]
    fn watch_args_refuse_a_relative_path_and_a_non_string_extension() {
        assert_eq!(parse_watch_args(&[json!("walls")]), None);
        assert_eq!(parse_watch_args(&[json!("/walls"), json!([1])]), None);
        assert_eq!(parse_watch_args(&[json!("/walls"), json!("jpg")]), None);
        assert_eq!(parse_watch_args(&[]), None);
    }

    #[test]
    fn unwatch_args_take_one_absolute_path() {
        assert_eq!(parse_unwatch_args(&[json!("/walls")]), Some("/walls".to_string()));
        assert_eq!(parse_unwatch_args(&[json!("walls")]), None);
    }
}
