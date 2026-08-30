//! `oblisk.brightness` capability: display backlight percent (docs/oblisk-idl-api-specs.md
//! § 2.3), sysfs-read via a udev `backlight` subsystem watch, written through logind
//! (docs/adr/0053). Sibling to `battery` under `hardware`, shaped the same way but with a
//! `dispatch` function for the write side (`brightness:set(pct)`, § 3.2).
//!
//! No backlight device at startup means the controller never emits
//! [`controller::BrightnessSignal::Changed`], not even once with a placeholder -- § 2.3 has
//! no absence sentinel for `percent`, and fabricating `0` would read as "backlight is off",
//! not "no hardware". The Lua `oblisk.brightness` signal stays `nil` forever in that case
//! (ADR-0037's nil-until-hydrated contract); don't "fix" the silence by pushing a `0`.

pub mod controller;

pub use controller::{BrightnessController, BrightnessSignal, parse_set_args};

/// `oblisk.brightness`'s action dispatch (ADR-0037): `set` makes a real D-Bus call (logind),
/// so it gets `tokio::spawn`ed (ADR-0029).
pub fn dispatch(controller: &BrightnessController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    match params.action.as_str() {
        "set" => match parse_set_args(&params.arguments) {
            Some(pct) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.set(pct).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        _ => crate::log_unknown_action(params),
    }
}
