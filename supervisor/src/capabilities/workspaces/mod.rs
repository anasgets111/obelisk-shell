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
//!
//! The two halves are split by file rather than by trait. `controller` holds § 2.9's payload,
//! the reduction onto it, and the publish contract, all in terms of its own row types; `niri`
//! holds everything that names `niri_ipc`. That is where a trait would eventually go, and until
//! a second implementor is live-tested it is a module boundary instead -- which keeps ADR-0056's
//! decision while making its own stated upgrade path cheap.

pub mod controller;
pub mod niri;

pub use controller::{WorkspacesController, WorkspacesSignal, parse_focus_args};

/// Every action `oblisk.workspaces:invoke(...)` accepts. `dispatch` matches this rather than a string,
/// so a variant with no arm (or an arm with no variant) fails the build.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspacesAction {
    Focus,
}

/// `oblisk.workspaces`'s action dispatch (ADR-0037): `focus` writes to the compositor over a
/// fresh socket, which [`WorkspacesController::focus`] does on its own thread, so this arm is a
/// plain call rather than a `tokio::spawn` (there is no future to drive).
pub fn dispatch(controller: &WorkspacesController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<WorkspacesAction>(params) else { return };
    match action {
        WorkspacesAction::Focus => match parse_focus_args(&params.arguments) {
            Some(id) => controller.focus(id),
            None => crate::log_malformed_command(params),
        },
    }
}
