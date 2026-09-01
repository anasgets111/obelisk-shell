//! `oblisk.battery` capability: system battery presence/percent/charging state
//! (docs/oblisk-idl-api-specs.md § 2.2), sysfs-only, no D-Bus proxy. Read-only -- no write
//! actions, so no `dispatch` function here.

pub mod controller;

pub use controller::{BatteryController, BatterySignal};
