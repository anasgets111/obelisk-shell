//! `obelisk.brightness` reports display backlight percent (docs/lua-api.md § 2.3),
//! reads sysfs through a udev `backlight` watch, and writes through logind (ADR-0053). Its write
//! action is `brightness:set(pct)` (§ 3.2), unlike read-only `battery`.
//!
//! No device means no [`controller::BrightnessSignal::Changed`], not a placeholder: § 2.3 has no
//! absence sentinel, and `0` means "backlight is off", not "no hardware". Lua therefore keeps
//! `obelisk.brightness` `nil` forever (ADR-0037's nil-until-hydrated contract).

pub mod controller;

pub use controller::{BrightnessController, BrightnessSignal, parse_set_args};

/// Every action `obelisk.brightness:invoke(...)` accepts. `dispatch` matches variants exhaustively.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrightnessAction {
    Set,
}

/// `set` makes a logind D-Bus call, so dispatch spawns it (ADR-0037, ADR-0029).
pub fn dispatch(controller: &BrightnessController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<BrightnessAction>(params) else { return };
    match action {
        BrightnessAction::Set => match parse_set_args(&params.arguments) {
            Some(pct) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.set(pct).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
    }
}
