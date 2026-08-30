# A restarted Supervisor learns the session was locked from a file in the runtime directory

ADR-0059 made the shell restartable and named what that cost, in its own Consequences:

> **A restart while the session is locked comes back believing it is unlocked.** The new
> Supervisor's `LockState` is `Default`, so `active` is false, while the compositor is still locked
> from before. The new Renderer paints its ordinary bar behind the compositor's lock fallback where
> nothing can see it.

That is the quickshell failure ADR-0058 measured, rebuilt through a door of ADR-0059's own making.
Every other piece of the recovery already exists: ADR-0058 decision 4 asks a generation to take an
orphaned lock over, and the takeover itself was measured against niri 26.04. The only missing part
is the one fact that dies with the process, which is that the session was locked at all.

## Decision 1: the fact lives in a file, because it has to survive `SIGKILL`

`$XDG_RUNTIME_DIR/oblisk-session-locked` exists exactly while the compositor is locked.

The storage requirement rules out everything cheaper. This has to survive the Supervisor being
killed without running a line of its own code, so no in-process option qualifies, and it has to be
read before anything else happens at startup, so nothing that needs a running Renderer qualifies
either. A file whose existence is the whole payload is the smallest thing that clears that bar.

`$XDG_RUNTIME_DIR` rather than a config or state directory, and that choice is what bounds the
staleness. The runtime directory goes away with the user's last session, so a marker can only be
read back inside the login that wrote it. Within one login, a marker that is set means the
compositor really was locked and nothing has unlocked it since.

## Decision 2: it is written off the Renderer's report, never off `LockState.active`

`active` is the wrong source, and reaching for it is the obvious mistake here.

`LockState.active` means "this shell holds the lock". `LockEvent::RendererLost` clears it (ADR-0058
decision 4) precisely because a crashed Renderer holds nothing, while the compositor stays locked
and is required by the protocol not to unlock on client death. A marker driven off `active` would
therefore erase itself in the exact case it exists for.

So the marker reads `shared::LockOutcome` instead: `Locked` sets it, `Unlocked` and `Finished` clear
it, `Refused` leaves it alone because nothing was taken and so nothing was released. `RendererLost`
is not a `LockOutcome` at all and cannot reach the marker even by accident, which is the property
worth having rather than a comment asking someone to remember.

## Decision 3: a set marker at startup feeds the path that already exists

`relock_when_connected` was ADR-0058 decision 4's intent flag, spent when the replacement
registers. It now starts as `Some(SupervisorRestarted)` when the marker is set instead of always
starting `None`, and the rest of that path is untouched.

Which means the acquisition predicate still gates this. ADR-0058 decision 4's refinement stands: a
replacement re-acquires only if the config on disk still declares exactly one `textfield` with
`secure_submit = { capability = "lock", action = "authenticate" }`, checked in the Renderer where
it already lives rather than copied here where a second copy could disagree. A restarted shell
whose config cannot authenticate refuses the lock and says so, which leaves the user exactly where
ADR-0059 left them, at a VT switch, rather than at a lock screen with no way out.

The one thing that did need adding is that the two callers now say different things. "The
replacement could not retake the session lock" is true after a crash and false after a restart,
where there was no replacement and nothing to retake. `RelockReason` carries which one it is and
the log lines read off it.

## What was measured

In a nested niri, the whole cycle, with the marker checked at each step:

| step | marker |
| --- | --- |
| nested compositor up, no shell yet | absent |
| Supervisor up, Renderer painting, nothing locked | absent |
| the lock taken | set |
| `SIGKILL` the Supervisor, and the Renderer exits behind it (ADR-0059) | set, with both processes gone |
| a second Supervisor started | reads it, and asks generation 0 to take the lock over |

The new Renderer then created its lock surface, got `locked`, and gave keyboard focus to its
`secure_submit` field. That last line is the one that matters: the recovered screen is a lock screen
someone can actually type a password into, not a surface that merely covers the glass.

The release direction is covered by unit tests against a real file rather than by that run, and it
is worth saying why rather than leaving a gap unexplained. A genuine `Unlocked` needs a correct
password through PAM, which a test rig does not have. The obvious substitute does not work either:
a second lock client cannot release this lock, because the compositor refuses its takeover while the
Renderer is still holding one, so it never gets `locked` to release in the first place. Attempting
it is what the run's last step actually demonstrated, by way of a protocol error in the throwaway
client.

## Consequences

**The wrong answer is a locked screen, on purpose.** Both error directions exist and they are not
symmetric. A marker that should have been cleared costs one password prompt at the next start. A
marker that should have been set costs the ADR-0059 hole: a shell painting behind a lock fallback
with no way back in but a VT switch. So an unreadable or unwritable marker file is logged and
swallowed rather than treated as fatal, and `is_set` reads a missing file as "not locked" while
every ambiguity elsewhere resolves towards locking.

**One sequence can still produce a stale marker**, and it is worth naming. Kill the Supervisor while
locked, then unlock the session from outside oblisk entirely (a VT login and `loginctl
unlock-session`), then start the shell again. The marker still says locked, and the shell locks a
session the user had already opened. The cost is one password prompt, the sequence needs a VT
switch to reach, and the alternative is worse, so this is left as it is rather than defended.

**Nothing asks the compositor.** It would settle every case above, and `ext-session-lock-v1` has no
request that answers it. A client cannot ask whether the session is locked, and it cannot infer it
from taking a lock either, because a takeover of an existing lock and a fresh lock both come back as
`locked`. niri does distinguish the two in its own log, which is what made ADR-0059's measurement
possible, but a compositor's log lines are not an interface.

## Rejected: serialize the whole `LockState`

The marker could be a JSON dump of `LockState`, which would carry `attempts`, `error` and
`acquisition` across the restart too.

Rejected because every one of those fields is about a lock screen that no longer exists. `attempts`
counts failures since an acquisition, and the acquisition it counted against died with the process;
`acquisition` numbers locks so a PAM answer can be matched to the question it answered, and no
worker survives to answer. Restoring them would show a user a failure count from a shell that is
gone. The one field that means anything across the boundary is the boolean, so the boolean is what
crosses.

## Rejected: clear the marker on a clean shutdown

A Supervisor that exits through its own signal handler could remove the marker on the way out,
which would kill the stale-marker sequence above.

It is exactly backwards. A clean Supervisor shutdown does not unlock the compositor: the shutdown
reap `SIGTERM`s the Renderer, which exits without unlocking for ADR-0059 decision 2's reason, and
the session stays locked. Clearing the marker there would mean locking the screen, stopping the
shell cleanly, and starting it again lands on an unlocked-looking shell in front of a locked
session. That is the failure this ADR exists to prevent, reached through the tidying.
