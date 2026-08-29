//! `oblisk.brightness` capability: display backlight percent (docs/oblisk-idl-api-specs.md
//! § 2.3), sysfs-read via a udev `backlight` subsystem watch, written through logind
//! (docs/adr/0053, "brightness... get a phase of their own" -- this is that phase,
//! docs/build-steps.md Phase 28 § 2 item 2). Sibling to `battery` under `hardware` (see
//! `hardware/mod.rs`'s own doc comment for the module-layout boundary) and shaped like it
//! closely: a sysfs-read controller driven by one udev subsystem watch with a poll fallback, one
//! `Changed` signal, an unbounded channel back to `main.rs`. Differs from `battery` two ways:
//! the write side (`brightness:set(pct)`, § 3.2) rides `org.freedesktop.login1.Session.
//! SetBrightness` on the shared system-bus connection, so this capability gets a `dispatch`
//! function battery has no need for; and no backlight device found at startup means the
//! controller never emits [`controller::BrightnessSignal::Changed`] at all, not even once with a
//! placeholder value.
//!
//! That last point is deliberate, not an oversight. Unlike `battery.present`/`sysinfo.temp_gpu`'s
//! `-1`, § 2.3 specifies only `percent: integer [0, 100]` and no absence sentinel. Fabricating a
//! `0` would read to a config as "the backlight is off", not "no backlight hardware exists", and
//! inventing a sentinel the spec doesn't have would make this capability's shape diverge from
//! what a config author can read straight off the IDL. So with no device, the Lua
//! `oblisk.brightness` signal simply stays `nil` forever -- ADR-0037's uniform
//! nil-until-hydrated contract already covers exactly this, and a config's own `or` fallback is
//! the mechanism meant to handle it. A later reader tempted to "fix" the silence by pushing a
//! `0` should read this paragraph first: it would be trading a correct absence for an incorrect
//! reading.

pub mod controller;

pub use controller::{BrightnessController, BrightnessSignal, parse_set_args};

/// `oblisk.brightness`'s action dispatch (ADR-0037): `set` makes a real D-Bus call (logind), so
/// it gets `tokio::spawn`ed (ADR-0029), the same shape `keyboard::dispatch`'s own
/// `set_backlight` arm already uses.
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
