//! `oblisk.workspaces` capability: per-output workspace state and the focused window
//! (`docs/oblisk-idl-api-specs.md` § 2.9), read off niri's IPC event stream (ADR-0056) or
//! Hyprland's event socket plus its command socket (ADR-0118).
//!
//! A top-level module rather than a tenant of `hardware/` or `dbus/`: it is a compositor's
//! Unix socket, not a device or a D-Bus interface.
//!
//! Two compositors, still no trait (ADR-0056 decision 1, ADR-0075 decision 4, ADR-0118): the
//! second implementor is built to its protocol and not live-tested, and the seam it plugs into
//! is `StatePublisher` for reads and two exhaustive-match arms for writes, which is all a trait
//! would give. A session with neither compositor never pushes, so `oblisk.workspaces` stays
//! `nil` -- § 2.9 has no absence sentinel, and an empty `outputs` array would read as "no
//! workspaces" rather than "nobody asked".
//!
//! The halves are split by file. `controller` holds § 2.9's payload, the reduction onto it, and
//! the publish contract, all in terms of its own row types; `niri` holds everything that names
//! `niri_ipc`, `hyprland` everything that names Hyprland's JSON.

pub mod controller;
pub mod hyprland;
pub mod niri;

pub use controller::{WorkspacesController, WorkspacesSignal, parse_focus_args, parse_toggle_special_args};

/// Every action `oblisk.workspaces:invoke(...)` accepts. `dispatch` matches this rather than a string,
/// so a variant with no arm (or an arm with no variant) fails the build.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspacesAction {
    Focus,
    ToggleSpecial,
}

/// `oblisk.workspaces`'s action dispatch (ADR-0037): both actions write to the compositor over
/// a fresh socket, which the controller does on its own thread, so each arm is a plain call
/// rather than a `tokio::spawn` (there is no future to drive).
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
