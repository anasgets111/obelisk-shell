//! Non-D-Bus hardware/session-signal capabilities: sysfs/udev/inotify/logind-adjacent device
//! state, distinct from `dbus/`'s D-Bus-interface capabilities (see `dbus/mod.rs`'s own doc
//! comment for that boundary). `idle` is the first tenant -- it moved here from `dbus/idle.rs`
//! because it's majority Wayland-protocol code (`ext_idle_notifier_v1`) with one D-Bus proxy
//! (`login1.Manager.Inhibit`) riding along, not a D-Bus interface in its own right. `sysinfo`
//! (docs/adr/0035) is the second: pure sysfs/procfs parsing, no D-Bus involvement at all.
//! `keyboard` (docs/adr/0034) is the third: mostly sysfs/evdev/compositor-socket, with one
//! D-Bus proxy (`UPower.KbdBacklight`) riding the shared system-bus connection for its
//! backlight half, the same "mostly non-D-Bus, one proxy riding along" shape `idle` already has.
//! `battery` (docs/adr/0053) is the fourth: sysfs-only, no D-Bus at all. `brightness`
//! (docs/adr/0053) is the fifth: sysfs/udev reads with one D-Bus proxy (`login1.Session.
//! SetBrightness`) riding the shared connection for its write side, the same shape `keyboard`
//! already has -- and its raw/percent scaling, plus `keyboard`'s own, both live in `scale`
//! rather than in either capability, since the math has no capability-specific behavior in it.

use std::path::Path;

/// Reads and trims one sysfs attribute file under `entry_dir`, `None` for both "file missing" and
/// any other read error.
///
/// Here rather than in a capability because `battery` and `brightness` both walk a sysfs class
/// directory reading single-line attributes off each entry, and had a byte-identical copy each.
/// Not distinguishing absent from unreadable is the shared convention: every caller treats a
/// device it cannot read as a device that is not there, and no caller has anything useful to do
/// with the difference.
pub fn read_attr(entry_dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(entry_dir.join(name)).ok().map(|text| text.trim().to_string())
}

pub mod battery;
pub mod brightness;
pub mod idle;
pub mod keyboard;
pub mod scale;
pub mod sysinfo;
