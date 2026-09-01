//! `oblisk.power` capability: power profiles and the battery's charge/discharge rate
//! (`docs/oblisk-idl-api-specs.md` § 2.13), read from two D-Bus services
//! (ADR-0053's `power` amendment).
//!
//! Two services, because § 2.13 is two unrelated facts wearing one name. `active_profile` and
//! `profiles` come from power-profiles-daemon; `on_battery` and `energy_rate` come from UPower.
//! Either half can be missing on a real machine and the other still works, so every field is
//! optional and the payload carries what this host can actually answer -- never fabricated (no
//! `"balanced"` invented for a host with no profile daemon, no `0` invented for a host with no
//! UPower; § 2.13 gives no absence sentinel and ADR-0037's nil-until-hydrated contract already
//! covers the difference).
//!
//! **The power-profiles-daemon half is not live-verified.** That daemon is not installed on the
//! machine this was written on, so its interface is built to the project's documented D-Bus API
//! and confirmed against nothing. The UPower half is verified: both properties were read off
//! this machine's own `upowerd` and both emit `PropertiesChanged`.

pub mod controller;

pub use controller::{PowerController, PowerSignal, parse_set_profile_args};

/// Every action `oblisk.power:invoke(...)` accepts. `dispatch` matches this rather than a string,
/// so a variant with no arm (or an arm with no variant) fails the build.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    SetProfile,
}

/// `oblisk.power`'s action dispatch (ADR-0037): `set_profile` writes a D-Bus property, so it gets
/// `tokio::spawn`ed (ADR-0029), the same shape `brightness::dispatch` uses for its own D-Bus
/// write.
pub fn dispatch(controller: &PowerController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let Some(action) = crate::parse_action::<PowerAction>(params) else { return };
    match action {
        PowerAction::SetProfile => match parse_set_profile_args(&params.arguments) {
            Some(profile) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.set_profile(&profile).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
    }
}
