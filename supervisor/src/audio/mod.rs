//! PipeWire-backed audio state (build-steps.md Phase 6; § 2.4's master volume/mute added per
//! docs/adr/0053 decision 3). `mixer` tracks per-app streams and § 2.4's master output
//! volume/mute; `master` holds the pure parsing/resolution logic `mixer` wires PipeWire events
//! through. Default sink/source *routing* (writing a new default, not just reading the current
//! one) and BlueZ codec control (`docs/oblisk-supervisor-services-dbus.md` §6) are later work --
//! nothing here is a write path yet.

pub mod master;
pub mod mixer;
