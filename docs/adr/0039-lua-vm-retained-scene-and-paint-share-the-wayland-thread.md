# The Lua VM, the retained scene, and the paint pass share the Wayland dispatch thread

> Decision 4 is deferred to Phase 20, and its stated reason was wrong. "Layout resolves against
> each surface's real configured size, which is now in scope" treats the thread boundary as the
> only blocker. Consolidation makes the sizes reachable but not attributable: `Scene` keys surfaces
> by the `id` a config writes, `wayland::mod` derives `TrackedSurface::surface_id` from
> `SurfaceRole::label()`, and those two id spaces do not intersect. Deleting `SurfaceRole` is what
> makes them one, so the per-surface size lands with ADR-0038 in Phase 20 item 4. Decisions 1, 2, 3
> and 5 are unaffected; 1 through 3 shipped with the refactor.

The 2026-08-28 renderer review found the Renderer split into two OS threads that cannot reach each
other's state, with the scene on one side and the pixels on the other.

`renderer/src/main.rs` spawns `socket::spawn_client` on a dedicated thread with a current-thread
tokio runtime, then runs `wayland::run`'s blocking dispatch loop on the main thread. Four one-way
`std::sync::mpsc` channels connect them, and every one of them carries PBA handshake traffic:
`ready_tx`, `presented_tx`, `activate_tx`, `secure_submit_tx`. None carries scene data.

The ownership split that follows:

| Socket thread | Wayland thread |
| :--- | :--- |
| `mlua::Lua`, `Loader`, live signals, rescue state | `wl_surface`s, `LayerSurface`s, EGL context |
| `layout::Scene`, the retained node tree | `TextPainter` (FemtoVG `Canvas<OpenGl>`) |
| `ShapingHandle` (a second `FontSystem`) | `ShapingHandle` (the first one) |
| Real output geometry: none, `PLACEHOLDER_OUTPUT_SIZE` | Real output geometry, in `OutputState` |
| `wp-text-input-v3` state: none | `ZwpTextInputV3`, `SecureBuffer` |

Three of ADR-0023's recorded scope ceilings are the same defect seen from three angles, and that
ADR names the thread split as the cause for each. Item 5: `overlay_input_regions` is computed and
tested but never pushed, because `wl_region` lives on the Wayland thread and `Scene` does not.
Item 6: layout resolves against a hardcoded `PLACEHOLDER_OUTPUT_SIZE` constant, because real output
dimensions live in `App`. Item 8: two `ShapingHandle`s exist, each paying `FontSystem::new()`'s
roughly one-second startup, because neither thread can borrow the other's.

Item 9 is the consequence: nothing downstream of a resolved `ResolvedNode` tree exists. The only
thing `wayland/mod.rs` draws is a hardcoded proof string on `main_bar`, labelled in its own comment
as a Phase 4 integration proof with "no draw loop or Lua-driven content".

## Why this is not a paint-pass problem

FemtoVG's `Canvas<OpenGl>` must be used on the thread holding the current GL context, which is the
Wayland thread. `Scene`'s nodes hold `mlua::Value` properties, so `Scene` is `!Send` and must sit
with the Lua VM. Painting the retained scene therefore requires the scene and the VM on the Wayland
thread. This is a constraint, not a preference: no ordering of paint work resolves it.

The same constraint reappears in every unbuilt feature. ADR-0038's Lua-declared surfaces need the
evaluated topology to create Wayland objects. `button`'s `on_click` (stored since ADR-0021 item 2,
never invoked) needs pointer events to reach Lua closures. Per-surface `keyboard_interactivity`
needs the same crossing. Each one is blocked on the same seam.

## Decision

**The Lua VM, the `Loader`, the retained `Scene`, and the paint pass all live on the Wayland
dispatch thread.** The socket thread is demoted to framed I/O: it reads `SupervisorFrame`s and
forwards them over a channel, and writes outbound `CommandEnvelope`s and secure-submit frames it
receives over another. It keeps its tokio current-thread runtime, which is the right home for async
socket I/O and the wrong home for a Lua VM.

Concretely:

1. `Loader`, the live-signal map, the rescue state, and `Scene` are constructed inside
   `wayland::run` rather than `socket::spawn_client`. `mlua::Lua` is `!Send`, so it must be built on
   the thread that runs it; this is a move, not a hand-off.
2. The four PBA channels collapse to two. `ready_tx`/`presented_tx`/`activate_tx` become direct
   calls, because the code that evaluates and the code that commits buffers are now the same loop.
   `secure_submit_tx` becomes an outbound-frame send, in the same direction as every other write.
3. One `ShapingHandle` survives, shared by content-sizing and painting. Shaping stays off-thread;
   that worker is a real bounded cost, unlike the second `FontSystem`.
4. `PLACEHOLDER_OUTPUT_SIZE` is deleted. Layout resolves against each surface's real configured
   size, which is now in scope.
5. `overlay_input_regions` gets its production caller, per ADR-0038 decision 5.

## Consequences

PBA gets simpler rather than harder. `oblisk-supervisor-services-dbus.md` § 15.2 specifies the
Candidate as: evaluate `shell.lua`, then bind layer-shell surfaces, then commit null buffers, then
signal ready. On one thread that is four sequential statements. Across two threads it is the
channel dance currently spread over `ready_signal_sent`, `null_buffered`, and `maybe_send_ready_signal`.

The cost is real and worth stating plainly: a slow `shell.lua` evaluation now blocks Wayland
dispatch, so a pathological config stalls frames instead of stalling only its own thread. Three
things bound it. ADR-0021's 5ms CPU cap already aborts a runaway `computed` closure through
`Lua::set_hook`, and it is enforced, not merely measured. Text shaping, the one genuinely expensive
operation in an evaluation, is already off-thread behind `ShapingHandle`. Full re-evaluation happens
on config edit, which is rare, and on nothing else: ADR-0029 made a `StateSnapshot` push hydrate its
signal without triggering evaluation.

Both reference toolkits accept the same trade. Quickshell runs the QML engine on Qt's GUI thread,
the thread that owns the scenegraph. Noctalia v5's stated reason for dropping Qt was to "own the
whole stack, the event loop, rendering, input handling, all of it", which is one thread's worth of
ownership. Neither treats config evaluation as something to isolate from rendering.

## Rejected: ship resolved trees over a channel, keep the split

`Scene` stays on the socket thread; each `apply` serializes a `Send` snapshot of the resolved tree
and sends it to the Wayland thread to paint. This preserves the current shape and needs no move.

Rejected because it pays the crossing forever, on every feature, in both directions. A pointer click
becomes Wayland thread to socket thread to Lua closure to re-evaluation to snapshot to Wayland
thread: two channel hops and a frame of latency on every interaction, for a callback that is a
function call away once the threads are one. ADR-0038's surface creation becomes a request/response
protocol (declare, request, create, await configure, return size, re-resolve, return tree) instead
of a method call. Real output geometry and input regions each need their own crossing. The
serialization itself is not free either: the snapshot must drop `mlua::Value` properties or resolve
them, which means either a second node representation to maintain or resolving Signals at snapshot
time, exactly the cache-invalidation problem ADR-0023 item 2 deferred.

Consolidation deletes two channels, one `FontSystem`, one placeholder constant, and one entire class
of future plumbing. The alternative adds a permanent tax to buy back a thread boundary that is not
protecting anything: the socket thread does not currently isolate a failure, since a Lua evaluation
failure is already caught and routed to rescue in-process.

## Rejected: move EGL and paint onto the socket thread instead

Symmetric on paper, worse in practice. Wayland dispatch and EGL surface lifetime are coupled through
`WlEglSurface`, which wraps a `wl_surface` whose configure events arrive on the dispatch queue.
Splitting dispatch from paint means the `configure` handler and the code that resizes the EGL
surface sit on opposite sides of a channel, which is the defect this ADR removes, relocated rather
than fixed.

## Scope

This ADR settles where the state lives. It does not build the paint pass, the surface manager, or
input dispatch, all of which it unblocks and none of which it specifies. Those are separate slices,
sequenced in `build-steps.md` Phases 18 through 21. Landing this ADR's refactor should change no
observable behavior: the same hardcoded surfaces, the same proof string, the same PBA handshake,
with fewer threads under them.
