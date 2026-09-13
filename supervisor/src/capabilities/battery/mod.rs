//! `obelisk.battery` reports presence, percentage, charge state, and time estimates from UPower's
//! `DisplayDevice`. Read-only, with no `dispatch`.
//!
//! The former `/sys/class/power_supply` udev watch missed capacity changes on this hardware and
//! could not distinguish a reached charge limit from running on battery (ADR-0080).

pub mod controller;

pub use controller::{BatteryController, BatterySignal};
