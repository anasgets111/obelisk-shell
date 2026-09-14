//! `obelisk.keyboard` combines UPower backlight, lock state, and compositor layout (ADR-0034) in
//! one `Arc<Mutex<KeyboardState>>` and signal channel.

pub mod backlight;
pub mod controller;
pub mod layout;
pub mod locks;

pub use controller::{KeyboardController, KeyboardSignal, parse_set_backlight_args, parse_switch_layout_args};

#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KeyboardAction {
    /// (percent: integer) Sets the keyboard backlight, `0` to `100`.
    SetBacklight,
    /// (index: integer) Switches to the 0-based configured layout.
    SwitchLayout,
}

/// `set_backlight` is a spawned D-Bus write (ADR-0037, ADR-0029); `switch_layout` forwards
/// synchronously through the compositor link (ADR-0034).
pub fn dispatch(controller: &KeyboardController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<KeyboardAction>(params) else { return };
    match action {
        KeyboardAction::SetBacklight => match parse_set_backlight_args(&params.arguments) {
            Some(pct) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.set_backlight(pct).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        KeyboardAction::SwitchLayout => match parse_switch_layout_args(&params.arguments) {
            Some(index) => controller.switch_layout(index),
            None => crate::log_malformed_command(params),
        },
    }
}
