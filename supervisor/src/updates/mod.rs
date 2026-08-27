//! `oblisk.updates` capability: Arch package update checking and installation via `alpm`
//! (ADR-0034). Fully separate from `oblisk.sysinfo`'s scheduler -- same interval-suspend-at-zero
//! *shape*, zero shared code, per the ADR's own instruction. Top-level, sibling to `hardware`/
//! `dbus`/`audio`/`privacy` (none of these five ADR-0034 capabilities are D-Bus interfaces).

pub mod check;
pub mod controller;
pub mod install;
pub mod pacman_conf;

pub use controller::{UpdatesController, UpdatesSignal, UpdatesState, parse_configure_args};
