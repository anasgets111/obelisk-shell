//! `obelisk.sysinfo` provides CPU/RAM/swap/temperature telemetry with three independently
//! Lua-configurable poll intervals (ADR-0035).

pub mod controller;
pub mod cpu;
pub mod ram;
pub mod temp;

pub use controller::{SysinfoController, SysinfoSignal, parse_configure_args};

#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SysinfoAction {
    /// (intervals: { cpu_interval?: integer, ram_interval?: integer, temp_interval?: integer }) Seconds.
    Configure,
}

/// `configure` is synchronous: it rewrites shared config under its lock and nudges watch channels
/// (ADR-0037, ADR-0035).
pub fn dispatch(controller: &SysinfoController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<SysinfoAction>(params) else { return };
    match action {
        SysinfoAction::Configure => match parse_configure_args(&params.arguments) {
            Some(cfg) => controller.configure(cfg),
            None => crate::log_malformed_command(params),
        },
    }
}
