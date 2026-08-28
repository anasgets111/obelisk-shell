# The Renderer holds `ext_session_lock_v1`; the Supervisor supervises the lock client

Supersedes ADR-0010's session-lock half. Its idle-notify half stands unchanged.

ADR-0010 put `ext_session_lock_v1` in the Supervisor, reasoning that "if the Renderer crashes while
`system.state.locked == true`, that surface dies with it" and that a security boundary must not fail
open on a GPU driver panic or a Lua VM crash. Two things about that turn out to be wrong, and the
second is the one that forces the decision.

## The protocol already guarantees fail-secure

`ext-session-lock-v1` says it twice, once with RFC 2119 weight:

> If the client dies while the session is locked, the compositor **must not** unlock the session in
> response. It is acceptable for the session to be permanently locked if this happens. The
> compositor may choose to continue to display the lock surfaces the client had mapped before it
> died or alternatively fall back to a solid color, this is compositor policy.

`smithay-client-toolkit` encodes the same philosophy in `Drop for SessionLockInner`, whose comment
notes that not unlocking on drop "results in us failing secure". Hyprland implements the policy
clause with a distinct "lockscreen app died" fallback screen.

So a Renderer crash while locked cannot unlock the session. The failure mode ADR-0010 was built to
prevent does not exist for a real lock client. It did exist for what ADR-0010 was actually looking
at, which was a lock screen drawn as an ordinary `overlay_canvas` widget rather than a lock surface
at all.

## ADR-0010's design cannot render a Lua lock screen

This is the decisive part. ADR-0010 kept the Lua-authored lock UI in the Renderer: "The full
Lua-styled lock UI (the reference fixture's `screen_lock_block`) still renders through the Renderer
as before". That is not achievable.

The spec's `locked` event fires only once "no security sensitive normal/unlocked content is possibly
visible". When the session is locked the compositor shows lock surfaces and nothing else; a
layer-shell surface is exactly the normal content being hidden. A Lua lock widget on
`overlay_canvas` is invisible the moment the Supervisor takes the lock.

And it cannot be fixed by handing the surface across. `get_lock_surface` is a request on the
`ext_session_lock_v1` object itself, and Wayland object IDs live inside one client's connection.
There is no mechanism for the Supervisor to create a lock surface the Renderer can draw into. The
process that holds the lock is the process that paints it.

That leaves a forced choice. Either the Supervisor holds the lock and the lock screen is whatever
the Supervisor itself can paint, which ADR-0010 defined as a solid `wl_shm` color with a
pre-rasterized indicator and explicitly no EGL in the privileged process, or the Renderer holds the
lock and the lock screen is a normal Lua-authored surface. A Lua-authored lock screen was named as a
goal on 2026-08-28.

## Decision

**The Renderer holds `ext_session_lock_v1` and creates one `ext_session_lock_surface_v1` per
output.** `lock` is the fourth surface role in ADR-0040, and a lock surface takes a node tree like
any other, so the lock screen is authored, themed, and reloaded the same way a bar is.

**The Supervisor keeps everything about the lock except the protocol object.** It owns
`ext_idle_notifier_v1` and therefore owns *when* to lock (ADR-0010's idle half, unchanged). It
issues the lock command. It already tracks and reaps generations, so it is the process that notices
a lock client died and can respawn one. This is the same division ADR-0010 named in its own closing
line, "authority handles and data-stream backends, never presentation", applied more accurately: the
durable thing is the decision to lock and the supervision of the locker, not the protocol handle,
which the kernel and the compositor already make durable.

**Authentication composes from pieces that already exist.** The lock screen's `textfield` carries
`secure_submit`, so keystrokes go into `shared::SecureBuffer` and never through Lua (ADR-0005,
ADR-0027). The buffer crosses the control socket to the Supervisor, which runs the real PAM
conversation in its re-exec'd worker subprocess (ADR-0028). On success the Supervisor tells the
Renderer to unlock, and the Renderer, holding the lock, calls `unlock_and_destroy`. The password
never enters the Lua VM and never enters the process holding the graphics context.

## Constraints this imposes

**No generation swap while locked.** Only one client may hold a session lock; a second `lock`
request gets `finished` immediately. PBA's overlapping-generation handoff is therefore impossible
while locked, because candidate `N+1` cannot acquire the lock `N` holds. A config edit during a
locked session queues its swap until unlock. In-place reloads are unaffected and still work, so a
value change still applies live.

**Lock surfaces track outputs.** The client "is expected to create lock surfaces for all outputs
currently present and any new outputs as they are advertised", and a second surface on one output is
a `duplicate_output` error. Destroying a lock surface while its output is still active makes the
compositor "fall back to rendering a solid color", so teardown ordering on output removal matters.
This reuses ADR-0041's `oblisk.screens` output tracking rather than a second one.

**`finished` is two different events and both must surface.** Arriving in response to `lock`, it
means the lock was denied, typically because another lock client already holds it. Arriving later,
it means the compositor tore the lock down through its own secure mechanism. Neither may be
swallowed: the first is a real failure the user must see, and `oblisk.rescue` is the existing
channel for it.

**Never call `unlock_and_destroy` except on a successful authentication.** SCTK's `Drop` deliberately
does not unlock, and nothing in Oblisk should add a convenience path that does. A client wanting to
exit right after unlocking must `wl_display.sync` first, or the server may not have processed the
unlock.

## Consequences

Crash recovery is worse than ADR-0010 imagined and better than it sounds. If the Renderer dies while
locked, the session stays locked and the compositor shows its fallback, which on Hyprland is a
"lockscreen app died" screen. Whether a respawned Renderer can retake the lock is compositor policy:
Hyprland gates it behind `misc:allow_session_lock_restore`, off by default. Do not promise portable
recovery. The Supervisor should attempt the respawn, report the `finished` denial when it comes, and
leave the user with the compositor's own recovery path rather than pretending to have one.

This is not a regression against the alternative. ADR-0010's fallback surface would have shown a
solid color with a "locked" indicator and no password prompt, which is the same dead end reached
through more privileged code. It is also exactly the position every real lock client is in: hyprlock
and swaylock live with it, and the accepted mitigation is an external watchdog, which is what the
Supervisor already is.

ADR-0010's `smithay-client-toolkit` `session_lock` dependency moves from the Supervisor to the
Renderer. The Supervisor keeps its own Wayland connection for idle-notify alone, which is still the
right call for the same reason ADR-0010 gave: idle must survive a Renderer crash, because it is what
triggers the lock in the first place.
