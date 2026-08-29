# The session lock is commanded through a capability, and its surfaces live for the lock

ADR-0042 settled who holds `ext_session_lock_v1`: the Renderer holds it, the Supervisor supervises
the client and issues the lock command. It left four things open that the implementation cannot
avoid answering, and each one is visible from a config.

## Decision 1: `oblisk.lock` is an ordinary capability

ADR-0042 says the Supervisor "issues the lock command" and § 6.4 says "locking is triggered by the
Supervisor", and neither says what makes it issue one. No capability named `lock` exists in the
service spec, so this decision is where one comes from.

The answer is Lua, through ADR-0037's generic capability dispatch, the same way every other write
action already works:

```lua
button { on_click = function() oblisk.lock:invoke("lock") end, child = text { content = "lock" } }
```

`lock` is an action on a capability the Supervisor owns, so this needs no second mechanism and no
policy invented in Rust.

There is no matching `unlock` action, and the asymmetry is the decision rather than an omission. A
lock screen's node tree is Lua too, it is the only thing on the glass while the session is locked,
and its `button` callbacks run, so an `unlock` action would put a one-click path past PAM on the very
surface PAM is guarding. The unlock direction therefore has exactly one caller by construction, the
Supervisor's own `PamOutcome::Success` arm, which is what makes ADR-0042's "never call
`unlock_and_destroy` except on a successful authentication" checkable by reading one match arm
instead of trusting every config.

The alternative was a lock timeout the Supervisor owns internally and applies whether or not the
config asks. It locks a session whose config is broken, which sounds like the secure choice and is
not: nothing else in Oblisk does anything the config did not ask for, and a shell that locks a
session no config declared a lock screen for is a shell that strands the user at a black screen.
Decision 3 covers that case directly instead.

This costs one thing Phase 23 did not plan for. Lua cannot write at all yet: `Signal` exposes `get`
and `map`, and § 3.2's write commands have no implementation outside `process.run`'s hand-built
envelope. So `oblisk.lock` as a capability with no caller is dead code, which this codebase does not
ship. Phase 25 item 1, the one generic `CommandEnvelope`-building method that covers every write
command, comes forward into this phase. Items 2 through 4 (revision tracking, the `oblisk`
namespace, `oblisk.version`) stay where they are, except for the one name decision 2 forces.
`expected_revision` is `0` for the same reason `process.run` hardcodes it: a lock command is not a
read-modify-write, so there is no state for § 7.3's guard to be stale about.

That exception is item 3, for one name only. The Renderer seeds every `shared::CAPABILITIES` entry
as a bare Lua global, and § 6.4's `lock` node constructor already owns that name. The seeding loop
runs after `NODE_KINDS` registration, so a bare `lock` signal would overwrite the constructor
silently and break every `lock { ... }` declaration in the file that declares the lock screen. So
`lock` is seeded as `oblisk.lock`, which is what § 2 calls it anyway, and the other ten keep their
bare names until Phase 25 item 3 moves them together. One `oblisk` table with one member is a
smaller change than renaming ten globals mid-phase, and it is the direction of travel rather than a
detour.

The Supervisor still owns the decision in the sense ADR-0042 meant. It is the process that holds the
command, tracks whether the lock took, gates swaps behind it (decision 4), and survives the Renderer
that crashed while holding it.

## Decision 2: a `lock` is returned at the root, and its lifetime is the lock

This reverses a Phase 22 decision. `renderer/src/lua/require_surface` currently rejects a `lock` at
the root of `shell.lua` with "a lock surface exists only while the session is locked, so the
Supervisor is what makes one appear", and `NODE_KINDS` has no `lock` constructor to build one with.
That leaves § 6.4's "declaring it says what the lock screen looks like" with nowhere to write the
declaration, which is why it has to move now rather than later.

The rejection conflated two things ADR-0049 had already separated for `window` and `popup`. Where a
declaration lives and when its Wayland object exists are independent. A `window` is returned at the
root and has no `xdg_toplevel` until `visible` goes true; twenty declared popups cost twenty retained
nodes and zero Wayland objects. A `lock` is the same shape with a different trigger: returned at the
root, one retained node, and no `ext_session_lock_surface_v1` until the compositor says `locked`.

So `lock` joins `NODE_KINDS` as a fourth constructor, `require_surface` admits it, and it expands to
one instance per output the way a `panel` with `monitor = "All"` does.

§ 6.4 already states the property shape ("there is no `visible`, `monitor`, `anchor`, or size").
That part stands, and this records why, because ADR-0049 made `visible` create and destroy the object
for the other two roles and the obvious reading is that a third joins them.

The compositor decides when lock surfaces exist. They are created after `locked` arrives and
destroyed by `unlock_and_destroy`, and between those two points the protocol requires one surface on
every output. A `visible = false` on a lock surface would either be ignored or would destroy a
surface the compositor is still showing, which ADR-0042 already records as the thing that makes it
"fall back to rendering a solid color". There is no useful reading of the property, so it is not
accepted.

For the same reason a `lock` node has no `monitor` property. `panel` expands per output because
`monitor = "All"` asked it to (ADR-0038 decision 3); `lock` expands per output because the protocol
says "the client is expected to create lock surfaces for all outputs currently present". Offering a
choice that has exactly one legal value is worse than not offering it.

## Decision 3: a config with no `lock` node refuses the lock

`oblisk.lock.lock()` against a config that declares no `lock` node does not acquire the lock. The
Renderer refuses before calling `SessionLockState::lock`, reports the refusal, and the session stays
unlocked.

The alternative is acquiring the lock and painting nothing. That is a black screen with no password
field, and because the protocol guarantees the compositor will not unlock on client death
(ADR-0042), the only way out is a VT switch and killing the shell. Locking a user out of their own
session on a config omission is not fail-secure, it is a denial of service that happens to be
spelled the same way.

Fail-secure is about a lock that was taken. This is a lock that was never taken, and nothing was
protected by it a moment earlier.

**A declared lock screen that cannot authenticate is refused just as loudly.** `lock { id = "x" }`
parses: § 6.4 requires only an `id`, and `child` is optional. It also produces a surface with no
password field, which reaches the same black screen through the guard rather than around it. So the
condition is not "a `lock` node exists" but "a `lock` node whose tree can actually reach PAM", which
means exactly one `secure_submit` field targeting `("lock", "authenticate")`.

Exactly one, not at least one, and the count is load-bearing rather than fussy. A lock surface has
to be typable the moment the compositor gives it keyboard focus, with no pointer click, or a
keyboard-only machine cannot be unlocked. That rule can only arm a field it can identify without
guessing, so two fields on a lock screen would admit the lock and then arm neither. The predicate
that grants the lock and the predicate that arms the keyboard are therefore the same one, not two
that agree in the common case.

**And a lock screen may not lose its way out while it is up.** A surface's `child` is not topology,
so editing it is an in-place reload, which ADR-0042 deliberately leaves ungated so a colour or a
label still applies live. Deleting the password field is the one in-place edit that strands the
session, so an apply is vetoed and rolled back while a lock is held if it would leave the lock tree
unable to authenticate. Restyling a live lock screen keeps working; removing its way out does not.

## Decision 4: acquisition failures go to `rescue`, authentication failures go to the capability

ADR-0042 requires both `finished` cases to surface through `oblisk.rescue` and neither to be
swallowed. That is right for acquisition, and wrong for the failure the user hits far more often.

The split is whether a lock screen is on the glass to read the message:

- **No lock screen.** A refused lock (decision 3, a denied `lock` request, an absent
  `ext_session_lock_manager_v1`) and a compositor teardown (`finished` after `locked`) both leave the
  ordinary scene showing. `rescue` is rendered by the config's own surfaces, so it is the channel
  that reaches the user.
- **A lock screen.** A wrong password happens with the lock surfaces mapped and everything else
  hidden. `rescue` is unreachable there. It reaches the config as `oblisk.lock` state instead, which
  is the tree the lock screen is already built from.

So `oblisk.lock`'s state is `{ active, authenticating, attempts, error }`. `attempts` counts failures
since the lock was acquired and exists because the config cannot reconstruct it: capability state is
sampled at layout time (ADR-0044), not evented, so two consecutive identical failures are one
unchanged `error` string and a counter built in Lua would miss the second. `active` is how a config
learns its `lock()` call took effect at all.

The Renderer sets `rescue` itself rather than round-tripping through the Supervisor. It owns the
signal, and it is the process that learns of the refusal first.

## Consequences

**A masked field had to stop using the text input to make any of this reachable.** Phase 23 item 3
calls authentication "composes what exists", and what existed did not work: `zwp_text_input_v3`
delivers nothing without an input method bound, so a password could never be typed and the lock could
never be left. ADR-0027 carries the amendment; the short version is that a `secure_submit` field
reads `wl_keyboard` directly, for the same reason every other lock screen does.

**Only the Supervisor orders an unlock.** The Renderer never calls `unlock_and_destroy` on its own
reading of a PAM outcome, because it never sees one. The password crosses as a `SecureSubmit` for
`("lock", "authenticate")`, the Supervisor's re-exec'd worker runs the conversation (ADR-0028), and
success comes back as the same command that started this, with `locked = false`. ADR-0042's "never
call `unlock_and_destroy` except on a successful authentication" is then a property of one call site
in the Supervisor rather than a rule the Renderer has to be trusted to keep.

**A compositor teardown is answered with `unlock_and_destroy`, and that is not a hole in
ADR-0042.** `ext-session-lock-v1` makes `ext_session_lock_v1.destroy` a protocol error once `locked`
has been sent, unconditionally, so a `finished` that follows a `locked` has exactly one legal
teardown verb. Reading ADR-0042's "never call `unlock_and_destroy` except on a successful
authentication" as forbidding it gets the worst outcome available: an `invalid_destroy` drops the
connection, the compositor has already ended the lock, and the session finishes unlocked with the
shell dead and no `rescue` message anywhere to show it.

The rule is about initiating an unlock. The compositor initiated this one ("the compositor has
decided that the session lock should be destroyed"), so the verb ends an object rather than a
session. The single path that ends a live lock is still the `SetSessionLock { locked: false }` the
Supervisor sends on `PamOutcome::Success`.

**Deleting the `lock` node while locked is already blocked.** A surface declaration is topology, so
removing one is a `TopologyChanged` report, so it is a swap, so decision 4 of ADR-0042 queues it
until unlock. Lock surfaces cannot lose their tree underneath them, and no separate rule is needed to
say so.

**Nothing locks on idle yet, and that is a different gap.** `SupervisorFrame::IdleEvent` reaches the
Renderer and stops there: no Lua-side `register_threshold` callback registry exists to dispatch it
to. Until that lands, a config's only way to reach `lock.lock()` is an input callback, which is what
the dev config uses. The capability is the same either way, and this is one missing lookup table
away rather than a missing design.

**An in-place reload still restyles a live lock screen.** A colour or a label on the lock tree
changes under a locked session exactly as it does anywhere else. That is the whole point of holding
the lock in the process that paints Lua.

## Deliberately not built: a fallback lock screen

If no `lock` node is declared, there is no built-in screen to fall back to, and ADR-0046's rescue
renderer is not repurposed into one. Rescue exists for a config that failed to evaluate; a config
that evaluated fine and declared no lock screen has not failed at anything. The upgrade path, if a
session ever genuinely needs to lock against the config's wishes, is a Supervisor-owned lock policy
and a built-in lock tree, and both halves are absent on purpose.
