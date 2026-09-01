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
A config edit that adds or removes a top-level `surface` node, or changes its layer, anchor, monitor target, or namespace. Triggers a generation swap. Namespace and monitor are fixed when the compositor creates a layer surface and cannot be changed on a live one, so they belong here by protocol. Layer and anchor are listed here by choice, not necessity: the protocol can change both in place, and narrowing the swap set to only what genuinely requires recreation is an open question, not a settled one.
_Avoid_: structural change, breaking change

**Value change**:
A config edit that is not a topology change. Triggers an in-place reload of the same generation.
_Avoid_: minor change, hot patch

**Generation swap**:
The full candidate-spawn, presentation-evidence, promote-and-reap flow. Reserved for topology changes.
_Avoid_: hot-reload (ambiguous: covers both swap and in-place reload)

**Handoff window**:
The part of a generation swap where two renderer processes are alive at once: from the candidate's spawn until the superseded generation is reaped. Bounded by PBA's own timeouts, so it lasts seconds and only exists during a swap. Named because it is the one state the memory budget is measured against separately (ADR-0043 decision 1 item 3), never folded into a steady-state number.
_Avoid_: overlap, transition period

**In-place reload**:
Re-running the config on the current generation's existing Lua VM, without spawning a candidate or rebinding Wayland/EGL. Used for value changes. The VM is not reset: the retained scene holds `mlua` values that keep it alive, so dropping it would leak the whole VM per reload and leave the scene reading a state no config runs in (ADR-0044). One VM per generation, dropped only when the generation ends.
_Avoid_: hot-reload, live patch, VM reset (a generation swap's job, and a swap gets a new process)

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

**Rescue process**:
A Supervisor-spawned process that draws an evaluation error on one `Overlay` layer surface per output. Holds no Lua VM, no config, and no capability connections, so it works precisely when the config does not. Not a generation: no generation id, no dependency snapshots, no PBA handshake, no authority. Reaped as soon as a real generation reaches presentation evidence. Distinct from the `oblisk.rescue` signal, which covers the opposite case, a reload failure where a working config is still on screen to render its own error (ADR-0046).
_Avoid_: rescue mode (the `oblisk.rescue` signal's state), fallback config (a rejected alternative), rescue generation

## Surfaces

**Surface**:
A top-level thing `shell.lua` declares, mapped to one Wayland surface per output it targets. The umbrella term covering all four roles, not any one of them. The engine owns no surface the config did not declare. A generation builds its whole surface set from its own evaluation at startup and does not add or remove surfaces afterwards; changing the set is a topology change. `main_bar`, `overlay_canvas`, and `wallpaper_layer` are ids the default config happens to use, not engine-defined roles (ADR-0038).
_Avoid_: window (one of the four roles, not the umbrella), panel (likewise), layer (protocol term for a panel's stacking level)

**Surface role**:
Which Wayland protocol gives a surface its behavior, and the Lua constructor that declares it: `panel` (layer surface), `window` (toplevel), `popup` (popup), `lock` (lock surface). A surface has exactly one role, fixed when it is declared, and the role decides which properties are meaningful. Mirrors Wayland's own use of "role" for what turns an inert `wl_surface` into something the compositor shows (ADR-0040).
_Avoid_: surface type, window kind, surface class

**Lock client**:
The process holding `ext_session_lock_v1`, which is the Renderer. Only one may exist at a time, so no generation swap can happen while locked, and the process holding the lock is necessarily the one painting the lock screen: lock surfaces cannot cross a process boundary. If it dies, the compositor is required to keep the session locked (ADR-0042).
_Avoid_: lock authority (the superseded ADR-0010 term for a Supervisor-held handle), locker, lock screen (the Lua-authored UI, not the client)

**Surface instance**:
One `(surface, output)` pair, addressed as `"{id}@{output}"`. A surface targeting every monitor has one instance per monitor, each with its own configured size. This is what the compositor actually maps, and what a presentation-evidence report names. Monitor hotplug adds and removes instances within a live generation: the declared surfaces are unchanged, so it is not a topology change and triggers no swap.
_Avoid_: surface copy, per-monitor surface

**Wallpaper surface**:
A `panel` on the `Background` layer, non-exclusive, one instance per monitor. Distinct from the UI surfaces because `Overlay` sits above every application window by protocol definition, so wallpaper drawn there would cover the desktop rather than sit behind it (ADR-0007). It holds an `image` node like any other surface holds its children; there is no wallpaper-specific engine code and no `wallpaper` capability (ADR-0055).
_Avoid_: background layer (protocol term, not the Oblisk surface), wallpaper capability (there is none)

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

**Resolved style**:
One node's geometry properties, parsed into typed values exactly once per layout pass and read by every pass that follows. A node's parent produces it in the same loop iteration that resolves the node's signals, because the parent needs the child's margin before it can hand it a budget; a surface root's is produced by the transaction itself. It exists because a resolved property is still a Lua value, and a table carrying an `__index` answers each read separately, so parsing the same property in the sizing pass and again in the positioning pass let the two disagree about one node's margin.
_Avoid_: style (ambiguous: also the paint properties), computed style, layout cache

**Layout pass budget**:
The single CPU deadline covering one whole retained-scene transaction, as opposed to the per-evaluation cap that bounds one signal getter. It is what puts a resolved table's `__index` metamethod under a limit at all, since that runs between signal evaluations rather than inside one, and it is what stops a tree of individually-legal getters adding up to an unbounded pass. Exceeding it fails the transaction and rolls it back, like any other layout error.
_Avoid_: frame budget (a different thing: how long a repaint may take), CPU cap (the per-evaluation one)

**Paint pass**:
The walk over one surface instance's resolved geometry that emits its draw calls and swaps its buffer. Runs per surface instance, never across them, and reads the retained scene without changing it. Distinct from the layout passes, which decide geometry; the paint pass only consumes what they resolved.
_Avoid_: render pass (ambiguous: also a GPU term), draw loop, frame

**Image cache**:
The Renderer's map from a resolved file path, an integer pixel size, and the file's own revision to one uploaded GPU texture. The size is in the key because an SVG rasterized for a 12px box is a different texture from the same file rasterized for a 24px box; the revision is, because the tray overwrites one spool path in place on every icon update (ADR-0031) and a path-only key would serve an app's first icon forever. Belongs to one generation and is cold again after every swap.
_Avoid_: texture atlas (femtovg's private glyph store, ADR-0012), asset cache

**Icon resolver**:
The Renderer-side lookup turning a theme name into an absolute file path, through the installed XDG icon themes. A name that is already an absolute path is used as itself, the same rule a `.desktop` file's `Icon=` key follows. Lives in the Renderer rather than the Supervisor because the control socket has no request/response shape to resolve one over (ADR-0054).
_Avoid_: find_icon (the unbuilt Lua-facing half), icon theme engine

**Node identity**:
What makes a freshly evaluated node the same node as the one already in the retained scene, so its lease and its named state follow it. An optional `id`, unique among its siblings and scoped to its parent rather than to the tree, decides it; unidentified siblings still fall back to matching by position among themselves. A `list`'s items use their `key` instead (ADR-0045).
_Avoid_: node id (the property, not the concept), key (a `list`'s spelling of this, not the general term), handle

**Named state**:
Config-authored reactive state, created by `state(name, initial)` and writable from Lua, unlike every other signal. The name is what survives: a generation keeps one map that outlives any single evaluation, so re-running the config on an in-place reload finds the same signal holding the same value and an open dropdown stays open. What survives is a name whose `initial` the config author left alone. Editing that literal re-seeds the signal, because the edit is a later write than the `:set()` it lands on. Dies on a generation swap, since the map lives in the process being reaped (ADR-0044 decision 5 and its amendment).
_Avoid_: persistent state (implies it survives a swap or a restart, which it does not), local state, property

**Signal resolution**:
Reading a `Signal` handle's current value at layout time, where a config put the handle itself into a node property rather than the result of `:get()`. The two spellings differ in lifetime, not in type: a handle stays live and re-reads on every resolve, a `:get()` result is a value frozen at evaluation time. Only a handle makes a property reactive (ADR-0044).
_Avoid_: binding, unwrapping, dereferencing

**Dirty scene**:
The flag a live signal's write sets, meaning the retained scene must re-resolve before the next paint. Re-resolving re-runs layout against the last evaluation's node tree and reads current signal values through it; it does not re-run `shell.lua`. One flag covers the whole scene, so any write re-resolves every surface (ADR-0044).
_Avoid_: damage (a paint-region mechanism, not this), invalidation (implies a dependency graph, which there isn't), stale scene

**Frame gating**:
The rule that a surface instance repaints only when the compositor has returned its frame callback and the retained scene has actually changed since the last paint. Keeps an idle shell at zero redraws rather than repainting on a timer.
_Avoid_: vsync, throttling, damage (a different mechanism: which region changed, not whether to paint)

## Ownership

**Lock authority**:
The decision to lock and the supervision of whoever holds the lock, both the Supervisor's. Not the protocol handle: the Renderer holds `ext_session_lock_v1` and paints the lock screen, because the process that holds the lock is the only one that can create a surface for it (ADR-0042). Only a successful PAM authentication ends a lock, and only the Supervisor can order that.
_Avoid_: lock screen (ambiguous: covers both the authority and the surfaces), lock client (the Renderer, which is a different owner)

**Lock surface**:
One `ext_session_lock_surface_v1` per output, existing only between the compositor granting the lock and the unlock. A `lock` declaration is returned at the root of `shell.lua` like any other role and costs one retained node per output and no Wayland object until the session locks (ADR-0052).
_Avoid_: lock screen widget (the pre-ADR-0042 term for a layer-shell surface, which cannot be part of a lock screen at all)

## Capabilities

**Capability**:
A named IPC-addressable module owning one slice of state and its write actions (e.g. `audio`, `network`).
_Avoid_: module, service, backend

**Revision**:
A capability's state-version counter, carried on every snapshot it pushes and stamped onto every write issued against what that snapshot said.
_Avoid_: version, sequence number

**Capability roster**:
`shared::Capability`, the enum naming every snapshot-hydrated capability. Each rostered name is reachable from a generation's first evaluation and reads `nil` until its first dependency snapshot arrives (ADR-0037). One name serves as the Lua name, the roster key and the `capability` field of every command written through it. The enum, its `ALL` and its `as_str` all come from one `roster!` macro list, so a variant cannot exist in one and be missing from another. Adding a line to that list fails the build at the three places that decide whether a capability starts, how its commands dispatch, and what channel it answers on, and the last of those then walks you through the `Signal` variant and the push handler it needs (ADR-0076). `idle` and `polkit` are deliberately off it and covered by the Supervisor's `Startable`.
_Avoid_: pre-seed list, known capabilities, `CAPABILITIES` (the `&[&str]` this replaced)

**Capability registry**:
`supervisor/src/capabilities`: `Capabilities`, holding every lazily-started controller and every signal sender, and `Signals`, the receiving half the main loop awaits. `start`/`push`/`dispatch` are the three things a capability does, one exhaustive match each, and a fourth exhaustive match proves every roster entry has a channel to push on or a stated reason for having none. `Signals::next` is deliberately the only awaiting half -- bare `recv()`s, so a lost `tokio::select!` race drops nothing -- and `Capabilities::push` runs in the winning arm's body, which `select!` never cancels; the two capabilities that `await` while building state depend on that split (ADR-0076). Not a trait and not boxed objects: ADR-0037 decision 3's "static calls, no registry, no trait" is unchanged, the calls just have a struct to hang off. The module also holds one child module per roster entry, flat, because the roster is flat and is the public interface.
_Avoid_: capability manager, service registry, plugin table

**Supervisor state**:
`supervisor/src/supervisor.rs`: `Supervisor`, everything the Supervisor loop carries from one iteration to the next -- the authoritative generation, the capability registry, the lock controller, the revision and snapshot maps, the reload sequence, the restart brake, and the three flags that decide whether a lock is owed, a re-acquisition is in flight, or a swap is waiting on an unlock. `run_supervisor`'s `select!` in `main.rs` says which events exist and in what order; the methods here say what each one does to that state. The split with `main.rs` is what survives an iteration: the channel receivers the loop awaits are locals, each read by one arm. Not a borrow-checker split -- `tokio::select!` drops the branch futures before running the winning handler, which is what lets the departure arm `await` the authoritative child through the same `&mut` its handler mutates. Cannot be built in a unit test, because the capability registry it holds needs a live system bus; the functions that are unit-testable stay free functions in `main.rs` and the methods forward to them.
_Avoid_: session (that is the logind session, and `SetSessionLock` means it), context, app state, world

**Capability start**:
The first read of `oblisk.<name>` in a generation, which is what makes the Supervisor build that capability's controller (ADR-0070). A name no config reads has nothing running behind it. One-way for the life of the Supervisor process: nothing stops a started capability.
_Avoid_: activation, subscription, enabling a capability

**Oblisk namespace**:
The single Lua table every capability, `rescue`, `screens` and `version` hang off. It is what keeps engine vocabulary (the node constructors a config calls) and system state (the things a config reads and commands) from sharing a name.
_Avoid_: globals, the state tree

**Secure submit**:
A `textfield` property naming the capability/action that receives a masked field's native input buffer directly, bypassing Lua. Without it, a masked field's value is unreadable from Lua entirely.
_Avoid_: secure handle, password callback

**Idle threshold**:
A Lua-registered duration, in seconds, that fires a matched `idled`/`resumed` event pair through the `idle` capability. The Supervisor holds one `ext_idle_notification_v1` listener per distinct duration on its own Wayland connection (Lock authority's sibling), fanning that listener's events out to every registration sharing the duration.
_Avoid_: idle timeout, inactivity timer

**Idle inhibit**:
A generation-scoped refcount that, above zero, holds one `org.freedesktop.login1.Manager.Inhibit(what="idle")` file descriptor open, blocking the system's own auto-suspend. The same per-generation reset that clears idle thresholds on reload or crash zeros this count too; the fd closes the instant the count returns to zero.
_Avoid_: wake lock, keep-awake handle

**Notification urgency**:
The `low`/`normal`/`critical` tier carried by every fed notification, read from the sender's `urgency` hint. Critical bypasses both do-not-disturb and the sender's `expire_timeout`, staying until dismissed instead of expiring or being silenced.
_Avoid_: priority, severity

**Do-not-disturb**:
A Supervisor-held global toggle (not per-generation) that gates notification sound playback only; `notifications.feed` keeps receiving every notification regardless of its state. Critical-urgency notifications bypass it. Lives in Supervisor memory and resets on Supervisor restart until the XDG atomic state manager exists to persist it.
_Avoid_: focus mode, silent mode, mute

**Notification body span**:
One allowlisted markup run inside a sanitized notification body: a text run carrying `{text, bold, italic, underline, href}`, or an image run carrying `{image_path}` resolved through the path-trust validator shared with action-icon names (an allowlisted-directory check, not full XDG theme resolution). The sanitizer emits an array of these instead of collapsing markup to flat text; every construct outside the five allowlisted tags (`<b>`, `<i>`, `<u>`, `<a href>`, `<img src>`) is rejected exactly as the prior flat-text sanitizer rejected all markup.
_Avoid_: rich text, HTML fragment

**Primary keyboard**:
The one input device Oblisk reads `oblisk.keyboard`'s per-key state from (backlight, lock LEDs, active layout), chosen once at Supervisor startup rather than tracked per-device. Matches the IDL's own singular `active_layout: string` declaration and every reference implementation checked.
_Avoid_: main keyboard, active keyboard

**Compositor link**:
The trait behind `keyboard.active_layout`/`keyboard:switch_layout`, one implementor per compositor (Hyprland, Niri). Scoped deliberately to what keyboard layout needs today. There is no workspace adaptor and this trait did not grow one: `oblisk.workspaces` has a single implementor and so has no trait at all (ADR-0056). Extracting a shared trait is what a second live-tested compositor is for, and whether it is one trait or two is still that commit's question.
_Avoid_: workspace adaptor (never built; ADR-0056 decided against it), compositor adapter

**Compositor probe**:
`supervisor/src/compositor.rs`: `CompositorKind` plus the `PROBES` table of env vars each compositor sets for every process in its own session, which is how the Supervisor picks an implementor at startup. Session-level, not a capability's property -- `keyboard` and `workspaces` both ask and neither owns the answer, which is why it no longer lives beside the compositor link (ADR-0075). Detection only: it hands back a kind, never an adaptor. `$XDG_CURRENT_DESKTOP` is not a probe, because it is set by whatever launched the session rather than by a compositor that is running; it is only used to name an unsupported session in a log line.
_Avoid_: compositor detection trait, session detector

**Track identity**:
A composite key (`mpris:trackid` + `xesam:url` + `xesam:title`) an `oblisk.mpris` player entry uses to detect whether its current track actually changed, since real players are observed to leave any one of `mpris:trackid`/`xesam:url`/`xesam:title` unchanged across a genuine track change (any single one changing counts as a change). Unchanged track identity across a resync means a missing/malformed `album_art_path`/`length` in that resync keeps its last known-good value instead of clearing; changed identity resets both before applying the new read (ADR-0036).
_Avoid_: track key, cache key
