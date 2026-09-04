//! `oblisk.updates` capability: package update checking and installation (ADR-0034), split into
//! a schedule that knows nothing about package managers and a backend that is nothing but one
//! (ADR-0134). `backend.rs` holds the trait and picks this machine's implementation; `pacman/`
//! is the one implementation shipped, and `controller.rs` never names it.
//!
//! Fully separate from `oblisk.sysinfo`'s scheduler -- same shape, zero shared code, per
//! ADR-0034's own instruction. Top-level, sibling to `hardware`/`dbus`/`audio`/`privacy`.

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

/// `oblisk.updates`'s action dispatch (ADR-0037): `check` and `configure` are synchronous (each
/// only nudges the scheduler through a channel -- ADR-0034); `install` runs a real `pacman` child
/// and gets `tokio::spawn`ed.
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
