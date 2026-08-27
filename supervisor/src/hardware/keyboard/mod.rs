//! `oblisk.keyboard` capability: backlight (UPower), lock state, and layout (docs/adr/0034),
//! all wired in, sharing one `Arc<Mutex<KeyboardState>>` and one shared signal channel.

pub mod backlight;
pub mod controller;
pub mod layout;
pub mod locks;

pub use controller::{KeyboardController, KeyboardSignal, KeyboardState, parse_set_backlight_args, parse_switch_layout_args};
