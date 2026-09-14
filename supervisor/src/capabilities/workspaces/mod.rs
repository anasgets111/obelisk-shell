//! `obelisk.workspaces`: per-output workspace state and focused window, from niri's IPC stream
//! (ADR-0056) or Hyprland's event and command sockets (ADR-0118).
//!
//! Top-level because this is a compositor Unix socket, not a device or D-Bus interface.
//!
//! Two compositors still use no trait (ADR-0056 decision 1, ADR-0075 decision 4, ADR-0118): the
//! protocol-specific seam is `StatePublisher` for reads plus two exhaustive write arms. With
//! neither compositor, nothing pushes and `obelisk.workspaces` stays `nil`; the payload has no
//! absence sentinel, and `outputs: []` would mean no workspaces rather than no answer.
//!
//! `controller` holds the payload, reduction, and publish contract; `niri` owns `niri_ipc`, and
//! `hyprland` owns Hyprland JSON.

pub mod controller;
pub mod hyprland;
pub mod niri;

pub use controller::{WorkspacesController, WorkspacesSignal, parse_focus_args, parse_toggle_special_args};

#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspacesAction {
    /// (id: integer) Focuses a `WorkspaceEntry.id`.
    Focus,
    /// (name: string) Shows or hides a special workspace.
    ToggleSpecial,
}

/// `obelisk.workspaces` action dispatch (ADR-0037): each action writes over a fresh compositor
/// socket on its own thread, so arms are plain calls rather than `tokio::spawn`.
pub fn dispatch(controller: &WorkspacesController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<WorkspacesAction>(params) else { return };
    match action {
        WorkspacesAction::Focus => match parse_focus_args(&params.arguments) {
            Some(id) => controller.focus(id),
            None => crate::log_malformed_command(params),
        },
        WorkspacesAction::ToggleSpecial => match parse_toggle_special_args(&params.arguments) {
            Some(name) => controller.toggle_special(name),
            None => crate::log_malformed_command(params),
        },
    }
}
