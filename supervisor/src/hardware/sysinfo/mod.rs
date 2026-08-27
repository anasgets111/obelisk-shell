//! `oblisk.sysinfo` capability: CPU/RAM/swap/temperature telemetry, three independently
//! Lua-configurable poll intervals (docs/adr/0035).

pub mod controller;
pub mod cpu;
pub mod ram;
pub mod temp;

pub use controller::{SysinfoController, SysinfoSignal, SysinfoState, parse_configure_args};
