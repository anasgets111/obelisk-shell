//! PipeWire-backed audio state (build-steps.md Phase 6; § 2.4's master volume/mute added per
//! docs/adr/0053 decision 3, and its `sinks`/`sources` arrays plus real per-app volume/mute by
//! Phase 28 item 5). `mixer` tracks the registry and every list § 2.4 names; `master` holds the
//! pure parsing/resolution logic `mixer` wires PipeWire events through.
//!
//! BlueZ codec control (`docs/oblisk-supervisor-services-dbus.md` §6) is later work and belongs
//! to `bluetooth` rather than here.

pub mod master;
pub mod mixer;
