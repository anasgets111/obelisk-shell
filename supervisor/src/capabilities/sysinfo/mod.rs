//! `obelisk.sysinfo` provides CPU/RAM/swap/temperature telemetry with three independently
//! Lua-configurable poll intervals (ADR-0035).

pub mod controller;
pub mod cpu;
pub mod ram;
pub mod temp;

pub use controller::{SysinfoController, SysinfoSignal};

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SysinfoAction {
    /// Poll intervals in seconds.
    Configure { intervals: controller::SysinfoConfigure },
}

/// `configure` is synchronous: it rewrites shared config under its lock and nudges watch channels
/// (ADR-0037, ADR-0035).
pub fn dispatch(controller: &SysinfoController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<SysinfoAction>(&envelope.params) else { return };
    match action {
        SysinfoAction::Configure { intervals } => controller.configure(intervals),
    }
}
