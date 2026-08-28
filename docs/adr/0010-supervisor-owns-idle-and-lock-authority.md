# Supervisor owns idle-notify and lock authority, with its own Wayland connection

> The session-lock half is superseded by ADR-0042; the idle-notify half stands. Two premises below
> did not survive contact with the protocol. `ext-session-lock-v1` requires the compositor to keep
> the session locked when the lock client dies, so a Renderer crash cannot fail open. And the
> Lua-styled lock UI cannot render "through the Renderer as before": a locked session hides every
> non-lock surface, so an `overlay_canvas` widget is invisible while locked, and lock surfaces
> cannot be shared across processes. The Renderer now holds the lock and paints it.

`supervisor/Cargo.toml` had no Wayland dependency, but `oblisk-supervisor-services-dbus.md` §7 states the Supervisor binds `ext_idle_notifier_v1` directly, a Wayland protocol, not a D-Bus interface. That contradiction forced a real decision: does idle-notify move to the Renderer (which already owns a Wayland connection, ADR-0008/0009), or does the Supervisor get a second one.

The deciding case is session-lock, not idle-notify. `ext_session_lock_v1` and the `ext_session_lock_surface_v1` objects it requires are Wayland protocol objects bound to whichever process's connection created them; they cannot cross a process boundary. The reference fixture's lock screen today is an ordinary Lua-authored `overlay_canvas` widget, painted by the ephemeral Renderer. If the Renderer crashes while `system.state.locked == true`, that surface dies with it: `rescue.is_rescue` only covers Lua syntax errors at load time, not a runtime crash of an already-locked Renderer. Locking is a security boundary, not a visual one, so the process that holds "is the screen locked" has to be the one durable enough that a Renderer crash can never drop coverage, even for one frame.

Decision: the Supervisor holds `ext_idle_notifier_v1` and `ext_session_lock_v1`, both via its own Wayland connection (`smithay-client-toolkit`'s `session_lock` module for the latter; idle-notify is hand-dispatched against raw `wayland-protocols`, no SCTK wrapper exists for it). The moment the Supervisor locks, it commits a minimal fallback surface itself: a solid-color `wl_shm` buffer with a pre-rasterized "locked" indicator, no GLES3/EGL context in the privileged process. The full Lua-styled lock UI (the reference fixture's `screen_lock_block`) still renders through the Renderer as before, but only while the Renderer is alive and presenting; the Supervisor's fallback is what's actually authoritative for screen coverage, and repaints immediately if the Renderer's surface drops.

This generalizes: the Supervisor owns anything a Renderer crash or reload cannot be allowed to interrupt, authority handles and data-stream backends, never presentation. Renderer death should degrade the visual experience, never the security or data-availability guarantees underneath it.

Considered keeping session-lock Renderer-owned entirely and accepting the crash window as a rare, low-severity risk. Rejected: a security boundary that fails open on a GPU driver panic or a Lua VM crash isn't a rare-risk trade-off, it's a bug budgeted as a feature.
