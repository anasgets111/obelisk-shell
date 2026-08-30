# The Renderer exits when the Supervisor is gone, and a service manager reruns the pair

ADR-0058 covered one direction of the process boundary: the Renderer dies, the Supervisor notices,
respawns it, and retakes the lock. Its Consequences named the other direction as the larger
remaining hole and left it there. This closes it.

The hole was not that nothing recovered the Supervisor. It was that the Renderer did not notice.
`try_recv`'s `Err` collapses `Disconnected` into `Empty`, so a dead socket thread read exactly like
an idle one, and the 15ms poll at the bottom of the loop kept turning.

## What the surviving Renderer actually was

Measured before the change and again after, in a nested niri so no real session was at risk:

| the situation | before | after |
| --- | --- | --- |
| `SIGKILL` the Supervisor, no lock up | Renderer survives, reparented to `systemd --user`, spinning at 17.8% of a core | Renderer exits `70` |
| `SIGKILL` the Supervisor while the Renderer holds the lock | the same, plus a lock screen that cannot authenticate anyone | Renderer exits `70`, session stays locked |
| a Renderer started with no Supervisor listening | runs a whole shell with every signal frozen at whatever it never received | exits `70` before the loop's second turn |

The middle row is the one that matters, and the third is the one that was easiest to reach by
accident. Both were reported as healthy.

What the survivor could still do was the trap. It painted. It hit-tested clicks and ran their
handlers. What it could not do was reach a single thing behind them: every capability lives in the
Supervisor, `process.run` writes `Command` frames to a socket nobody reads, and PAM is a Supervisor
worker (ADR-0028). A shell that answers the pointer and nothing else is worse than one that is
visibly gone, because only the second kind sends anyone looking for the cause.

## Decision 1: a disconnected inbound channel ends the process

`TryRecvError::Disconnected` is now its own answer in the drain, and it exits.

The alternative is a reconnect loop, and it is the wrong shape here. There is no state on this side
worth reconnecting *with*: a new Supervisor is a new `LockState`, a new capability roster and a new
generation id, so a Renderer that reattached to it would be a generation the Supervisor has never
heard of, holding signals hydrated from a process that no longer exists. The pair is the unit that
restarts, not either half.

## Decision 2: the exit does not unlock, even holding the lock

A Renderer that holds `ext_session_lock_v1` when its Supervisor dies exits still holding it.

There is a version of this that unlocks first, on the reasoning that a lock screen which cannot
reach PAM is a lock screen nobody can get past. It is not a close call: it makes one `kill` on a
process a way past a lock screen, which is the whole thing a lock screen is for. Exiting while
locked leaves the session locked behind whatever the compositor shows for a dead lock client, and
the way back in is a VT switch. SCTK's own `SessionLockInner::Drop` calls the same choice failing
secure.

Measured, in the nested rig: after the Supervisor was killed with the lock up and the Renderer
exited, a probe client that locked the session got niri's `locking session (replacing existing dead
lock)` rather than a plain `locking session`, and no `unlocking session` appeared between the two.
niri names a takeover differently from a fresh lock, which is what makes the log a real answer here
rather than an inference.

One implementation detail carries that decision, and it is worth naming because the obvious version
is wrong. This exits with `std::process::exit`, not by breaking the loop. Breaking returns from
`run` and drops `App`, and SCTK's `SessionLockInner::Drop` sends a bare
`ext_session_lock_v1.destroy`, which is `invalid_destroy` once `locked` has been sent, the one
protocol error ADR-0052 was built to stay away from. Skipping the destructor closes the connection
instead. The compositor reads that as the same lock client death and logs nothing, which the nested
run confirms: not one protocol error in the compositor's log across all three cases.

## Decision 3: a tripped restart brake exits with a code that means "do not restart"

The Supervisor now returns a `Shutdown` rather than `()`, and the brake's give-up becomes exit `3`.

This exists because the two stop conditions operate on different timescales. systemd's default start
limit (5 starts in 10 seconds) catches a fast restart loop with no help from anyone. ADR-0058's
brake is slow by construction: three Renderers have to die, inside a 60 second window, so a restart
policy that could not tell that exit from a signal would rerun the whole stack roughly once a
minute, forever, under every rate limit systemd applies by default. The code is what carries the
difference across the process boundary.

Measured with a scratch unit carrying the `Restart=` and `RestartPreventExitStatus=` lines this
ADR's own unit file uses:

| Supervisor exit | what systemd did |
| --- | --- |
| `0`, a clean shutdown | restarted it |
| `1`, a startup failure | restarted it 5 times in 10s, then stopped and marked the unit failed |
| `3`, the brake | ran it once and stopped |

## Decision 4: the rerun is a service manager's job

`packaging/oblisk-shell.service` is the first packaging in this repo, and it exists because niri's
`spawn-at-startup` does not restart what it spawns. That is niri issue #2986's answer verbatim, the
same issue ADR-0058 reproduced with quickshell: recovering a dead shell is the shell's problem.

`PartOf=graphical-session.target` is doing more work than it looks like. Session end *stops* the
unit rather than letting the Supervisor exit into a restart, which is what keeps `Restart=always`
from fighting a logout. Stopping is also what collects a Renderer or a `process.run` child that
outlived a Supervisor which ran none of its own cleanup, since the whole control group goes with the
unit. Decision 1 is the part that works when nobody is running systemd at all; this is the part that
works when decision 1 has been defeated by something worse.

## Consequences

**A restart while the session is locked comes back believing it is unlocked.** The new Supervisor's
`LockState` is `Default`, so `active` is false, while the compositor is still locked from before.
The new Renderer paints its ordinary bar behind the compositor's lock fallback where nothing can see
it. This is exactly the quickshell failure ADR-0058 measured, and this ADR reaches it through a door
of its own: killing the Supervisor mid-lock now ends with a restarted stack that has forgotten the
lock instead of a spinning one that remembered it. The trade is still worth making, because the
spinning survivor could not authenticate either, but the hole is real and it is the next thing to
build.

The fix is cheap and deliberately not built here: a flag in `$XDG_RUNTIME_DIR`, written when the
lock is taken and removed when it is released, read once at Supervisor startup. The runtime directory
dies with the session, so a fresh login finds no flag and a restart mid-lock finds one, and
ADR-0058 decision 4's re-acquisition path already exists to act on it. It is left out because it
makes a freshly started Supervisor lock the screen on its own, which is a behaviour to build
deliberately with its own test, not to slip into a change about process death.

**A Renderer can no longer be run on its own.** `cargo run -p renderer` now exits immediately
instead of taking over the session. That is a side effect rather than the point, but it defangs the
footgun section 6 of build-steps.md found: `--validate` is still not a flag, and running it still
starts a live shell, but that shell now lasts a fraction of a second rather than the twelve minutes
it held `/dev/dri/renderD128` for when it was found.

**The signal is coarse.** The Renderer learns that the Supervisor is gone, never why, and there is
nothing finer available: a closed socket is a closed socket whether the peer was killed, panicked or
exited cleanly. The exit code it uses (`70`) is written for a journal, not for a handshake, since
the process that classifies Renderer exit codes is the one that just died.

## Rejected: reconnect to a new Supervisor instead of exiting

Covered under decision 1. A reattached Renderer would be a generation the new Supervisor never
spawned, carrying signals hydrated by a process that is gone. Every one of those is a fact the
Supervisor owns, so making them agree again is a whole synchronisation protocol, in service of
skipping a process spawn that takes milliseconds.

## Rejected: have the Renderer respawn the Supervisor

The surviving process could re-exec its own parent, which needs no packaging at all.

It inverts the ownership this whole design rests on. The Supervisor spawns the Renderer, holds the
capabilities, runs PAM and decides what is authoritative; a Renderer that spawns Supervisors is a
Renderer that can hand itself a fresh capability roster, and ADR-0042 put the lock decision in the
Supervisor precisely so the process on the glass could not make it. A unit file is nine lines and
does not move any authority.
