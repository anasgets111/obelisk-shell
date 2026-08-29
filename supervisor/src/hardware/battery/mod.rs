//! `oblisk.battery` capability: system battery presence/percent/charging state
//! (docs/oblisk-idl-api-specs.md § 2.2), sysfs-only -- no D-Bus proxy, unlike `keyboard`'s
//! UPower-backed backlight half. Sibling to `idle`/`sysinfo`/`keyboard` under `hardware` (see
//! `hardware/mod.rs`'s own doc comment for that module-layout boundary). Closest in shape to
//! `privacy`: a sysfs/procfs, read-only capability with a controller, a single-variant signal
//! enum, and an unbounded channel back to `main.rs` (see `privacy/mod.rs`'s own doc comment for
//! the analogy this was built against) -- `battery` has no write actions, so there is no
//! `dispatch` function here either, matching `privacy`'s own read-only shape.

pub mod controller;

pub use controller::{BatteryController, BatterySignal};
