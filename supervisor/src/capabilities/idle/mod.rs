//! Idle capability (`oblisk.idle`, docs/oblisk-supervisor-services-dbus.md §7; ADR-0032).
//! Notify uses the Supervisor's dedicated `ext_idle_notifier_v1` Wayland connection; inhibit uses
//! `org.freedesktop.login1.Manager.Inhibit` on the existing system bus. They share one controller
//! and generation-scoped cleanup.
//!
//! Notify becomes inert (silent no-op, logged once) if the protocol is absent, its connection
//! fails, or setup exceeds [`IDLE_NOTIFY_SETUP_TIMEOUT`]. Background setup lets
//! [`IdleController::new`] return before a hung compositor. Inhibit rides the required system
//! bus, so only its per-request `Inhibit` call can fail (see [`IdleController::inhibit`]).
//!
//! Pure seams hold the decisions: [`register_threshold_entry`]/[`cleanup_generation_thresholds`]
//! for notify and [`apply_inhibit`]/[`apply_release_inhibit`]/[`cleanup_generation_inhibit`] for
//! refcounts. [`IdleController`] wraps them with async/Wayland operations.

pub mod controller;
pub mod gate;
pub mod inhibit;
pub mod notify;
pub mod state;

pub use controller::{IdleController, parse_inhibit_args, parse_register_args};
pub use state::IdleState;

/// Actions accepted on an `idle` `CommandEnvelope`; exhaustive dispatch keeps variants and arms in
/// sync. Not all of them are config-callable: see `stubs::internal_actions`.
///
/// Doc comments on the variants would be a mistake here. schemars emits a flat `enum` for a plain
/// unit enum and a `oneOf` once any variant carries a description, and `stubs::action_names` reads
/// the flat form -- so one `///` below silently empties the generated `invoke` union.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IdleAction {
    Register,
    // Sent by the Renderer's `IdleRegistry::forget_thresholds` before each evaluation, not by a
    // config (ADR-0158). It rides the same ordered socket as the registrations that follow it,
    // which is the whole point: a reset the Supervisor ran on its own timing landed after them.
    ForgetThresholds,
    Inhibit,
    ReleaseInhibit,
}

/// `oblisk.idle` action dispatch (ADR-0037): matches, parses, and spawns every `idle`
/// `CommandEnvelope`; each action carries its registering generation id (ADR-0032/ADR-0006).
pub fn dispatch(controller: &IdleController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let generation_id = params.generation_id;
    let Some(action) = crate::parse_action::<IdleAction>(params) else { return };
    match action {
        // Both threshold arms run inline rather than in a spawned task. A forget and the
        // registrations that follow it come off one ordered socket, and two tasks would be free to
        // apply them the other way round, which is the whole failure this pair exists to stop
        // (ADR-0158).
        IdleAction::Register => match parse_register_args(&params.arguments) {
            Some(sec) => controller.register_threshold(generation_id, sec),
            None => crate::log_malformed_command(params),
        },
        IdleAction::ForgetThresholds => controller.reset_thresholds(generation_id),
        IdleAction::Inhibit => match parse_inhibit_args(&params.arguments) {
            Some(reason) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.inhibit(generation_id, &reason).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
        IdleAction::ReleaseInhibit => {
            let controller = controller.clone();
            tokio::spawn(async move {
                controller.release_inhibit(generation_id).await;
            });
        }
    }
}
