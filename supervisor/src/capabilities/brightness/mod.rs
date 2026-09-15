//! `obelisk.brightness` reports display backlight percent, reads sysfs through a udev `backlight`
//! watch, and writes through logind (ADR-0053). Its write action is `brightness:set(pct)`, unlike
//! read-only `battery`.
//!
//! No device means no [`controller::BrightnessSignal::Changed`], not a placeholder: `brightness`
//! has no absence sentinel, and `0` means "backlight is off", not "no hardware". Lua therefore
//! keeps `obelisk.brightness` `nil` forever (ADR-0037's nil-until-hydrated contract).

pub mod controller;

pub use controller::{BrightnessController, BrightnessSignal};

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum BrightnessAction {
    /// Sets the screen backlight, `0` to `100`.
    Set { percent: u64 },
}

/// `set` makes a logind D-Bus call, so dispatch spawns it (ADR-0037, ADR-0029).
pub fn dispatch(controller: &BrightnessController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<BrightnessAction>(&envelope.params) else { return };
    match action {
        BrightnessAction::Set { percent } => {
            let controller = controller.clone();
            tokio::spawn(async move { controller.set(percent).await });
        }
    }
}
