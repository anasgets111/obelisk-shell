//! Idle capability (`oblisk.idle`, docs/oblisk-supervisor-services-dbus.md §7;
//! docs/adr/0032). Splits transport -- `ext_idle_notifier_v1` on the Supervisor's own dedicated
//! Wayland connection for notify, `org.freedesktop.login1.Manager.Inhibit` on the existing
//! system-bus connection for inhibit -- but shares one controller and one generation-scoped
//! cleanup hook (ADR-0032's "Consequences": "share generation-scoped cleanup but not code
//! paths").
//!
//! Mirrors `dbus::tray`'s degrade-to-inert precedent for notify: if `ext_idle_notifier_v1` or
//! `wl_seat` isn't advertised, the dedicated Wayland connection itself fails to establish, or
//! setup doesn't finish within [`IDLE_NOTIFY_SETUP_TIMEOUT`] (a hung/misbehaving compositor --
//! observed live once against a real niri session as a genuine `roundtrip()` stall with every
//! thread parked, not a slow-but-progressing one), `register_threshold` becomes a silent no-op
//! (logged once when the outcome is known, not per call). That setup -- a genuinely blocking,
//! synchronous Wayland connect+roundtrip -- runs inside `tokio::task::spawn_blocking` (never
//! inline on the async executor, per this project's own async-hygiene rule: docs/build-steps.md
//! Phase 9) as its own background task kicked off from [`IdleController::new`], which returns
//! immediately with notify starting `Inert` and only swapping to `Live` if/when that task
//! actually succeeds -- so a slow or hung compositor can never delay `socket::spawn_listener`
//! (or anything else in `main.rs`'s boot sequence) behind idle-notify setup.
//!
//! Inhibit has no equivalent degrade path -- it rides the Supervisor's already-required system-bus
//! connection, so its only failure mode is the `Inhibit` call itself failing per-request (ADR-0032).
//! Its `Login1ManagerProxy` is therefore built fresh on every `inhibit()` call rather than cached
//! once at construction (a cached failure would freeze that per-request failure mode into a
//! permanent one), and its refcount decision, D-Bus call, and fd write are one atomic critical
//! section under a single `tokio::sync::Mutex` -- see [`IdleController::inhibit`]'s doc comment.
//!
//! Two pure, unit-testable seams carry the real decision logic, wrapped by thin
//! async/Wayland-touching methods on [`IdleController`]:
//! - [`register_threshold_entry`] / [`cleanup_generation_thresholds`]: the notify fan-out
//!   registry's create-vs-append decision and per-generation cleanup.
//! - [`apply_inhibit`] / [`apply_release_inhibit`] / [`cleanup_generation_inhibit`]: the inhibit
//!   refcount arithmetic and per-generation cleanup.

pub mod controller;
pub mod inhibit;
pub mod notify;

pub use controller::{IdleController, parse_inhibit_args, parse_register_args};
