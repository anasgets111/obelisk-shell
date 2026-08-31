//! `oblisk.updates` capability: Arch package update checking and installation via `alpm`
//! (ADR-0034). Fully separate from `oblisk.sysinfo`'s scheduler -- same shape, zero shared
//! code, per the ADR's own instruction. Top-level, sibling to `hardware`/`dbus`/`audio`/`privacy`.

pub mod check;
pub mod controller;
pub mod install;
pub mod pacman_conf;

pub use controller::{UpdatesController, UpdatesSignal, parse_configure_args};

/// `oblisk.updates`'s action dispatch (ADR-0037): `configure` is synchronous (it only rewrites
/// the interval under its lock and nudges the scheduler's watch channel -- docs/adr/0034);
/// `install` runs a real `pacman` child and gets `tokio::spawn`ed.
pub fn dispatch(controller: &UpdatesController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    match params.action.as_str() {
        "configure" => match parse_configure_args(&params.arguments) {
            Some(interval_secs) => controller.configure(interval_secs),
            None => crate::log_malformed_command(params),
        },
        "install" => {
            let controller = controller.clone();
            tokio::spawn(async move {
                controller.install().await;
            });
        }
        _ => crate::log_unknown_action(params),
    }
}
