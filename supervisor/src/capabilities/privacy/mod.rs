//! `oblisk.privacy` capability: camera-in-use detection (ADR-0034). Kernel-level `/dev/videoN`
//! opener detection (`video.rs`) is primary; PipeWire `Video/Source` node classification
//! (`audio::mixer`'s already-running registry thread, no second PipeWire connection) is a
//! name-enrichment layer only, for the portal-routed subset of camera users it can see.
//! Top-level, sibling to `hardware`/`dbus`/`audio`: sysfs/procfs/inotify plus a PipeWire
//! enrichment feed, not a hardware-thread/D-Bus-proxy mix.

pub mod controller;
pub mod video;

pub use controller::{PrivacyController, PrivacySignal};
