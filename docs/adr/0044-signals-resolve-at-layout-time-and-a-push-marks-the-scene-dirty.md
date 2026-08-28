# Signals resolve at layout time, and a push marks the scene dirty

> Decision 1 has a carve-out it did not state: the four `SurfaceTopology` fields (`id`, `layer`,
> `anchor`, `monitor`) keep rejecting a `Signal` rather than resolving it. Topology is computed at
> evaluation time so `handle_reevaluate` can diff it against `applied_topology` and choose a
> generation swap or an in-place reload, which is ADR-0001's split. A signal in one of those fields
> would resolve once for that comparison and then change underneath the live generation, so a
> surface could move layer or monitor with no swap and the decision would stand on a value that no
> longer holds. Every other property resolves as this decision describes. `build-steps.md` Phase 19
> item 1's "delete `reject_signal` and its twelve call sites" is amended to the same effect: the
> helper survives, renamed, for those fields alone.
>
> Decision 3's "no memoization" is kept, but Phase 19 item 3's review found the cost sits somewhere
> this ADR did not look, and it is not the missing cache. `SignalKind::Computed` holds
> `deps: Vec<Signal>` by value and `Signal` derives `Clone`, so cloning a computed deep-copies its
> whole dependency subtree. `s:map(f)` copies `s`. A config therefore cannot express a shared
> dependency graph at all: `computed({s, s}, f)` does not reference `s` twice, it embeds two copies
> of it. Twenty levels of that is a 2^20 node tree, built in memory before anything is evaluated,
> and evaluating it is 1,048,575 closure calls.
>
> That matters for what the upgrade path actually is. Memoization keyed on signal identity would not
> help, because after the copy there is no shared identity left to key on. Making `Signal` a shared
> reference (`Rc`) is the change that turns a diamond back into a diamond, and only then is a cache
> even meaningful. Recorded here so a future reader does not reach for the cache first.
>
> The decision stands. ADR-0021's 5ms cap is what keeps this survivable, once that cap governs a
> whole evaluation rather than each leaf call (see ADR-0021's amendment): the config gets an error
> instead of a wedged shell, measured firing after roughly 3,200 calls. The original reason to
> refuse a cache is also unchanged, since a cache needs an invalidation rule and no push has one.

Nothing connects a capability's state to the screen. Three facts, each defensible alone, combine
into a shell that cannot react to anything:

1. `RendererClient::apply_state_snapshot` writes the pushed value into the capability's
   `LiveSignalHandle` and returns. It touches no scene and triggers no evaluation.
2. ADR-0039 states this on purpose: "Full re-evaluation happens on config edit, which is rare, and
   on nothing else."
3. Every parser in `layout/node.rs` calls `reject_signal` and fails on `Value::UserData`.

So a `Signal`'s value reaches the scene only when `shell.lua` calls `:get()` during an evaluation,
and an evaluation runs only when the config file changes. Change the volume and no pixel moves.

Fact 3 also contradicts the IDL directly. § 5.1 types `visible` as `boolean / Signal`, § 5.2 types
`text.content` and `icon.name` as `string / Signal`, and `list.source` as `Signal` only.
`parse_visible` and `parse_content` reject exactly the type the spec promises.

This was deferred once, with a named owner, and the owner shipped without it. ADR-0023 item 2 wrote:
"Phase 13's Watcher... is the natural place to resolve a `Signal`-valued geometry property once
there's a cache-invalidation story to hang it on." Phase 13 landed. Nothing picked the item up, and
Phases 18 through 24 never mention it.

It is also a silent prerequisite of work already planned. Phase 19 gates redraws on "the scene
actually changed" and names no mechanism that could produce that answer.

## Decision 1: property parsers resolve a `Signal` instead of rejecting it

`reject_signal` goes away. Where a parser finds `Value::UserData` holding a `Signal`, it calls
`get()` and parses the result under the same rules as a literal. A `Signal` returning a string where
`content` wants a string is valid; one returning a table is the same error a literal table would be.

This makes `marshal.rs` load-bearing for the first time. Its `#[allow(dead_code)]` comes off: a
resolved value is a Lua-authored value crossing into Rust, which is precisely what
`check_number` / `check_integer` / `check_string` were written to guard.

`:get()` inside `shell.lua` keeps working and keeps its current meaning, a value read once at
evaluation time that never updates. Passing the handle is what opts into reactivity. That difference
is worth documenting in the IDL, because both spellings look reasonable and only one is live.

## Decision 2: `LiveSignalHandle::set` marks the scene dirty; the dirty bit re-resolves, it does not re-evaluate

A push sets one flag. On the next loop turn, a dirty scene re-runs `Scene::apply` against the
`LoadOutput` from the last evaluation, held for exactly this purpose. `shell.lua` does not run.

The retained `VirtualNode` tree still holds the `Signal` handles Lua put in it, so re-applying reads
current values through decision 1. Reconciliation matches by position and preserves node identity
and leases (ADR-0023 § 4), so this is a re-resolve of an existing tree, not a rebuild.

The flag is also the missing input to Phase 19's frame gating. "The scene actually changed" now has
a source: a surface whose re-resolve produced different geometry or different paint properties needs
a frame, and one whose re-resolve produced an identical result does not.

`ponytail:` one flag for the whole scene, so any push re-resolves every surface. The ceiling is a
config with many surfaces and a high-frequency capability, where surfaces that reference nothing
from that capability still pay a layout pass. The upgrade path is per-surface flags, which needs a
signal to know which surfaces read it, which is the dependency graph decision 3 declines to build.

## Decision 3: no memoization and no dependency graph

`computed` and `map` keep recomputing on every read, as ADR-0021 chose. The dirty bit is a single
boolean, not an invalidation set. A push re-resolves; it does not trace what depends on what.

This is the correct baseline and not a placeholder. The graph buys skipping work in a tree that a
5ms-capped closure and a per-surface layout pass already traverse in well under a frame. Build it
when a profile names the re-resolve as the cost, with a number attached.

## Decision 4: a generation's Lua VM outlives its retained scene, so an in-place reload does not reset it

`ResolvedNode::properties` and `RetainedNode::properties` are `HashMap<String, mlua::Value>`, and
decision 2 adds a retained `LoadOutput` holding more of them. Every one of those pins the `Lua` that
created it.

`CONTEXT.md` currently defines an in-place reload as "resetting the Lua VM and re-running the
config". The code does no such thing: `handle_reevaluate` calls `evaluate_file` on the same
`Loader`. The doc and the code already disagree, and the code is right.

State the rule rather than leaving it implicit, because the failure it prevents is silent. A reset
that dropped the `Lua` while the retained scene held its values would not crash. `mlua::Value` keeps
the state alive by reference count, so the old VM would leak whole, once per reload, against
ADR-0043's 50 MB per monitor budget. The scene would then read stale values from a VM no config runs
in any more.

One VM per generation, created once and dropped only when the generation ends. An in-place reload
re-runs the config on it and lets the retained-scene transaction replace what changed. Resetting the
VM is a generation swap's job, and a swap gets a new process anyway.

## Rejected: re-evaluate `shell.lua` on every push

The obvious alternative, and it is what a naive reading of "reactive config" suggests.

Rejected because it puts a full Lua run in the path of every volume tick, every battery poll, and
every MPRIS position update. ADR-0039 accepted a real cost when it moved evaluation onto the Wayland
thread, and it accepted that cost specifically because "full re-evaluation happens on config edit,
which is rare, and on nothing else". Re-evaluating per push retracts the premise that argument rests
on and turns a rare stall into a per-frame one.

It is also wrong on identity. Re-running the config builds a fresh tree with fresh closures, so
every `on_click` identity changes on every push and the retained scene reconciles against a tree
that differs everywhere, not only where state changed.

## Decision 5: Lua-authored state is named, and the name is what survives a reload

Live signals are read-only to Lua and written only by Rust, so a config has nowhere to keep "is this
dropdown open" that anything watches. `state(name, initial)` returns a writable `Signal` whose
`:set()` marks dirty through decision 2's flag.

The name is not decoration. A generation holds a `name -> Signal` map that outlives any single
evaluation, and `state` returns the existing signal when the name is already present, ignoring
`initial`. So re-running the config on an in-place reload finds the same signal holding the same
value, and an open dropdown stays open across a config edit.

Quickshell reaches the same result the harder way. `PersistentProperties` is a `Reloadable` that,
on reload, walks the old instance's meta-object and copies every property across by name, matched to
its old self by `reloadableId`. That is property migration between two object trees, which Oblisk
does not need: the map never moves, because decision 4 keeps the VM alive underneath it.

Named state dies on a generation swap, and that is accepted rather than overlooked. A swap means the
config's structure changed and a new process evaluated it, so the map is in the process that is being
reaped. The upgrade path, if it matters, is to serialize the map into the PBA handshake that already
runs between the two processes. A user who just restructured their config is not surprised that a
dropdown closed.

Building this is Phase 22's job, since `popup` is the first thing that cannot work without it. The
shape is settled here because it decides what the constructor's signature is, and a `state(initial)`
without a name cannot be made to survive a reload afterwards.

## Consequences

`oblisk-idl-api-specs.md` § 1.2 gains the distinction decision 1 creates: a handle in a property is
live, a `:get()` result is a snapshot. § 5.1 and § 5.2's existing `/ Signal` type unions become true
rather than aspirational.

`CONTEXT.md`'s **In-place reload** entry drops "resetting the Lua VM", which was never what the code
did and which decision 4 rules out. Two entries are added: **Dirty scene** for the flag, and
**Signal resolution** for the layout-time read.

`list` stays deferred. A `Signal` in `source` resolves to a table under decision 1, but expanding
that table through `itemfn` is Lua execution during a resolve rather than during an evaluation, and
`children_of` does not handle `list` at all yet. ADR-0023 item 1 still owns it.
