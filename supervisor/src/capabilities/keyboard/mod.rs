//! `obelisk.keyboard` combines UPower backlight, lock state, and compositor layout (ADR-0034) in
//! one `Arc<Mutex<KeyboardState>>` and signal channel.

pub mod backlight;
pub mod controller;
pub mod layout;
pub mod locks;

pub use controller::{KeyboardController, KeyboardSignal};

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum KeyboardAction {
    /// Sets the keyboard backlight, `0` to `100`.
    SetBacklight { percent: u64 },
    /// Switches to the 0-based configured layout.
    SwitchLayout { index: usize },
}

/// `set_backlight` is a spawned D-Bus write (ADR-0037, ADR-0029); `switch_layout` forwards
/// synchronously through the compositor link (ADR-0034).
pub fn dispatch(controller: &KeyboardController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<KeyboardAction>(&envelope.params) else { return };
    match action {
        KeyboardAction::SetBacklight { percent } => {
            let controller = controller.clone();
            tokio::spawn(async move { controller.set_backlight(percent).await });
        }
        KeyboardAction::SwitchLayout { index } => controller.switch_layout(index),
    }
}
