//! NetworkManager, BlueZ, MPRIS, and Polkit D-Bus interfaces: `polkit`, `network`, `bluetooth`
//! (docs/adr/0030), `mpris` (docs/adr/0036), and `power` (UPower plus power-profiles-daemon,
//! docs/adr/0053). `idle` used to live here but moved to `hardware::idle` -- it's majority
//! Wayland-protocol code with one D-Bus proxy riding along, not a D-Bus interface in its own
//! right (see `hardware/mod.rs`'s doc comment).

pub mod bluetooth;
pub mod mpris;
pub mod network;
pub mod notifications;
pub mod polkit;
pub mod power;
mod shm_icons;
pub mod tray;

/// Shared `arguments: [en]` boolean-argument parse for `*:set_*_enabled(en)`-style write
/// actions -- reads the first argument as a JSON bool, or `None`.
pub fn parse_bool_arg(arguments: &[serde_json::Value]) -> Option<bool> {
    arguments.first()?.as_bool()
}
