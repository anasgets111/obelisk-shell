//! Non-D-Bus hardware/session-signal capabilities: sysfs/udev/inotify/logind-adjacent device
//! state, distinct from `dbus/`'s D-Bus-interface capabilities (see `dbus/mod.rs`'s own doc
//! comment for that boundary). `idle` is the first tenant -- it moved here from `dbus/idle.rs`
//! because it's majority Wayland-protocol code (`ext_idle_notifier_v1`) with one D-Bus proxy
//! (`login1.Manager.Inhibit`) riding along, not a D-Bus interface in its own right. `sysinfo`
//! (docs/adr/0035) is the second: pure sysfs/procfs parsing, no D-Bus involvement at all.
//! `keyboard` (docs/adr/0034) is the third: mostly sysfs/evdev/compositor-socket, with one
//! D-Bus proxy (`UPower.KbdBacklight`) riding the shared system-bus connection for its
//! backlight half, the same "mostly non-D-Bus, one proxy riding along" shape `idle` already has.

pub mod battery;
pub mod idle;
pub mod keyboard;
pub mod sysinfo;
