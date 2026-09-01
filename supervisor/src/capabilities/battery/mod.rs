//! `oblisk.battery` capability: the system battery's presence, percentage, charge state and
//! time estimates (docs/oblisk-idl-api-specs.md § 2.2), read off UPower's `DisplayDevice`.
//! Read-only -- no write actions, so no `dispatch` function here.
//!
//! It read `/sys/class/power_supply` behind a udev watch until ADR-0080: that watch never
//! fired for a capacity change on this hardware, and sysfs has no word for "the charge limit is
//! reached" that a config could tell apart from "you are on battery".

pub mod controller;

pub use controller::{BatteryController, BatterySignal};
