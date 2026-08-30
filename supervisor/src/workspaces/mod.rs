//! `oblisk.workspaces` capability: per-output workspace state and the focused window
//! (`docs/oblisk-idl-api-specs.md` § 2.9), read off niri's IPC event stream (docs/adr/0056).
//!
//! A top-level module rather than a tenant of `hardware/` or `dbus/`: it is one compositor's
//! Unix socket, not a device or a D-Bus interface.
//!
//! One compositor, no trait (docs/adr/0056 decision 1): a trait with one implementor is
//! speculative generality. A session that is not niri never pushes, so `oblisk.workspaces`
//! stays `nil` -- § 2.9 has no absence sentinel, and an empty `outputs` array would read as
//! "no workspaces" rather than "nobody asked".

pub mod controller;

pub use controller::{WorkspacesController, WorkspacesSignal, parse_focus_args};

/// `oblisk.workspaces`'s action dispatch (ADR-0037): `focus` writes to the compositor over a
/// fresh socket, which [`WorkspacesController::focus`] does on its own thread, so this arm is a
/// plain call rather than a `tokio::spawn` (there is no future to drive).
pub fn dispatch(controller: &WorkspacesController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    match params.action.as_str() {
        "focus" => match parse_focus_args(&params.arguments) {
            Some(id) => controller.focus(id),
            None => crate::log_malformed_command(params),
        },
        _ => crate::log_unknown_action(params),
    }
}
