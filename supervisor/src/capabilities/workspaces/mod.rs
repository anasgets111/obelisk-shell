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

pub use controller::{WorkspacesController, WorkspacesSignal};

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WorkspacesAction {
    /// Focuses a `WorkspaceEntry.id`; the compositor ignores one that does not exist.
    Focus { id: u64 },
    /// Shows or hides a special workspace; Hyprland creates an unknown name.
    ToggleSpecial {
        #[serde(deserialize_with = "crate::capabilities::non_empty")]
        name: String,
    },
}

/// `obelisk.workspaces` action dispatch (ADR-0037): each action writes over a fresh compositor
/// socket on its own thread, so arms are plain calls rather than `tokio::spawn`.
pub fn dispatch(controller: &WorkspacesController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<WorkspacesAction>(&envelope.params) else { return };
    match action {
        WorkspacesAction::Focus { id } => controller.focus(id),
        WorkspacesAction::ToggleSpecial { name } => controller.toggle_special(&name),
    }
}
