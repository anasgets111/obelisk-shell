//! `oblisk.workspaces` capability: per-output workspace state and the focused window
//! (`docs/oblisk-idl-api-specs.md` § 2.9), read off niri's IPC event stream
//! (docs/adr/0056, `docs/build-steps.md` Phase 28 item 3).
//!
//! A top-level module rather than a tenant of `hardware/` or `dbus/`: it is neither a device nor a
//! D-Bus interface, it is one compositor's Unix socket, which is the same reason `privacy`,
//! `updates` and `system` sit here.
//!
//! One compositor, no trait (docs/adr/0056 decision 1). `keyboard`'s `CompositorLink` has two
//! implementors and was deliberately scoped to layout; this has one implementor and a trait with
//! one implementor is speculative generality by this repo's own review checklist. What is shared
//! with `keyboard` is the verified part with no per-capability shape, `detect_compositor()` and
//! `CompositorKind`. A session that is not niri never pushes, and `oblisk.workspaces` stays `nil`,
//! which is `brightness`'s missing-backlight posture applied unchanged: § 2.9 has no absence
//! sentinel, and an empty `outputs` array would read as "this compositor has no workspaces"
//! rather than "nobody asked this compositor".

pub mod controller;

pub use controller::{WorkspacesController, WorkspacesSignal, parse_focus_args};

/// `oblisk.workspaces`'s action dispatch (ADR-0037): `focus` writes to the compositor over a
/// fresh socket, which [`WorkspacesController::focus`] does on its own thread, so this arm is a
/// plain call rather than a `tokio::spawn` (there is no future to drive).
pub fn dispatch(controller: &WorkspacesController, envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    match params.action.as_str() {
        "focus" => match parse_focus_args(&params.arguments) {
            Some(id) => controller.focus(id),
            None => crate::log_malformed_command(params),
        },
        _ => crate::log_unknown_action(params),
    }
}
