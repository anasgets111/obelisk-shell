//! `obelisk.power` reports profiles and battery charge/discharge rate
//! from two D-Bus services (ADR-0053 amendment).
//!
//! `obelisk.power` combines unrelated facts: power-profiles-daemon supplies
//! `active_profile`/`profiles`, UPower supplies `on_battery`/`energy_rate`. Either half may be
//! absent, so fields are optional and never fabricated: no `"balanced"` without the profile daemon
//! or `0` without UPower. It has no absence sentinel; ADR-0037's nil-until-hydrated contract covers
//! it.
//!
//! **The power-profiles-daemon half is not live-verified.** It is absent on this machine; its
//! interface follows the documented D-Bus API. UPower is verified here: both properties were read
//! from this machine's `upowerd` and both emit `PropertiesChanged`.

pub mod controller;

pub use controller::{PowerController, PowerSignal, parse_set_profile_args};

#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    /// (name: string) Switches to one of `profiles`.
    SetProfile,
}

/// `set_profile` writes a D-Bus property and is spawned (ADR-0037, ADR-0029), like
/// `brightness::dispatch`.
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
