//! `obelisk.applications`: installed `.desktop` entries (ADR-0061). A top-level capability using
//! plain filesystem reads, with no D-Bus proxy or hardware thread.
//!
//! ADR-0054 decision 5 called for this when a window needed an icon it did not report. The
//! launcher needs every name/icon/command, a focused window has `app_id` but no icon, and a tray
//! item may have neither `IconName` nor `IconPixmap`. Enumerate instead of the synchronous
//! `system:find_icon(app_id, ...)`: the control socket has no reply shape for it, and launchers
//! need the whole list.

pub mod controller;
pub mod entry;
pub mod scan;

pub use controller::{ApplicationsController, ApplicationsSignal, LaunchError, OpenUrlError};
pub use scan::application_dirs;

#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ApplicationsAction {
    /// () Rescans installed desktop entries.
    Refresh,
    /// (id: string) Launches the `entries[].id` desktop entry.
    Launch,
    /// (url: string) Opens a URL with `xdg-open`.
    OpenUrl,
}

/// `obelisk.applications` action dispatch (ADR-0037). `refresh` calls `spawn_blocking`; `launch`
/// and `open_url` spawn detached children without waiting.
pub fn dispatch(controller: &ApplicationsController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<ApplicationsAction>(params) else { return };
    match action {
        ApplicationsAction::Refresh => controller.refresh(),
        ApplicationsAction::Launch => match params.arguments.first().and_then(serde_json::Value::as_str) {
            Some(id) => {
                if let Err(err) = controller.launch(id) {
                    let reason = match err {
                        LaunchError::Unknown => format!("no application entry with id {id:?}"),
                        LaunchError::NoTerminal => {
                            format!(
                                "{id:?} declares Terminal=true and $TERMINAL is unset, so there is no emulator to run it in"
                            )
                        }
                        LaunchError::Spawn(message) => format!("spawning {id:?} failed: {message}"),
                    };
                    eprintln!("applications:launch: {reason}");
                }
            }
            None => crate::log_malformed_command(params),
        },
        ApplicationsAction::OpenUrl => match params.arguments.first().and_then(serde_json::Value::as_str) {
            Some(url) => {
                if let Err(err) = controller.open_url(url) {
                    let reason = match err {
                        OpenUrlError::Refused(why) => format!("refused {url:?}: {why}"),
                        OpenUrlError::Spawn(message) => format!("spawning xdg-open for {url:?} failed: {message}"),
                    };
                    eprintln!("applications:open_url: {reason}");
                }
            }
            None => crate::log_malformed_command(params),
        },
    }
}
