//! `oblisk.system` capability: reactive wall-clock time and the persisted `state.json`
//! dictionary (docs/oblisk-idl-api-specs.md §2.11). Top-level, sibling to `hardware`/`dbus`/
//! `privacy`/`audio` -- plain filesystem I/O plus a timer, not a hardware-thread/D-Bus-proxy mix.
//!
//! `system:write_state` is here (§3.2); `system:find_icon` is still a separate IDL row with no
//! dispatch, because nothing in this codebase can call it yet.

pub mod controller;
pub mod paths;
pub mod state;

pub use controller::{SystemController, SystemSignal};

/// Every action `oblisk.system:invoke(...)` accepts. `dispatch` matches this rather than a string,
/// so a variant with no arm (or an arm with no variant) fails the build.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SystemAction {
    WriteState,
}

/// `system:write_state(key, val)`'s `arguments: [key, val]` -- positional, unlike `sysinfo`'s and
/// `updates`' table argument, because §3.2 spells it that way and two positional arguments need no
/// names to be read correctly.
pub fn parse_write_state_args(arguments: &[serde_json::Value]) -> Option<(String, serde_json::Value)> {
    let key = arguments.first()?.as_str()?.to_string();
    let value = arguments.get(1)?.clone();
    Some((key, value))
}

/// `oblisk.system`'s action dispatch (ADR-0037). Synchronous: the write is a small file rewritten
/// under the controller's own lock, so there is nothing to spawn.
pub fn dispatch(controller: &SystemController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<SystemAction>(params) else { return };
    match action {
        SystemAction::WriteState => match parse_write_state_args(&params.arguments) {
            Some((key, value)) => controller.write_state(&key, value),
            None => crate::log_malformed_command(params),
        },
    }
}
