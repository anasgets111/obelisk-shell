//! `oblisk.applications` capability: the installed `.desktop` entries, enumerated
//! (docs/adr/0061). Top-level, sibling to `system`/`updates`/`privacy` -- plain filesystem
//! reads, no D-Bus proxy and no hardware thread.
//!
//! This is the capability ADR-0054 decision 5 said would arrive "the day something needs an icon
//! for a window that is not already telling us its icon". Three callers arrived at once: an
//! application launcher needs every entry's name, icon and command; a focused-window readout has
//! an `app_id` and no icon; and a tray item can report neither an `IconName` nor an
//! `IconPixmap`. Enumeration rather than § 3.2's `system:find_icon(app_id, ...)`, because that
//! row is a synchronous call the control socket has no reply shape for, and because a launcher
//! wants the whole list rather than one lookup at a time.

pub mod controller;
pub mod entry;
pub mod scan;

pub use controller::{ApplicationsController, ApplicationsSignal, LaunchError};
pub use scan::application_dirs;

/// Every action `oblisk.applications:invoke(...)` accepts. `dispatch` matches this rather than a string,
/// so a variant with no arm (or an arm with no variant) fails the build.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationsAction {
    Refresh,
    Launch,
}

/// `oblisk.applications`'s action dispatch (ADR-0037). Both actions are synchronous here:
/// `refresh` hands the actual scan to `spawn_blocking` itself, and `launch` spawns a detached
/// child without waiting for it.
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
    }
}
