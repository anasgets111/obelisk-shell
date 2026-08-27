//! `oblisk.privacy` capability: camera-in-use detection (ADR-0034). Kernel-level `/dev/videoN`
//! opener detection (`video.rs`) is primary; PipeWire `Video/Source` node classification
//! (`audio::mixer`, the *already-running* registry thread, no second PipeWire connection) is a
//! name-enrichment layer only, for the portal-routed subset of camera users it can see. Top-
//! level, sibling to `hardware`/`dbus`/`audio` (ADR-0034's own module-layout decision: none of
//! these five capabilities are D-Bus interfaces, and privacy specifically is sysfs/procfs/
//! inotify plus a PipeWire enrichment feed, not a hardware-thread/D-Bus-proxy mix like
//! `hardware::keyboard`).

pub mod controller;
pub mod video;

pub use controller::{PrivacyController, PrivacySignal};
