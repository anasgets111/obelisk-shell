# Rescue renders out of band when no scene survives

`oblisk.rescue` (§ 2.10) is a Lua signal a config reads and renders into its own tree. That works
only while the config works. When `shell.lua` fails to evaluate at startup there is no tree, and
ADR-0024 item 4 records the result plainly: the shell stays blank. The failure that most needs an
error message is the one that cannot produce one.

This is the same shape as the mistake ADR-0042 found in ADR-0010, where a Lua-authored lock screen
was supposed to render through a surface that a locked session hides. A presentation path that
depends on the thing that just failed is not a fallback.

Quickshell renders its errors from outside the failed engine. `rootwrapper.cpp` keeps the previous
generation alive on a scan or component error, collects the error text with file and line, and calls
`ReloadPopup::spawnPopup` to show it in a separate process, alongside a `reloadFailed(errorString)`
signal for configs that would rather handle it themselves.

## Decision 1: split the two failures, because only one of them is recoverable

A **reload failure** leaves a working scene on screen. ADR-0024's rollback guarantee holds, the
config from before the edit is still running, and it can render its own error banner. `oblisk.rescue`
is the right mechanism and stays exactly as specified.

A **startup failure** leaves nothing. No prior scene, no working config, no Lua tree to render
through. This is what gets the out-of-band path.

Stating the split is the point. `oblisk.rescue` was never wrong, it was load-bearing for a case it
structurally cannot cover, and the fix is a second path rather than a replacement.

## Decision 2: the Supervisor spawns a rescue process, which is not a generation

On a startup evaluation failure with no prior scene, the Supervisor spawns a process that binds one
`Overlay` layer surface per output and draws the error text. It has no Lua VM, no config, and no
capability connections. Its content is hardcoded Rust.

It is not a generation and must not be mistaken for one. It has no generation id, receives no
dependency snapshots, takes part in no PBA handshake, and holds no authority over any output. The
Supervisor reaps it as soon as a real generation reaches presentation evidence.

Re-exec is the mechanism, following ADR-0028's PAM worker: the Supervisor re-runs its own binary
with a flag and the error text. No new crate, no second binary to install, no shared code path with
the Renderer that a config could influence.

## Decision 3: it shows the error, not a shell

Error text, the file and line `mlua::Error` already carries, and the path it tried to load. Nothing
else. No fallback bar, no default config, no recovery UI.

A fallback config was the obvious alternative and is rejected in decision 4. What is worth saying
here is that the rescue process has no reason to grow: the moment a working config exists it is
killed, and any feature added to it is a feature nobody sees while their shell works.

## Rejected: a built-in default config the engine falls back to

Evaluate a Rust-embedded `shell.lua` when the user's fails, and render that through a normal
generation.

Rejected because it needs a working Lua VM, a working loader, a working layout pass, and a working
paint pass to be the thing that reports those are broken. It covers the narrow case where only the
user's file is malformed and fails at exactly the moments a fallback matters most. It also invites
the shell to look like it is working when it is not, which is worse than a blank screen with an
error on it.

## Rejected: exit with the error on stderr

The current behavior, near enough.

Rejected because a shell is launched from a session file or a compositor `exec-once`, where nothing
is attached to stderr and the user's whole experience is a screen that stays empty. The error is
already written to the journal and should stay there; the journal is not the notification.

## Consequences

No `inhibitReloadPopup` equivalent is needed. Quickshell has one because its popup spawns on reload
failures too, which is the case a config can handle for itself. Decision 1 gives that case to
`oblisk.rescue` and never spawns a process for it, so there is nothing to inhibit.

`oblisk-idl-api-specs.md` § 2.10 gains a sentence naming the boundary of what `rescue` covers, so
the next reader does not assume it handles the startup case.
