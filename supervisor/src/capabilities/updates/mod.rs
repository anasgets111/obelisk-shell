//! `oblisk.updates` capability: package update checking and installation (ADR-0034).
//!
//! Separated into a schedule component and a backend abstraction (ADR-0134).
//! `backend.rs` defines the backend trait; `pacman/` implements it for Arch Linux.
//! Separate from `oblisk.sysinfo` scheduler with no shared code (ADR-0034).

pub mod backend;
pub mod controller;
pub mod pacman;

pub use controller::{UpdatesController, UpdatesSignal, parse_configure_args};

/// Every action `oblisk.updates:invoke(...)` accepts. `dispatch` matches this rather than a string,
/// so a variant with no arm (or an arm with no variant) fails the build.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UpdatesAction {
    Check,
    Configure,
    Install,
}

/// `oblisk.updates` action dispatch (ADR-0037): `check` and `configure` are synchronous,
/// sending scheduler channel requests (ADR-0034). `install` runs a package manager child
/// and is spawned asynchronously.
pub fn dispatch(controller: &UpdatesController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<UpdatesAction>(params) else { return };
    match action {
        UpdatesAction::Check => controller.check_now(),
        UpdatesAction::Configure => match parse_configure_args(&params.arguments) {
            Some(configure) => controller.configure(configure),
            None => crate::log_malformed_command(params),
        },
        UpdatesAction::Install => {
            let controller = controller.clone();
            tokio::spawn(async move {
                controller.install().await;
            });
        }
    }
}
