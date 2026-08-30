# A crashed Renderer is detected and respawned, because a lock cannot be recovered otherwise

ADR-0042 put `ext_session_lock_v1` in the Renderer and the lock decision in the Supervisor, and
ADR-0052 built the capability that commands it. Both assume the Renderer keeps running. The one
case neither covers is the Renderer dying while it holds the lock, and that case is not a degraded
shell, it is a user who cannot get back into their session without a VT switch.

Today the Supervisor does not even notice. `main()`'s `select!` watches signals, the memory
sampler, polkit challenges, new connections and every capability channel. Nothing waits on
`authoritative.child`. A dead Renderer stops sending frames, and silence is what a healthy idle
Renderer also sends.

## What was measured first

The plan turns on a question the protocol deliberately does not answer. `ext-session-lock-v1` says
only that a compositor *may* let a second client take over:

> If the client dies while the session is locked, the compositor must not unlock the session in
> response. It is acceptable for the session to be permanently locked if this happens. [...]
> Compositors may allow a new client to create a `ext_session_lock_v1` object and take
> responsibility for unlocking the session.

So it was measured against niri 26.04 (`v26.04-85-gdd75865f`) rather than read, inside a nested
niri so no real session was ever at risk. A minimal `ext-session-lock-v1` client locked the nested
session and exited while locked, a second instance of it then acquired the orphaned lock, and that
second instance released it with `unlock_and_destroy`:

| step | result |
| --- | --- |
| lock, then die while locked | session stays locked, as the protocol requires |
| a second client locks | `locked`, not `finished` |
| that second client unlocks | session unlocks |

Takeover works here, and so does the half that matters more: the client that takes over can still
release. A takeover that could not unlock would be a second process holding a lock nobody can
clear, which is not a recovery.

The same rig showed what happens without this ADR. Quickshell, killed while its lock screen was up
and then restarted the way a session manager would restart it, reported `lock status: false` while
the compositor was still locked. Both statements were true, and the gap between them is the bug:
the restarted shell drew a bar behind niri's lock fallback where nothing could see it, and nothing
in the new process knew to re-lock. That is niri issue #2986, closed as working as intended, on the
grounds that recovering a dead locker is the shell's job and not the compositor's.

Quickshell cannot recover because the knowledge that the session was locked died with the process
that held it. Oblisk splits exactly that, which is the whole reason this ADR is cheap to implement:
`supervisor/src/lock.rs` keeps `LockState`, the Renderer keeps only the protocol object, and the
Supervisor outlives the crash holding the fact that a lock was active.

## Decision 1: the authoritative Renderer's exit is an event

`main()`'s `select!` gains an arm that waits on `authoritative.child`. A Renderer that exits is
something the Supervisor learns immediately, not something it infers later from pushes failing.

This is worth stating as a decision because the alternative is what the code does now and it looks
harmless. A `select!` full of capability channels reads as complete. Nothing about it announces
that the one process the whole Supervisor exists to feed is unwatched.

## Decision 2: a departure is classified, and the lock state is part of the message

The exit is reported as one of three things (a clean exit, a non-zero exit, a signal) and the log
line names whether a lock was active at the time.

The classification is a pure function over the exit code and signal, tested without a process,
because the interesting cases are the ones that are awkward to produce on demand: a SIGKILL from
the OOM killer, a panic's non-zero exit, and the clean `0` that a reap during shutdown produces and
that must never be reported as a crash.

The lock state belongs in the message because the two situations need different reactions from
whoever reads the log. A Renderer that dies unlocked costs a bar. A Renderer that dies locked costs
the session.

## Decision 3: the Supervisor respawns, with a brake

On an unexpected departure the Supervisor spawns a replacement Renderer rather than continuing with
none. The brake is a bounded number of restarts inside a window; past that it stops and says so.

The brake is the decision, not the respawn. A config that kills the Renderer on evaluation kills
every replacement too, and an unbraked loop turns one dead bar into a strobing lock screen that is
harder to escape than the thing it was fixing. A restart policy without a stop condition is a worse
failure than no restart policy.

## Decision 4: a respawn re-acquires the lock when `LockState` says one was active

The replacement Renderer is told to take the lock again when the Supervisor's own `LockState.active`
says the session was locked, and this leans on the takeover measured above.

The Supervisor is the only participant that can make this call. The compositor will not unlock and
should not, the dead Renderer knew and is gone, and the replacement starts with no history. The
`acquisition` counter already in `LockState` exists for a neighbouring reason (tying a PAM outcome
to the lock it was started for) and gives the re-acquired lock its own identity for free.

Where a compositor refuses the takeover, this degrades to what the user already has today, a locked
session and a VT switch, and it must say so rather than retry into the brake.

## Consequences

**ADR-0042's swap gate stays, and is still the primary defence.** Deferring a generation swap while
a lock is requested or active prevents the Supervisor from reaping the process holding the lock
object. That covers every planned reap and remains the cheaper mechanism. This ADR covers only what
prevention structurally cannot: a crash nobody scheduled.

**A respawned Renderer is a new generation, not a resumed one.** Every `state` signal is gone,
because the Lua VM that held it is gone. An in-place reload preserves those (ADR-0044 decision 5);
this cannot, and a config that treats a click counter as durable will see it reset. That is the
honest outcome and not worth hiding behind a state-restoration mechanism nothing has asked for.

**The Supervisor's own death is still unhandled, and is now the larger remaining hole.** Measured
the same day: `SIGKILL` the Supervisor and the Renderer survives, reparented to `systemd --user`,
spinning its 15ms poll at 17.8% of a core with a shell nobody can reach. It survives because
`try_recv`'s `Err` collapses `Disconnected` into `Empty`, so a dead socket thread reads exactly
like an idle one (the `ponytail:` at `renderer/src/wayland/mod.rs:634`). This ADR does not fix that
direction. It is named here so the next reader does not mistake the crash path being covered for
the process boundary being covered.

## Rejected: let a session manager restart the whole stack

systemd could restart the Supervisor, which would spawn a Renderer, and no code would be needed.

It loses the one fact that makes recovery possible. A restarted Supervisor's `LockState` is
`Default`, so `active` is false, and the new stack comes up believing the session is unlocked while
the compositor keeps it locked. That is precisely the quickshell failure reproduced above, rebuilt
deliberately. The Supervisor surviving the crash is not incidental to this design, it is the
mechanism.

## Rejected: have the Renderer re-acquire its own lock

The Renderer could notice its own lock died and retake it. It cannot: the process that would notice
is the process that died.
