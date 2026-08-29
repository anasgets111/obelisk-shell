//! `oblisk.power` capability: power profiles and the battery's charge/discharge rate
//! (`docs/oblisk-idl-api-specs.md` § 2.13), read from two D-Bus services
//! (`docs/build-steps.md` Phase 28 item 4, docs/adr/0053's `power` amendment).
//!
//! Two services, because § 2.13 is two unrelated facts wearing one name. `active_profile` and
//! `profiles` come from power-profiles-daemon; `on_battery` and `energy_rate` come from UPower,
//! which `keyboard`'s backlight half already talks to on the same shared system-bus connection.
//! Either half can be missing on a real machine and the other still works, so every field is
//! optional and the payload carries what this host can actually answer.
//!
//! That is a departure from `brightness`, which never pushes at all when its one source is
//! missing, and it is the same reasoning applied to a capability with more than one source: a
//! desktop with no power-profiles-daemon still has a mains adapter worth reporting. What both
//! refuse to do is fabricate. There is no `"balanced"` invented for a host with no profile
//! daemon, and no `0` invented for a host with no UPower, because § 2.13 gives no absence
//! sentinel for any of the four and ADR-0037's nil-until-hydrated contract already covers the
//! difference.
//!
//! **The power-profiles-daemon half is not live-verified.** That daemon is not installed on the
//! machine this was written on, so its interface is built to the project's documented D-Bus API
//! and confirmed against nothing. This is the same posture, and the same admission, that
//! ADR-0034 records for `keyboard`'s Hyprland implementor. The UPower half is verified: both
//! properties were read off this machine's own `upowerd` and both emit `PropertiesChanged`.

pub mod controller;

pub use controller::{PowerController, PowerSignal, parse_set_profile_args};

/// `oblisk.power`'s action dispatch (ADR-0037): `set_profile` writes a D-Bus property, so it gets
/// `tokio::spawn`ed (ADR-0029), the same shape `brightness::dispatch` uses for its own D-Bus
/// write.
pub fn dispatch(controller: &PowerController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    match params.action.as_str() {
        "set_profile" => match parse_set_profile_args(&params.arguments) {
            Some(profile) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.set_profile(&profile).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        _ => crate::log_unknown_action(params),
    }
}
