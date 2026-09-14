//! `obelisk.updates` capability: package update checking and installation (ADR-0034).
//!
//! Separates scheduling from the backend abstraction (ADR-0134): `backend.rs` defines the trait and
//! `pacman/` implements it for Arch. The scheduler is independent of `obelisk.sysinfo` (ADR-0034).

pub mod backend;
pub mod controller;
pub mod pacman;

pub use controller::{UpdatesController, UpdatesSignal, parse_configure_args};

#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UpdatesAction {
    /// () Checks for upgrades now.
    Check,
    /// (config: { interval: integer, checked_at?: integer, packages?: UpdateCandidate[] }) Seconds, `0` for none.
    Configure,
    /// () Installs pending upgrades.
    Install,
}

/// `obelisk.updates` dispatch (ADR-0037): `check`/`configure` send scheduler requests synchronously
/// (ADR-0034); `install` spawns the package-manager child.
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
