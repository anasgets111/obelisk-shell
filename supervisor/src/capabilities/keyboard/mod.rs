//! `oblisk.keyboard` capability: backlight (UPower), lock state, and layout (ADR-0034),
//! all wired in, sharing one `Arc<Mutex<KeyboardState>>` and one shared signal channel.

pub mod backlight;
pub mod controller;
pub mod layout;
pub mod locks;

pub use controller::{KeyboardController, KeyboardSignal, parse_set_backlight_args, parse_switch_layout_args};

/// Every action `oblisk.keyboard:invoke(...)` accepts. `dispatch` matches this rather than a string,
/// so a variant with no arm (or an arm with no variant) fails the build.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KeyboardAction {
    SetBacklight,
    SwitchLayout,
}

/// `oblisk.keyboard`'s action dispatch (ADR-0037): `set_backlight` is a D-Bus write, so it gets
/// `tokio::spawn`ed (ADR-0029); `switch_layout` is synchronous, forwarding through the compositor link's own channel (ADR-0034).
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
