//! NetworkManager, BlueZ, MPRIS, and Polkit D-Bus interfaces (build-steps.md §1). `polkit`
//! (Phase 5), `network` (Phase 16), and `bluetooth` (docs/adr/0030) exist so far; MPRIS is a
//! later phase. `idle` used to live here but moved to `hardware::idle` -- it's majority
//! Wayland-protocol code with one D-Bus proxy riding along, not a D-Bus interface in its own
//! right (see `hardware/mod.rs`'s doc comment).

pub mod bluetooth;
pub mod network;
pub mod notifications;
pub mod polkit;
mod shm_icons;
pub mod tray;

/// Shared `arguments: [en]` boolean-argument parse for `*:set_*_enabled(en)`-style write actions
/// -- `network::parse_bool_arg` and `bluetooth::parse_bool_arg` both re-export this rather than
/// each defining their own copy, since the shape (read the first argument as a JSON bool, or
/// `None`) isn't capability-specific.
pub fn parse_bool_arg(arguments: &[serde_json::Value]) -> Option<bool> {
    arguments.first()?.as_bool()
}
