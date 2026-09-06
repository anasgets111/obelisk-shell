//! `oblisk.system` provides reactive wall-clock time (docs/oblisk-idl-api-specs.md §2.11).
//!
//! Read-only. ADR-0136 moved the former persisted `state.json` and `system:write_state` to
//! `oblisk.storage`, where config names the file.
//!
//! `system:find_icon` remains an undispatched IDL row because nothing calls it yet.

pub mod controller;

pub use controller::{SystemController, SystemSignal};
