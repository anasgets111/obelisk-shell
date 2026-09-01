# The capability roster is a type, and the module tree mirrors it

The 2026-09-01 architecture review asked what makes a capability cheap to add. The answer was that
nothing did: `run_supervisor` was one ~800-line function, and it was the only scope where sixteen
controllers and twenty channels coexisted. Every capability was five locals in that function, so
adding one meant six edits across five separated regions of `main.rs`, and two of those six were
`&str` matches that nothing checked.

Six lists named the roster. Four were guarded -- `renderer/lua/namespace.rs` and `stubs.rs` iterate
it, `stubs.rs` asserts its schema table equals it, `snapshot.rs` debug-asserted membership on every
push. The two that decided whether a capability *starts* and whether its commands are *dispatched*
were not, and those are the two where being wrong costs the most: an unmatched name meant the Lua
member existed, read `nil` forever, and produced no error anywhere.

## Decision 1: `shared::Capability` replaces `CAPABILITIES: &[&str]`

The roster is an enum. Both `main.rs` matches became exhaustive matches inside `Capabilities`, so a
new variant fails the build at exactly the two arms that need code.

`snapshot::push_snapshot` takes it too, which deletes ADR-0037's `debug_assert` rather than
converting it: with a `Capability` parameter there is no off-roster name left to pass. A runtime
check that becomes a type is the version of that check worth having.

The enum, `ALL` and `as_str` come from one `roster!` macro list. That was not the first attempt --
three hand-kept lists were, and writing the test for them is what exposed the hole: a variant added
to the enum and to `as_str` but forgotten in `ALL` compiled, passed every test, and was silently
absent from the Lua namespace, the stubs and the schema check, all three of which iterate `ALL`. A
macro is more machinery than this codebase reaches for, and it earns it here by making that state
unwritable. One line added to `roster!` now fails the build in two places and nowhere else, which
was verified by adding one and reading the errors.

`idle` and `polkit` stay off the roster and are covered by the Supervisor's own `Startable`: `idle`
because it is event-shaped rather than snapshot state (ADR-0032), `polkit` because it arrives from a
`secure_submit` naming it rather than from a capability read (docs/adr/0070 decision 5). Both were
previously string arms in the same unguarded match.

## Decision 2: `Capabilities` owns the controllers, `main.rs` owns the engine

Sixteen `Option<Controller>` locals and twenty channels became two structs. `Capabilities::new`
builds every channel and returns the receiving half as `Signals`; `start`, `push` and `dispatch` are
the three things a capability does. `main.rs` drops from 1081 code lines to ~760, and what is left
is the reload/PBA/lock/generation loop it should have been.

**The split between `Signals::next` and `Capabilities::push` is load-bearing, not stylistic.**
Sixteen select arms could not simply become one, because `tokio::select!` cancels its losing
branches: `network` and `bluetooth` `await` while building their state, and folding that await into
the raced future would let a busier branch drop a signal mid-flight. So `next` only ever awaits
`recv()`, and `push` runs in the winning arm's body, which `select!` never cancels. The sixteen arms
today are safe for the same reason -- a chosen arm's body runs to completion -- and this preserves
it rather than rediscovering it later as a bug.

This is not ADR-0037's rejected merged channel, and its "do not re-propose" condition is not being
claimed. The channels are still sixteen typed single-variant ones; no controller serializes early;
`idle` and the lock arm are still their own arms in `main.rs`, which is what that ADR called the two
permanent carve-outs. Only the await site moved. Worth recording that ADR-0037's rejection rested on
each capability's residue being "a one-line select arm", and that ADR-0070's lazy-start `Option`
wrapper is what later turned each into six -- so this restores that ADR's own premise.

It is also not a trait or a registry of boxed objects. Controllers have genuinely different shapes
(`build_state` vs `snapshot` vs an `async handle_signal`, and one that is a channel rather than a
controller), and ADR-0037 decision 3 already settled that dispatch is "static calls, no registry, no
trait". That decision is unchanged; the calls just have a struct to hang off.

## Decision 3: one module per roster entry, flat, under `capabilities/`

The roster is flat and it is the public interface -- the same name appears in `shared::Capability`,
in `oblisk.<name>`, and in every command's `capability` field. The modules sat at four different
depths, and one grouping was by transport:

    dbus/{network,bluetooth,tray,notifications,mpris,power}
    hardware/{sysinfo,keyboard,battery,brightness,idle}
    {audio,privacy,updates,system,workspaces,applications}/
    lock.rs

`battery` and `power` are the same subject split by how the Supervisor happens to talk to the
device, which is a fact about our plumbing and not one a config author can see. Both trees are
dissolved into `capabilities/<name>/`, one per roster entry, so the module path is the roster name
is the Lua name.

The two groupings did share something real, and those moved rather than being duplicated:
`hardware::read_attr` (sysfs, used by `battery` and `brightness`) and `dbus::parse_bool_arg` (used
by five capabilities) are now on `capabilities` itself. `shm_icons` is shared by exactly `tray` and
`notifications`, so it sits beside them. `polkit` was never a capability -- it has no roster entry,
no snapshot and no commands, and `process`, `updates`, `network` and `pam_worker` all reach for it
-- so it moved out to a top-level `crate::polkit`.

`lock` keeps one asymmetry, and it is not an artifact: `LockController` is built at boot rather than
started on demand, because the Supervisor's own relock path (docs/adr/0060) commands it before any
config has read anything. So `Capabilities::dispatch` is handed it rather than holding it, and its
`start` arm is an explicit no-op. The alternative is moving boot-time lock ownership into a struct
whose entire premise is lazy construction, which would make the lie load-bearing.

## What this does not decide

The Renderer's organisation. Its three largest files -- `layout/scene.rs` (1476 code lines),
`wayland/surface.rs` (1139) and `wayland/input.rs` (1074) -- were measured as part of the same
review and left alone: each is one cohesive thing (the layout algorithm as pure functions plus one
`Scene` impl, one `impl App`, and SCTK handler impls plus pure helpers). Splitting a file because of
its length, rather than because two things inside it want different lifetimes, is churn.
