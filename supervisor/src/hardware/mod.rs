//! Non-D-Bus hardware/session-signal capabilities: sysfs/udev/inotify/logind-adjacent device
//! state, distinct from `dbus/`'s D-Bus-interface capabilities (see `dbus/mod.rs`'s own doc
//! comment for that boundary). `idle` is the first tenant -- it moved here from `dbus/idle.rs`
//! because it's majority Wayland-protocol code (`ext_idle_notifier_v1`) with one D-Bus proxy
//! (`login1.Manager.Inhibit`) riding along, not a D-Bus interface in its own right.

pub mod idle;
