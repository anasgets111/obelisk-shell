//! NetworkManager, BlueZ, MPRIS, and Polkit D-Bus interfaces (build-steps.md §1). `polkit`
//! (Phase 5), `network` (Phase 16), and `bluetooth` (docs/adr/0030) exist so far; MPRIS is a
//! later phase.

pub mod bluetooth;
pub mod idle;
pub mod network;
pub mod polkit;
pub mod tray;

/// Shared `arguments: [en]` boolean-argument parse for `*:set_*_enabled(en)`-style write actions
/// -- `network::parse_bool_arg` and `bluetooth::parse_bool_arg` both re-export this rather than
/// each defining their own copy, since the shape (read the first argument as a JSON bool, or
/// `None`) isn't capability-specific.
pub fn parse_bool_arg(arguments: &[serde_json::Value]) -> Option<bool> {
    arguments.first()?.as_bool()
}
