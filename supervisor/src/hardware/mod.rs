//! Non-D-Bus hardware/session-signal capabilities: sysfs/udev/inotify/logind-adjacent device
//! state, distinct from `dbus/`'s D-Bus-interface capabilities. Each capability here is
//! majority non-D-Bus work, with at most one D-Bus proxy riding the shared bus connection for
//! a single write action: `idle` (`ext_idle_notifier_v1` + `login1.Manager.Inhibit`),
//! `sysinfo` (no D-Bus, docs/adr/0035), `keyboard` (+ `UPower.KbdBacklight`, docs/adr/0034),
//! `battery` (no D-Bus, docs/adr/0053), `brightness` (+ `login1.Session.SetBrightness`,
//! docs/adr/0053). `brightness`/`keyboard` share their raw/percent scaling math in `scale`.

use std::path::Path;

/// Reads and trims one sysfs attribute file under `entry_dir`. `None` for both "file missing"
/// and any other read error -- callers don't distinguish absent from unreadable: a device that
/// can't be read is treated as a device that isn't there.
pub fn read_attr(entry_dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(entry_dir.join(name)).ok().map(|text| text.trim().to_string())
}

pub mod battery;
pub mod brightness;
pub mod idle;
pub mod keyboard;
pub mod scale;
pub mod sysinfo;
