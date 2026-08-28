# Layout engine ships a stacking model, not a full constraint solver

> Item 2 (`Signal`-valued properties rejected, not resolved) is settled by ADR-0044. This ADR's
> upgrade path handed the item to Phase 13's Watcher; Phase 13 shipped without it and no later phase
> claimed it, leaving the Renderer unable to turn any state change into a pixel. Property parsers now
> resolve a `Signal` at layout time, and a live push marks the scene dirty rather than triggering an
> evaluation.
>
> § 4's positional child matching is amended by ADR-0045. Matching by position alone loses node
> identity whenever a config inserts a node above an existing sibling, so nodes may now carry an
> `id` scoped to their parent, and identified children pair before the rest fall back to this ADR's
> positional rule. Item 1's deferred `list` gains a `key` requirement from the same ADR.
>
> Everything else here stands.

Phase 12's title ("Retained Scene & the One-Pass Layout Engine") and its build-steps.md text scope
`renderer/src/layout/mod.rs` against `oblisk-layout-engine-geometry.md` § 3-5 and `CONTEXT.md`'s
retained-scene entries: the constraint/size/position passes, the retained-scene transaction, the
lease mechanism, and overlay input-region bounding boxes. Matching that scope, this phase does not
build:

1. **`list` and `textfield` support.** Both are rejected as `LayoutError::UnsupportedNodeKind`
   wherever they appear, top-level or nested inside `children`. `list`'s exclusion matches
   build-steps.md's own explicit deferral text for this phase ("Deliberately deferred: list's
   virtual-repeater fast-reconciliation..."). `textfield` waits on ADR-0015's still-unbuilt PAM/
   IPC work -- there's no `wp-text-input-v3` handshake for it to resolve geometry against yet
   either way.

2. **`Signal`-valued geometry properties.** `node::reject_signal` turns any `mlua::Value::UserData`
   landing in a geometry-affecting property (`width`, `visible`, `content`, etc.) into
   `LayoutError::UnsupportedSignalProperty` instead of resolving it. § 5.1 types `visible` and a
   few other properties as `boolean / Signal`; auto-resolving a `Signal` here would mean
   re-evaluating layout on every pass with no cache-invalidation story, which is the Watcher's
   territory (`CONTEXT.md`, Watcher; Phase 13), not this module's.

3. **A confirmed `Percent` syntax.** `node::parse_size_mode` accepts a `"NN%"` string
   (`^\d+(\.\d+)?%$`) as `SizeMode::Percent`. This is this phase's own invented on-the-wire
   convention, not a spec answer -- § 5.1's base property table documents only integer/`"Fill"`
   for `width`/`height`, even though § 3.1 names `Percent(f32)` as one of the four size classes
   without giving it a literal Lua form. A future `shell.lua` author-facing decision could pick a
   different syntax (a `{ percent = 0.5 }` table, for instance) without changing anything below
   `parse_size_mode`.

4. **A real arrangement formula for `rect`(with children)/`button`/`surface`(with `child`).** § 3.2
   gives `row`/`column` explicit intrinsic-size formulas and gives every other container nothing.
   `intrinsic_content_size`/`position_children` treat these three as a stacking/overlay model:
   every child is resolved against the *full* content box independently, positioned per its own
   `align_h`/`align_v` with no spare-space distribution across siblings, and the container's own
   `Content` size (when applicable) is the bounding union of its children. Children can overlap.
   This is this phase's own interpretation, chosen because it's well-defined and covers the most
   common real shape (`rect { background = ..., children = { text { ... } } }`) without inventing
   a second axis-distribution concept the spec never asked for.

5. **A live `wl_region`/`wl_surface::set_input_region` push.** `overlay_input_regions` computes the
   bounding-box list correctly and is tested, but nothing calls it in production. Those Wayland
   objects live on `wayland::mod`'s main thread; `Scene` lives on `socket.rs`'s dedicated
   socket-client thread (the same split Phase 9/11 established for the connection and the `Loader`).
   Bridging the two threads is undesigned work, most naturally landing alongside Phase 14's
   renderer-presentation-feedback wiring, which already has to solve a related main-thread/
   socket-thread handoff.

6. **Real per-output pixel dimensions.** `socket.rs`'s `Scene::apply` call resolves against
   `PLACEHOLDER_OUTPUT_SIZE`, a hardcoded `LogicalSize` constant, for the same thread-split reason
   as item 5 -- real output geometry lives in `wayland::mod`'s `App`, not on the socket-client
   thread. Matches `PROOF_OF_WIRING_SHELL`'s own hardcoded-stand-in precedent from Phase 11.

7. **A real GPU resource behind the lease.** `Scene::release`/`retiring_ids` and the child-first
   teardown ordering are real and tested, but nothing in this codebase owns a per-node
   texture/VBO yet, so there's nothing for a lease to actually guard. Nothing calls `release` in
   production -- build-steps.md's own Phase 12 text says to build the mechanism ahead of its
   consumer ("Build the mechanism now even though its only real consumer lands later"), matching
   `supervisor/src/socket.rs`'s `GenerationRegistry::send_to` precedent from Phase 9.

8. **A single shared `ShapingHandle`.** `socket.rs` now spawns its own `ShapingHandle` (a second
   dedicated worker thread and `FontSystem`) alongside `wayland::mod`'s pre-existing one, instead
   of one instance threaded through `main.rs` and shared by both. Same thread-split reason as
   items 5/6 -- each is independently correct, just duplicates `FontSystem::new()`'s roughly
   one-second startup cost. A later simplification, not a bug, and not worth building until
   startup latency is a measured problem.

9. **Anything downstream of a resolved `ResolvedNode` tree.** Actual GPU/FemtoVG drawing of
   rects/text/icons per resolved geometry is untouched. This phase is the layout/reconciliation
   module, not the paint pipeline.

10. **A full two-pass constraint solver for `row`/`column` children.** `row`/`column`'s children
    resolve against their parent's own budget in one pass, not two: when the row/column's own
    size in an axis is `Content`, children get a `0.0` budget in that axis (a `Fill`/`Percent`
    child resolves to `0.0`), because the row/column's own final size in that axis isn't known
    until after its children resolve. Flagged with a `ponytail:` comment at
    `renderer/src/layout/scene.rs`'s `resolve_and_reconcile` where the budget is computed. Correct
    for every fixture this phase needs; wrong only for a child that specifically wants to fill a
    `Content`-sized ancestor in that same axis, which nothing here exercises.

11. **A `Stretch` child of a `Content`-sized parent doesn't reposition its own descendants.**
    `stretch_forced_size` pre-computes a `Stretch` child's final size *before* recursing into it
    only when the parent's own size in that axis is already known (`Fill`/`Percent`/`Pixels`) --
    in that case the child's own children get positioned against the correct final size in the
    same recursive pass (this is what fixed the shipped correctness bug: a `Stretch` child that
    itself had children previously kept its pre-stretch layout for everything below it). When the
    parent's axis is `Content`-sized, its own final size isn't known until *after* this child
    resolves, so pre-forcing is impossible -- `position_children` still patches that child's own
    `rect` afterward (matching the pre-fix behavior for that one remaining case), but that child's
    *descendants* stay positioned against its pre-patch size, same root cause as item 10. No
    fixture here exercises a multi-child `Stretch` node nested inside a `Content`-sized row/column.

12. **No recursion-depth limit.** `resolve_and_reconcile`, `retire_child_first`, and
    `RetainedNode::to_resolved` all recurse one stack frame per tree level with no cap. `shell.lua`
    is trusted local configuration, not adversarial input, so this isn't hardened against today;
    a pathologically deep `children`/`child` nesting could still overflow the socket-client
    thread's stack. Flag rather than solve speculatively, same as build-steps.md's own instruction
    for `list`'s reconciliation (item 1) -- a depth cap is cheap to add later if a real script ever
    needs one.

Decision: `renderer/src/layout/` ships two pieces.

- **`node.rs`**: typed, validated property parsing (`SizeMode`, `EdgeInsets`, `Align`,
  `LayoutError`) reading a `VirtualNode`'s raw `HashMap<String, mlua::Value>` -- the "actual
  consumer that needs typed, validated properties" `renderer/src/lua/nodes.rs`'s own doc comment
  named as Phase 12's job when it deferred this in Phase 10 (ADR-0021 item 5). An explicit pixel
  `width`/`height` is range-checked against § 5.1's `[0, 8192]`, delivering the validation
  ADR-0021 item 5 named this phase as the consumer for.

- **`scene.rs`**: `resolve_and_reconcile`, one recursive function doing all three of § 3's passes
  (constraint down, size up, position down) per node in a single recursion rather than three
  separate tree walks, matching the spec's literal "single... pass" language. It reconciles as it
  walks: fresh nodes are matched to retained ones purely by index within each parent's children
  list (§ 4, "positional indices" -- the same rule `CONTEXT.md`'s Retained-scene transaction entry
  names generally, not just for `list`), reusing a `NodeId` when the kind at that position matches
  and building fresh identity when it doesn't. Removed subtrees are torn down child-first into a
  `retiring` bag (`CONTEXT.md`, Lease), not dropped immediately. Top-level surfaces are the one
  exception to positional matching: they're keyed by their own `id` property (§ 6.1) in a
  `HashMap<String, RetainedNode>`, since surfaces are identified, not ordered -- `PROOF_OF_WIRING_SHELL`
  already returns a single surface keyed `"bar"`.

  `renderer/src/socket.rs`'s `run()` gets a `Scene` and a `ShapingHandle` alongside its existing
  `Loader`/live-signal setup; the `receive_loop` callback's new `apply_to_scene` helper feeds every
  evaluated `LoadOutput` into `Scene::apply` and logs the resolved root's geometry -- `layout`'s
  first production caller, the downstream consumer of `LoadOutput` that Phase 11's ADR-0022 item 6
  named as this phase's job.

  An invisible node (`visible == false`) still resolves normally -- geometry, children, everything
  -- but is treated as contributing nothing when it's a *child* being summed/distributed by a
  row/column or scanned for overlay input regions. § 5.1 only specifies that `visible` gates a
  future paint pass and the input-region scan; whether a hidden node still reserves row/column
  space is unspecified. The boring, least-surprising choice -- a hidden node collapses rather than
  reserving space -- is what's implemented, same convention most retained-mode UI toolkits use.

  § 3.1's `margin` is fully applied, not just parsed: a child's own margin is subtracted from its
  available budget before it resolves, widens its footprint in row/column intrinsic-size and
  spare-space math (so a margined child pushes its neighbor apart instead of overlapping it), and
  offsets its final position inward. § 3.2's text wrapping is also real: `text/shaping.rs`'s
  `ShapeRequest` gained a `max_width` field, and a `text` node's wrap boundary is its own resolved
  width when known, or the room its parent handed down when it's `Content`-sized.

  A `Stretch` cross-alignment (§ 3.3) is resolved *before* recursing into the stretched child, not
  patched onto its returned `rect` afterward, whenever the parent's own size in that axis is
  already known -- the pre-patch approach (still used as a fallback for a `Content`-sized parent,
  item 11) left a stretched child's own descendants positioned against its pre-stretch size, a real
  bug caught in this phase's Correctness review before landing.

Tested against (TDD, one seam per cycle): every `SizeMode`/`Align`/`EdgeInsets` parse path
including the invented percent syntax, the `[0, 8192]` range check (both the upper bound and a
negative value), and the `Signal`-rejection case (`node.rs`); `Pixels`/`Fill`/`Percent`/`Content`
resolution against a parent's available bounds; `row`/`column` intrinsic-size formulas against a
hand-computed example independent of the resolve code; a real `ShapingHandle` round-trip for text
content-sizing and for wrapping (a narrower `max_width` measures more lines and no wider than
unconstrained); the stacking model's independent-alignment and overlap behavior; an invisible
child not consuming row space or a spacing gap; a margined child offset inward and widening its
row's footprint; a margined child pushing its sibling apart instead of overlapping it; a `Stretch`
child that itself has children repositioning its own descendants against the post-stretch size
(the regression test for the fixed Correctness finding); `Scene::apply` reconciliation (same-index
same-kind reuse, kind-change replace-and-retire, a shrinking child list retiring its tail,
child-first teardown order asserted via `retiring_ids()`, `release` removing exactly one entry and
returning `false` on an unknown or already-released id); `list`/`textfield` rejected both at the
top level and nested inside `children`; `overlay_input_regions` including only visible direct
children and matching `snap_to_physical`'s already-tested projection; the `socket.rs` wiring,
reusing the existing `sample_snapshot`/`receive_loop` test seam to confirm a received
`StateSnapshot` ends up resolvable through `Scene::surface` after `apply`.

Not automated: nothing in this phase touches real hardware or a running compositor -- unlike
Phase 6's PipeWire tests or Phase 3's EGL research, there's no external system to fake evidence
from here. Every seam above is exercised with hand-built `VirtualNode`/Lua fixtures, matching this
codebase's established fixture style (`renderer/src/lua/nodes.rs`, `renderer/src/text/snap.rs`).

Upgrade path, in order: (a) Phase 13's Watcher gives `shell.lua` a real file and a reload trigger,
and is the natural place to resolve a `Signal`-valued geometry property (item 2) once there's a
cache-invalidation story to hang it on; (b) Phase 14's renderer-presentation-feedback wiring is the
likely place the Wayland-thread bridging for real output dimensions (item 6) and the live
`wl_region` push (item 5) lands, since it already needs a main-thread/socket-thread handoff; (c)
`list`/`textfield` (item 1) grow real support once their respective blockers clear -- `list`'s
virtual-repeater semantics are this phase's own explicitly-deferred item, `textfield`'s waits on
ADR-0015; (d) a real GPU resource type gives the lease mechanism (item 7) its first actual caller,
likely the wallpaper crossfade named in `CONTEXT.md`'s own Lease entry (Phase 16); (e) the paint
pipeline (item 9) consumes a resolved `ResolvedNode` tree once one exists; (f) if Content-sized-row
Fill/Percent children (item 10) turn out to matter, a second constraint pass gets added to
`resolve_and_reconcile` rather than reworking its single-recursion shape; (g) item 11 (a `Stretch`
child of a `Content`-sized parent not repositioning its own descendants) shares that same second-pass
upgrade path, since it's the identical chicken-and-egg cause; (h) a recursion-depth cap (item 12)
is a small, self-contained addition to `resolve_and_reconcile`/`retire_child_first` whenever a real
script's nesting depth makes it worth adding.

This does not contradict `docs/oblisk-idl-api-specs.md` § 5, `docs/oblisk-layout-engine-geometry.md`
§ 3-5, `CONTEXT.md`'s retained-scene entries, or build-steps.md's Phase 12 text: all describe or
assume the target shape once every node kind, a resolved Signal-property story, and a live Wayland
bridge exist, and none of that is built here. The stacking-model interpretation (item 4) and the
percent syntax (item 3) fill gaps the spec leaves open rather than contradicting anything it states.
