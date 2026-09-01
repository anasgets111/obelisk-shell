//! Idle capability (`oblisk.idle`, docs/oblisk-supervisor-services-dbus.md §7; ADR-0032).
//! Splits transport -- `ext_idle_notifier_v1` on the Supervisor's own dedicated Wayland
//! connection for notify, `org.freedesktop.login1.Manager.Inhibit` on the existing system-bus
//! connection for inhibit -- but shares one controller and one generation-scoped cleanup hook.
//!
//! Notify degrades to inert (silent no-op, logged once) if the protocol isn't advertised, the
//! dedicated connection fails, or setup exceeds [`IDLE_NOTIFY_SETUP_TIMEOUT`]; setup runs as a
//! background task from [`IdleController::new`], which returns immediately, so a hung
//! compositor can never delay boot. Inhibit has no equivalent degrade path -- it rides the
//! Supervisor's already-required system-bus connection, so its only failure mode is a
//! per-request `Inhibit` call failing (see [`IdleController::inhibit`]).
//!
//! The real decision logic lives in pure, unit-testable seams -- [`register_threshold_entry`]/
//! [`cleanup_generation_thresholds`] for notify fan-out, [`apply_inhibit`]/
//! [`apply_release_inhibit`]/[`cleanup_generation_inhibit`] for the inhibit refcount -- wrapped
//! by thin async/Wayland-touching methods on [`IdleController`].

pub mod controller;
pub mod inhibit;
pub mod notify;

pub use controller::{IdleController, parse_inhibit_args, parse_register_args};

/// Every action `oblisk.idle:invoke(...)` accepts. `dispatch` matches this rather than a string,
/// so a variant with no arm (or an arm with no variant) fails the build.
#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IdleAction {
    Register,
    Inhibit,
    ReleaseInhibit,
}

/// `oblisk.idle`'s action dispatch (ADR-0037): owns the action match, argument parse, and
/// write-action spawn for every `idle` `CommandEnvelope`. Every action carries the
/// registering generation's own id (ADR-0032/ADR-0006).
pub fn dispatch(controller: &IdleController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    let generation_id = params.generation_id;
    let Some(action) = crate::parse_action::<IdleAction>(params) else { return };
    match action {
        IdleAction::Register => match parse_register_args(&params.arguments) {
            Some(sec) => {
                let controller = controller.clone();
                tokio::spawn(async move {
                    controller.register_threshold(generation_id, sec).await;
                });
            }
            None => crate::log_malformed_command(params),
        },
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
