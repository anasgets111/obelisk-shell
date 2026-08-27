//! `oblisk.sysinfo` capability: CPU/RAM/swap/temperature telemetry, three independently
//! Lua-configurable poll intervals (docs/adr/0035).

pub mod controller;
pub mod cpu;
pub mod ram;
pub mod temp;

pub use controller::{SysinfoController, SysinfoSignal, parse_configure_args};

/// `oblisk.sysinfo`'s action dispatch (ADR-0037): `configure` is synchronous (it only rewrites
/// the shared config under its lock and nudges the watch channels -- docs/adr/0035), so nothing
/// here spawns.
pub fn dispatch(controller: &SysinfoController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    match params.action.as_str() {
        "configure" => match parse_configure_args(&params.arguments) {
            Some(cfg) => controller.configure(cfg),
            None => crate::log_malformed_command(params),
        },
        _ => crate::log_unknown_action(params),
    }
}
