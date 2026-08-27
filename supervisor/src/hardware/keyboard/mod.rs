//! `oblisk.keyboard` capability: backlight (UPower), lock state, and layout (docs/adr/0034).
//! Backlight is the first half wired in; locks and layout join the same
//! `Arc<Mutex<KeyboardState>>` and shared signal channel in the same shape once built.

pub mod backlight;
pub mod controller;
pub mod locks;

pub use controller::{KeyboardController, KeyboardSignal, KeyboardState, parse_set_backlight_args};
