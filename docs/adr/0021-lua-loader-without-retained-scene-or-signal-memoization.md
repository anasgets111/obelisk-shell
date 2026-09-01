# Lua loader ships without retained-scene reconciliation or signal memoization

> The 5ms CPU cap cannot bound a deepening recursion, and Phase 19 item 1's review measured it. Each
> nested `call_with_cpu_cap` pushes its own deadline, and the instruction hook reads `stack.last()`,
> the innermost and therefore always the freshest. A recursion that keeps nesting keeps pushing later
> deadlines, so the check provably never fires and the process dies of stack exhaustion instead.
> Reading `stack.last()` was deliberate and its stated reason still holds: a finished inner call must
> hand enforcement back to the outer one rather than erase it. The mistake is that the innermost
> deadline is also the most generous. Checking the minimum of the stack keeps the hand-back property
> and makes the budget apply to the whole nest, which is what "5ms" was meant to mean.
>
> Two further gaps, same review. The cap is per `get_value` call, so one layout pass reading four
> signal-valued properties grants four independent budgets rather than one. And `remove_hook` runs
> when `get_value` returns, so a resolved table's `__index` metamethod executes with no hook at all:
> a `__index` of `while true do end` hangs unkillably. Both are `build-steps.md` Phase 19 items 3
> and 5.
>
> Phase 19 item 3's own review then found the paragraph above understated the problem, and measured
> all of it. `Signal::get_value` resolves a `Computed`'s dependencies *before* calling
> `call_with_cpu_cap`, and each dependency's push has already been popped by the time it returns, so
> a dependency chain nests Rust frames while the deadline stack stays at depth 1. A 200 link `map`
> chain reached `get_value` depth 200 with a maximum stack depth of 1; a 5000 link chain aborted the
> process with neither the depth cap nor the time cap firing. The fix is to push the deadline around
> dependency resolution as well, which is what makes the stack depth mean nesting depth and what
> makes one deadline govern a whole dependency graph.
>
> Two holes make the cap advisory rather than enforced, which matters because ADR-0039 leans on
> "it is enforced, not merely measured" as one of three reasons a slow evaluation cannot wedge the
> Wayland thread. The hook raises an ordinary Lua error, so a `pcall` inside a computed body catches
> it and carries on: a measured body ran 37.6 ms, over seven times the cap, and returned a
> partially computed number rather than an error. And `Lua::set_hook` installs per Lua thread, so a
> body that works inside a coroutine is never hooked at all, measured at 5.75 seconds uninterrupted.
> `Lua::set_global_hook` covers the second. The first needs a second gate at the Rust boundary,
> after the call returns, that a config cannot catch. Until both land, treat ADR-0039's third
> bounding argument as weaker than it reads.
>
> **The two gaps in the second paragraph are now closed, and not by changing this cap.** A layout
> pass holds its own deadline, `lua::signal`'s `LayoutPassBudget`, entered by `Scene::apply` and
> held until it returns. It keeps the instruction hook installed across the whole pass, which is
> what finally puts a resolved table's `__index` under a limit: that metamethod runs between signal
> evaluations, not inside one, so no per-`get_value` budget could ever have reached it. And because
> one deadline spans the pass, four signal-valued properties no longer buy four independent 5ms
> budgets.
>
> Two independent deadlines rather than one, and the earlier expiry wins. Folding the pass into
> this ADR's stack would have broken the invariant the stack rests on: entries are pushed strictly
> LIFO and each is `CPU_CAP` from its own push, so the vector is non-decreasing and `first()` is
> its minimum in O(1). A longer pass deadline pushed underneath a shorter signal one destroys that.
> Keeping them apart leaves § 1.2's 5ms per evaluation exactly as specified and adds a ceiling on
> the pass that no number of individually-legal evaluations can walk past. The hook's callback and
> the Rust-boundary gate both consult whichever expired, and say which.
>
> The instruction hook is now shared, so its install and removal are refcounted rather than keyed
> on this stack's depth. Keyed on the depth, a signal evaluation finishing inside a layout pass
> took the stack to 0 and removed the hook the pass still needed, which would have left every
> metamethod after the first signal read unbounded again.

Phase 10's title ("Lua VM Bootstrap & the Loader") and its build-steps.md text scope a real
`mlua` VM instantiation and the loader: Lua evaluation of `shell.lua` into a node tree and
surface topology (`CONTEXT.md`, Loader). Matching that scope, this phase does not build:

1. **Full retained-scene reconciliation** (Phase 12). `renderer/src/lua::deserialize_lua_table`
   converts exactly one Lua table into one `VirtualNode`, pulling `kind` out and copying every
   other key as-is -- it never recurses into a `children`/`child` value, never matches fresh
   nodes against a previous tree by identity, and never tears anything down. That's Phase 12's
   retained-scene transaction (`CONTEXT.md`, Retained-scene transaction), a different operation
   from this phase's shallow, single-level conversion.

2. **Write-command dispatch back through Phase 9's socket.** `button`'s `on_click` and
   `textfield`'s `on_change` are accepted as ordinary Lua function values in a node's
   `properties` bag -- nothing calls them, and nothing routes a write command through
   `supervisor/src/socket.rs`'s connection. No capability handler exists yet to be the
   consumer (same ceiling ADR-0020 item 1 already hit for the transport side).

3. **`textfield`'s `secure_submit` wiring to `SecureBuffer`** (Phase 15). The `textfield`
   constructor tags and returns its props table like every other node; `mask_character`'s value
   passes through untouched. Nothing connects it to `shared::SecureBuffer` or `wp-text-input-v3`.

4. **Computed-signal memoization and invalidation.** `renderer/src/lua/signal.rs`'s
   `computed`/`map` recompute their function fresh on every `:get()` call. Deciding *when* a
   cached computed value goes stale is the Watcher's job (`CONTEXT.md`, Watcher) once a
   dependency's underlying value can actually change after construction -- nothing in this phase
   pushes a new value into an existing `Signal`, so there is no staleness to track yet.

5. **Per-field node-property schema validation** (`oblisk-idl-api-specs.md` § 5.2's table).
   Node constructors don't reject a `width` outside `[0, 8192]`/`"Fill"`, an invalid `align_h`,
   or a malformed hex color. Phase 12's layout engine is the actual consumer that needs typed,
   validated properties; validating here would be built ahead of its only real caller.

6. **Real `oblisk.*` system-signal population** (§ 2.1-2.13). `Signal::try_new_direct` is
   exercised only by this phase's own tests, constructing values by hand. Phase 11 gives the
   Supervisor's real `StateSnapshot` push somewhere to land as a `Signal`.

7. **`list`'s reconciliation-aware repeater semantics and `button`'s real input dispatch**
   (Phase 12/14). Both constructors are the same thin tag-and-return sugar as every other node;
   `list`'s `source`/`itemfn` and `button`'s `on_click` are stored as opaque property values.

8. **A production call site for `Loader` in `main.rs`.** `mod lua;` is declared (compiled,
   clippy'd, tested) but nothing calls `Loader::new`/`evaluate` outside this module's own tests.
   No real `shell.lua` file location exists yet (the Watcher, Phase 13, owns that), and Phase
   11's acceptance test -- evaluating a one-line `shell.lua` against a real pushed
   `StateSnapshot` -- is what gives the loader its first production caller. Matches
   `supervisor/src/socket.rs`'s `GenerationRegistry::send_to` from Phase 9: real, tested, and
   unwired until a later phase gives it a reason to run.

Decision: `renderer/src/lua/` ships four pieces, each scoped to exactly what Phase 10's research
tasks ask for:

- **The type-marshalling boundary** (`marshal.rs`, § 1.1). `check_number`/`check_integer`/
  `check_string` enforce the three rules the spec's type table actually constrains beyond what
  `mlua` already maps automatically: finite `f64` (NaN/Inf rejected), `i64`/`u64` within
  `[-2^53+1, 2^53-1]`, and a 64KB `String` cap. `Signal::try_new_direct` is the one real caller
  this phase gives it, since `Box<Signal<T>>` is the one row in § 1.1's table this phase
  genuinely exercises.

- **`Signal`/`computed`** (`signal.rs`, § 1.2). A `Signal` is either `Direct` (a plain Rust-
  pushed value) or `Computed` (a Lua closure plus its dependency `Signal`s, re-run on every
  `get()`). `computed(dependencies, fn)` calls `fn` with each dependency's *current value* as a
  positional argument, not the `Signal` handles themselves -- § 1.2 doesn't pin this calling
  convention down explicitly, and passing already-unwrapped values means a `computed` body never
  has to redundantly call `:get()` on its own inputs.

  The 5ms CPU cap is real, not just measured after the fact -- this phase's one named research
  task. `Lua::set_interrupt`, the API build-steps.md's own text names, turned out to be gated
  behind `mlua`'s `luau` feature (checked directly against the vendored `mlua` 0.12 source,
  `src/state.rs`); this workspace builds against `lua54`, not Luau. The non-Luau equivalent is
  `Lua::set_hook` with a `HookTriggers { every_nth_instruction: Some(1000), .. }` counter-based
  hook: the hook callback checks elapsed time since the call started and returns
  `Err(mlua::Error::runtime(..))` once it's over budget, which `mlua` propagates as a genuine Lua
  error out of the running closure -- not a value silently returned late.

  `Lua::set_hook`/`remove_hook` operate on one unstacked slot per Lua thread, and
  `call_with_cpu_cap` is reentrant: a `computed`/`map` body can read a *second* `Signal` (an
  upvalue or a global, not just its own declared `dependencies`) before it returns, which
  re-enters this same function. An install-before/remove-after-every-call version (this ADR's
  first draft) let the inner call's `remove_hook()` silently strip the outer call's still-active
  cap, leaving the rest of the outer body unguarded -- a review caught this before it landed.
  The fix is a deadline stack in `Lua::app_data`: the hook installs only on the 0->1 depth
  transition and is removed only on the 1->0 transition, and it always checks the innermost
  (topmost) deadline, so a finished inner call hands enforcement back to the outer one instead of
  erasing it. Proven by `a_nested_get_call_inside_a_computed_body_does_not_strip_the_outer_calls_cap`
  (the reentrant case) and `a_cap_abort_does_not_leave_the_hook_installed_for_later_unrelated_evaluation`
  (an aborted cap doesn't leak into later, unrelated top-level `shell.lua` evaluation).

- **Node constructors and `VirtualNode`** (`nodes.rs`, § 5.2/§ 6.1). `rect`/`row`/`column`/
  `text`/`icon`/`button`/`list`/`textfield`/`surface` are registered as Lua functions that tag
  their props table with a `kind` field and return it unmodified otherwise -- matching
  `docs/oblisk-tdd-test-harness.md` § 4.1's own worked example ("Echo table structure back to
  Rust"). `deserialize_lua_table` (the harness doc's own name) converts one such table into a
  `VirtualNode { kind, properties }`, a shallow, single-level conversion: `kind` is pulled out,
  every other key is copied into `properties` as a raw `mlua::Value`, and nested tables
  (`children`, `child`) are left unconverted.

- **`Loader` and topology extraction.** `Loader::evaluate(source)` evaluates `source`, requires
  its top-level return to be a `surface` node or a non-empty array of them (§ 6.1), and returns
  `LoadOutput { surfaces: Vec<VirtualNode> }`. Because `deserialize_lua_table` never recurses,
  each surface's own topology fields (`id`/`layer`/`anchor`/`monitor`/`exclusive`) sit directly
  in its `properties` bag, readable without walking `child` and without running Phase 12's
  retained-scene transaction -- exactly the "distinct, cheap-to-diff output" build-steps.md asks
  for. No side-channel (`Lua::app_data`, a mutating closure) was needed to get there.

Tested against (TDD, `/tdd` discipline, one seam per cycle): `Loader::evaluate` (a Lua syntax/
runtime error surfacing as `LoaderError::Eval`; a non-surface top-level return surfacing as
`LoaderError::InvalidTopLevelReturn`; single and array-of-surfaces top-level returns); `Signal`/
`computed` (`get`, `map`, multi-dependency `computed`, a busy-loop `while true do end` closure
actually aborting near the 5ms mark rather than hanging or returning stale data, and that an
aborted cap doesn't leak into a later unrelated evaluation); the marshalling boundary's three
rejection rules directly; node-table deserialization (`kind` pulled out, every other field kept,
a nested `child` left unconverted, all nine § 5.2/§ 6.1 node kinds constructing and tagging
correctly).

`docs/oblisk-tdd-test-harness.md` § 4.1 names a canonical `renderer/tests/test_lua_marshalling.rs`
integration-test path. `renderer` (like `supervisor`) is a binary-only crate with no `src/lib.rs`,
so a `tests/` integration file can't reach a private `mod lua` at all -- the same reason Phase 5-9
never produced any of the harness doc's other named `supervisor/tests/*.rs` files either. Tests
live inline (`#[cfg(test)] mod tests` per file), matching `socket.rs`'s Phase 9 precedent; the
harness doc's `VirtualNode` type name and marshalling-test intent are honored, its literal file
path is not.

Upgrade path, in order: (a) Phase 11 gives the Supervisor's real `StateSnapshot` a `Signal` to
land in and gives `Loader` its first production caller; (b) Phase 12's retained-scene transaction
replaces `deserialize_lua_table`'s shallow, single-level conversion with real identity-matched,
child-first-reconciled nodes, and its layout engine gives node properties real type validation
(item 5); (c) Phase 13's Watcher builds the dependency-invalidation graph that makes memoizing a
`Computed`'s value worthwhile (item 4), diffing this phase's topology output to decide swap vs.
in-place; (d) Phase 14 gives `button`/`textfield` real input dispatch and wires write commands
back through Phase 9's socket (items 2, 7); (e) Phase 15 wires `textfield`'s `secure_submit` to
`SecureBuffer` (item 3).

This does not contradict `docs/oblisk-idl-api-specs.md` § 1, § 5, § 6, `build-steps.md`'s Phase
10 text, or ADR-0019: all describe or assume the target shape once retained-scene reconciliation,
real signal population, and write-command dispatch exist, and none of that is built here. It also
does not contradict ADR-0020: this phase's `Loader` has no dependency on `supervisor/src/socket.rs`
or `shared::framing` and adds none.
