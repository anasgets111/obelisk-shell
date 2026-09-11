//! `obelisk.privacy` detects camera use (ADR-0034). Kernel `/dev/videoN` opener detection
//! (`video.rs`) is primary; PipeWire `Video/Source` classification from `audio::mixer`'s existing
//! registry thread only enriches names for the portal-routed subset. No second PipeWire
//! connection, hardware thread, or D-Bus proxy.

pub mod controller;
pub mod video;

pub use controller::{PrivacyController, PrivacySignal};
