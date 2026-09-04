//! `oblisk.system` capability: reactive wall-clock time (docs/oblisk-idl-api-specs.md §2.11).
//!
//! Read-only, and a clock is all that is left of it. The persisted `state.json` dictionary and
//! `system:write_state` were here until ADR-0136 moved persistence to `oblisk.storage`, where the
//! config names the file instead of this module naming it.
//!
//! `system:find_icon` is still a separate IDL row with no dispatch, because nothing in this
//! codebase can call it yet.

pub mod controller;

pub use controller::{SystemController, SystemSignal};
