# Oblisk shell

Shared language for renderer generations, reloads, retained scenes, and capability state.

## Reloads

**Generation**:
A renderer process and its Lua state with one generation ID.
_Avoid_: instance, worker

**Candidate**:
A generation being prepared but not yet authoritative.
_Avoid_: active generation, staged shell

**Authoritative generation** (per output):
The generation that receives input, reserves exclusive space, and owns capability routing for one output. Ownership transfers per output during a swap; a generation is fully authoritative once it owns every output it targets.
_Avoid_: active generation, current process

**Presentation evidence**:
Proof that a targeted output produced its first frame or presentation feedback after configuration.
_Avoid_: readiness, activation ACK

**Topology change**:
A config edit that adds, removes, or changes the layer, anchor, or monitor target of a top-level `surface` node. Triggers a generation swap.
_Avoid_: structural change, breaking change

**Value change**:
A config edit that is not a topology change. Triggers an in-place reload of the same generation.
_Avoid_: minor change, hot patch

**Generation swap**:
The full candidate-spawn, presentation-evidence, promote-and-reap flow. Reserved for topology changes.
_Avoid_: hot-reload (ambiguous: covers both swap and in-place reload)

**In-place reload**:
Resetting the Lua VM and re-running the config inside the current generation, without spawning a candidate or rebinding Wayland/EGL. Used for value changes.
_Avoid_: hot-reload, live patch

**Dependency snapshot**:
The Supervisor-owned system-state payload (`shared::StateSnapshot`: capability, revision, and JSON payload) pushed to a generation so its loader hydrates signals without live-querying NetworkManager, BlueZ, or PipeWire itself. The same snapshot type hydrates a candidate's first evaluation and an in-place reload's re-evaluation. One payload per capability, routed to that capability's own live Lua signal by name (ADR-0029).
_Avoid_: state snapshot, hydration payload

**Loader**:
The Lua evaluation of `shell.lua` into a node tree and surface topology, run inside a generation. The same loader logic runs both the authoritative generation's re-evaluation during an in-place reload and a candidate's first evaluation during a generation swap.
_Avoid_: config parser, AST compiler

**Watcher**:
The Supervisor-side `inotify` trigger that detects a config edit and dispatches the reload. It asks the authoritative generation's loader to re-evaluate, then either sends `reset_registrations` for an in-place reload or spawns a candidate for a generation swap, depending on whether the re-evaluation reports a topology change. Owns the swap-vs-in-place decision, not the reload's execution.
_Avoid_: file monitor, reload trigger

**Rollback**:
The rule that a reload never replaces a working generation's state with a failed one. A failed generation swap leaves the authoritative generation untouched (already built: `reload::run_pba` aborts the candidate on any pre-evidence failure). A failed in-place reload keeps the pre-reload retained scene applied and surfaces the failure through `oblisk.rescue` instead of applying a broken tree.
_Avoid_: revert, recovery

## Surfaces

**Wallpaper surface**:
The third static surface (`Background` layer, non-exclusive, one per monitor), distinct from `main_bar` and `overlay_canvas`. Owns wallpaper texture rendering.
_Avoid_: background layer (protocol term, not the Oblisk surface)

## Scene

**Retained scene**:
The persistent Rust-side node tree for one generation, kept alive across reload cycles instead of rebuilt from scratch. The loader's freshly-evaluated tree is reconciled into it, not swapped in wholesale.
_Avoid_: scene graph, node tree

**Retained-scene transaction**:
The batch operation that applies one loader evaluation to the retained scene: matches fresh nodes to existing ones by identity, writes the changes, and tears down removed subtrees child-first so a parent never frees a resource a child still holds.
_Avoid_: reload apply, tree diff

**Lease**:
A grace period that keeps a removed node's GPU resource alive past its removal from the retained scene, until whatever still needs it (a wallpaper crossfade, an in-flight transition) finishes consuming it. Child-first cleanup runs once the lease expires.
_Avoid_: keepalive, grace period

## Ownership

**Lock authority**:
The Supervisor-held `ext_session_lock_v1` handle and its minimal `wl_shm` fallback surface. Outlives the Renderer; distinct from the Lua-authored lock screen widget, which is presentation only and can die with the Renderer.
_Avoid_: lock screen (ambiguous: covers both the authority and the Lua widget)

## Capabilities

**Capability**:
A named IPC-addressable module owning one slice of state and its write actions (e.g. `audio`, `network`).
_Avoid_: module, service, backend

**Revision**:
A capability's state-version counter. A stale revision fails the write.
_Avoid_: version, sequence number

**Secure submit**:
A `textfield` property naming the capability/action that receives a masked field's native input buffer directly, bypassing Lua. Without it, a masked field's value is unreadable from Lua entirely.
_Avoid_: secure handle, password callback

**Idle threshold**:
A Lua-registered duration, in seconds, that fires a matched `idled`/`resumed` event pair through the `idle` capability. The Supervisor holds one `ext_idle_notification_v1` listener per distinct duration on its own Wayland connection (Lock authority's sibling), fanning that listener's events out to every registration sharing the duration.
_Avoid_: idle timeout, inactivity timer

**Idle inhibit**:
A generation-scoped refcount that, above zero, holds one `org.freedesktop.login1.Manager.Inhibit(what="idle")` file descriptor open, blocking the system's own auto-suspend. The same per-generation reset that clears idle thresholds on reload or crash zeros this count too; the fd closes the instant the count returns to zero.
_Avoid_: wake lock, keep-awake handle
