//! `oblisk.system` capability: reactive wall-clock time and the persisted `state.json`
//! dictionary (docs/oblisk-idl-api-specs.md §2.11). Top-level, sibling to `hardware`/`dbus`/
//! `privacy`/`audio` -- same ADR-0034 module-layout reasoning `privacy/mod.rs` states: this is
//! plain filesystem I/O plus a timer, not a hardware-thread/D-Bus-proxy mix.
//!
//! Read-only for now: `system:write_state` and `system:find_icon` are separate IDL rows (§3.2)
//! with no `dispatch` here, because nothing in this codebase can call them yet -- building the
//! write path ahead of a caller would be dead code by construction.

pub mod controller;
pub mod paths;
pub mod state;

pub use controller::{SystemController, SystemSignal};
