# ADR-0032: Idle capability splits transport (Wayland notify, logind inhibit) but keeps one controller

## Status

Accepted

## Context

`docs/oblisk-supervisor-services-dbus.md` §7 specs multi-threshold idle
notification only: Lua registers arbitrary durations, the Supervisor holds
one `ext_idle_notifier_v1` listener per duration, zero polling. It says
nothing about inhibit. `docs/oblisk-idl-api-specs.md` has `idle:
register_threshold(sec, on_idle, on_resume)` in §3.2 and no §2.x read-signal
section at all — idle is event-shaped, not `StateSnapshot`-polled, unlike
every other capability built so far (tray/bluetooth/network all push a
pollable signal table).

ADR-0010 already settled *notify's* ownership: the Supervisor holds
`ext_idle_notifier_v1` on its own dedicated Wayland connection so idle
authority survives a Renderer crash or reload, the same reasoning as lock
authority. It says nothing about inhibit. The user's own pushback mid-grill
("we need inhib too") forced the question ADR-0010 left open: does inhibit
share notify's Wayland ownership, or does it belong somewhere else entirely?

## Decisions

**Notify: one `ext_idle_notification_v1` listener per distinct threshold
duration, fanned out to every registration sharing it.** `register_threshold`
takes no id and returns none; `sec` is not a unique key. Two Lua call sites
registering `30` both get called on the same listener's `idled`/`resumed`
pair — the Supervisor allocates one Wayland listener per distinct duration
value, not per call, and keeps a `HashMap<Duration, Vec<registration>>`
fan-out list. No unregister command: ADR-0006's existing `reset_registrations`
(sent on every reload) already clears a generation's entire registration set,
the same mechanism idle threshold cleanup reuses rather than growing its own.

**Notify uses `get_idle_notification`, not `get_input_idle_notification`.**
Protocol v2 splits "no input" from general idle (also honors presence
sensors). Nothing in the docs or the product need (dim/lock/DPMS-sleep after
inactivity) asks for presence-sensor exclusion — that's a kiosk/accessibility
case nobody requested. One line to switch later if it ever matters.

**New `SupervisorFrame::IdleEvent { generation_id, threshold_sec, state }`
wire variant**, `state: "idled" | "resumed"`. `StateSnapshot` is built for
pollable state Lua diffs by `revision`; forcing an edge-triggered callback
through it would make Lua poll a revision counter every frame just to catch
one firing — exactly the busy-loop §7 exists to avoid. One more tagged
variant on the existing frame enum, no new transport, dispatched straight to
the registered Lua callback instead of through the signal-table path.

**Inhibit: `org.freedesktop.login1.Manager.Inhibit(what="idle", who="oblisk",
why=reason, mode="block") -> fd`, not the Wayland `idle-inhibit-unstable-v1`
protocol.** `zwp_idle_inhibit_manager_v1::create_inhibitor` needs a
`wl_surface`, which the Supervisor doesn't own (ADR-0010: Supervisor owns
authority and data-stream backends, never presentation) and the Renderer does
— splitting inhibit's owner from notify's for no real gain. logind's
`Inhibit` is a plain system-bus call on the connection NetworkManager/BlueZ/
polkit already share: zero new Wayland ownership question, zero Cargo
dependency, zero renderer involvement. Lock lifetime is fd lifetime — held
open while inhibiting, closed to release, and released automatically if the
holding process dies, so a Supervisor crash can't leak a stuck inhibit.
`what` scoped to `"idle"` only (not `"sleep"`/`"shutdown"`/lid-switch): that
governs systemd's own auto-suspend-on-idle action, the actual product need
(don't let the machine sleep while, say, a video widget plays); the other
`what` values inhibit unrelated system actions nothing here asked for.

**Inhibit is refcounted per generation, not a single boolean.** Two
concurrent Lua callers (a media widget, a manual keep-awake toggle) must
coexist without one's `release_inhibit()` killing the other's still-active
hold. `idle:inhibit(reason)` increments a per-generation count and opens the
fd on 0→1; `idle:release_inhibit()` decrements and closes it on 1→0. The same
per-generation reset `reset_registrations` already triggers on reload or
crash zeros this count too, reusing the cleanup hook built for threshold
fan-out rather than inventing a second one.

**Cargo: add the `"staging"` feature to `supervisor`'s `wayland-protocols`
dependency.** `wayland_protocols::ext::idle_notify::v1` is gated behind it
(`wayland-protocols-0.32.13/src/ext.rs:6-9`); the currently-enabled
`"client"` feature alone doesn't expose it.

**Graceful degradation matches every prior controller.** If
`ext_idle_notifier_v1` isn't advertised by the compositor, or the
Supervisor's dedicated Wayland connection itself fails to establish, log once
and construct an inert controller: `register_threshold` becomes a silent
no-op, no panic, no boot abort. Same shape as `TrayController::inert`/
`BluetoothController::new`'s degrade-not-abort precedent. Inhibit has no
equivalent degrade path to design: it rides the Supervisor's existing
system-bus connection, already required for NetworkManager/BlueZ/polkit, so
its only failure mode is the `Inhibit` call itself failing per-request
(logged, request no-ops), not a startup-time capability gap.

**`docs/build-steps.md` Phase 16 splits "idle capability (§7)" into two named
sub-items.** Notify and inhibit share a controller and this ADR, but not a
transport — notify needs the Wayland `"staging"` feature gate and the
Supervisor's dedicated Wayland connection, inhibit needs neither. Two
sub-bullets prevent a future reader from assuming inhibit inherits notify's
Wayland-specific setup work.

## Consequences

- `oblisk.idle`'s write table gains `inhibit(reason)` and `release_inhibit()`
  alongside the already-specified `register_threshold(sec, on_idle,
  on_resume)` — none of the three existed as a working command before this
  round; both spec docs need updating to match (tracked as a doc-sync task,
  not blocking implementation).
- `shared::SupervisorFrame` gains its first non-`StateSnapshot` push variant.
  Any future event-shaped (not state-shaped) capability has real precedent to
  follow instead of forcing itself through `StateSnapshot`.
- Idle-notify and idle-inhibit live in one `dbus::idle` (name kept for
  IDL/capability-string consistency despite inhibit's D-Bus-only, notify's
  Wayland-only transport) module and one controller struct, sharing generation-
  scoped cleanup but not code paths.

## Upgrade path

- `get_input_idle_notification` (presence-sensor-exclusive idle), if a real
  accessibility/kiosk use case ever needs it — one-line proxy swap.
- Wider `what` scope (`"sleep"`, `"shutdown"`, lid-switch) if a real feature
  needs to block something other than auto-suspend-on-idle.
- `docs/oblisk-idl-api-specs.md` gains the `inhibit`/`release_inhibit` rows
  and `docs/oblisk-supervisor-services-dbus.md` §7 gains an inhibit
  subsection matching this ADR's design.
