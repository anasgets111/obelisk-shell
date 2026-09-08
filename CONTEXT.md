# Oblisk shell

Current project vocabulary. Implementation contracts live in [docs](docs/oblisk-idl-api-specs.md); rationale and history live in [decisions](docs/decisions.md).

## Reloads

**Generation**: A Renderer process and its Lua state with one generation ID. _Avoid_: instance, worker

**Candidate**: A generation being prepared but not yet authoritative. _Avoid_: active generation, staged shell

**Authoritative generation**: The generation receiving input, reserving exclusive space and owning capability routing for an output. Authority transfers per output during a generation swap. _Avoid_: active generation, current process

**Presentation evidence**: Evidence accepted for a targeted surface instance before authority transfers. _Avoid_: readiness, activation ACK

**Topology change**: A config edit that changes the declared surface set or its topology fingerprint. It requires a generation swap. _Avoid_: structural change, breaking change

**Value change**: A config edit that leaves the surface topology unchanged and applies through an in-place reload. _Avoid_: minor change, hot patch

**Generation swap**: Replacement of a generation through candidate preparation, presentation evidence, authority transfer and retirement. _Avoid_: hot-reload

**Handoff window**: The interval in a generation swap when the candidate and the generation it replaces both exist. _Avoid_: overlap, transition period

**In-place reload**: Re-evaluation of the config in the same generation, preserving its Lua state and reconciling its retained scene. _Avoid_: hot-reload, live patch, VM reset

**Dependency snapshot**: A Supervisor-owned capability payload and revision supplied to a generation for signal hydration. _Avoid_: state snapshot, hydration payload

**Loader**: The evaluation of shell.lua into a node tree and declared surface topology. _Avoid_: config parser, AST compiler

**Watcher**: The Supervisor's config-edit observer that initiates reload evaluation. _Avoid_: file monitor, reload trigger

**Rollback**: Preservation of the working scene or generation when a reload fails. _Avoid_: revert, recovery

**Rescue process**: A separate process that displays an evaluation error when no working config can display it. _Avoid_: rescue mode, fallback config, rescue generation

## Surfaces

**Surface**: A top-level declaration in shell.lua with a fixed role and one or more surface instances. The declared set belongs to a generation. _Avoid_: window, panel, layer

**Surface role**: The behavior assigned to a surface: panel, window, popup or lock. _Avoid_: surface type, window kind, surface class

**Lock client**: The Renderer holding the session-lock protocol handle and painting its lock surfaces. _Avoid_: lock authority, locker, lock screen

**Surface instance**: One live mapping of a surface declaration. Panels and locks use `{id}@{output}`; windows and popups use their declared ID. _Avoid_: surface copy, per-monitor surface

**Wallpaper surface**: A config-declared Background panel displaying an image behind applications. _Avoid_: background layer, wallpaper capability

## Scene

**Retained scene**: A generation's persistent node tree, reconciled across evaluations. _Avoid_: scene graph, node tree

**Retained-scene transaction**: An atomic reconciliation and resolution of the retained scene, matching nodes by identity and removing unmatched subtrees. _Avoid_: reload apply, tree diff

**Resolved style**: A node's geometry properties after signal resolution and type validation for a layout pass. _Avoid_: style, computed style, layout cache

**Solver style**: The translation of a resolved style into the layout solver's sizing and positioning rules. _Avoid_: taffy style, flex style, constraint

**Layout pass budget**: The CPU allowance for a whole retained-scene transaction. Exceeding it fails the transaction. _Avoid_: frame budget, CPU cap

**Paint pass**: Drawing one surface instance from its resolved geometry without changing the retained scene. _Avoid_: render pass, draw loop, frame

**Image cache**: A generation's decoded/uploaded image reuse, distinguished by source revision and target size. _Avoid_: texture atlas, asset cache

**Icon resolver**: The Renderer lookup that turns an icon theme name or absolute path into an image source. _Avoid_: find_icon, icon theme engine

**Node identity**: The match between nodes across evaluations, scoped to their parent. Explicit sibling IDs or list keys take precedence over positional matching. _Avoid_: node id, key, handle

**Named state**: Lua-writable reactive state identified by a name within a generation. It survives in-place reloads with an unchanged seed, but not generation swaps. _Avoid_: persistent state, local state, property

**Signal resolution**: Reading a signal's current value when resolving a node property. A value obtained with `:get()` is a snapshot rather than a live property. _Avoid_: binding, unwrapping, dereferencing

**Dirty scene**: A retained scene awaiting re-resolution after a signal write. _Avoid_: damage, invalidation, stale scene

**Change handler**: A capability callback receiving the new and previous payload on a pushed snapshot. Registrations belong to one config evaluation. _Avoid_: watcher, subscription, signal listener, event

**Drag**: A left-button interaction reporting start, movement and end in the target button's coordinates. _Avoid_: gesture, slider node, grab

**Frame gating**: Permission to repaint only when compositor pacing permits it and the surface's display content has changed. _Avoid_: vsync, throttling, damage

**Tween**: A retained node's property in flight between the value it displayed and the target a pass resolved, advanced per compositor frame callback without Lua. _Avoid_: animation object, transition, Behavior

**Linger**: Keeping a surface mapped after its `visible` source dropped, for as long as its exit tween runs, through `delay(signal, ms)`. _Avoid_: close-hold timer, retained copy

**Keyframe sequence**: A node property walked through a declared list of values, once or repeatedly, driven by elapsed time rather than by what a pass resolved. _Avoid_: timeline, animation group, SequentialAnimation

**Leaving node**: A child the retained scene no longer holds, kept painted at its last rect and out of the flow for the length of its `animate.exit` block, which replaces every tween it was running. _Avoid_: exit transition, removal animation, ghost node

**Pulse**: A signal reading `true` for a fixed window after its source changes value, which is how a config fires a one-shot animation without an imperative call. _Avoid_: trigger, event, restart, edge signal

**Cross-dissolve**: An `image` crossing over a duration from the picture it was holding to the one whose decode has just landed, rather than swapping between them in one frame. _Avoid_: fade, transition, crossfade

**Spring**: A tween whose motion comes from stiffness and damping rather than a duration and a curve, and which hands its running speed to the run that replaces it when the target moves. _Avoid_: physics animation, damped tween, inertia

## Ownership

**Lock authority**: The Supervisor's decision to acquire the lock and authorize its authenticated release. _Avoid_: lock screen, lock client

**Lock surface**: A surface instance covering one output while the Renderer holds the session lock. _Avoid_: lock screen widget

## Capabilities

**Capability**: A named module owning one slice of platform state and its supported actions. _Avoid_: module, service, backend

**Revision**: A capability's state-version counter, carried on snapshots and stamped onto commands. _Avoid_: version, sequence number

**Capability roster**: The complete set of Supervisor capability names with snapshot state exposed to config, including idle. _Avoid_: pre-seed list, known capabilities, `CAPABILITIES`

**Capability registry**: The Supervisor's collection of capability controllers and their state/event channels. _Avoid_: capability manager, service registry, plugin table

**Supervisor state**: The durable session state needed to supervise generations, capabilities, authentication and reloads. _Avoid_: session, context, app state, world

**Capability start**: The first request that starts a capability's backend. Started backends remain for the Supervisor's lifetime. _Avoid_: activation, subscription, enabling a capability

**Oblisk namespace**: The Lua table exposing capabilities, output state, rescue state, version and config location. _Avoid_: globals, the state tree

**Secure submit**: A field's direct delivery of its native secret buffer to a named capability action without exposing the secret to Lua. _Avoid_: secure handle, password callback

**Idle threshold**: A config-registered inactivity duration with matched idle and resume callbacks. _Avoid_: idle timeout, inactivity timer

**Idle inhibit**: A generation-owned hold preventing idle actions through logind inhibition and the Supervisor's event gate. _Avoid_: wake lock, keep-awake handle

**Notification urgency**: The low, normal or critical tier supplied by a notification's sender. _Avoid_: priority, severity

**Do-not-disturb**: The Supervisor-held toggle suppressing noncritical notification sounds. It does not filter the notification feed. _Avoid_: focus mode, silent mode, mute

**Notification body span**: An allowlisted styled-text or trusted-image element in a sanitized notification body. _Avoid_: rich text, HTML fragment

**Primary keyboard**: The selected keyboard represented by the singular keyboard capability. _Avoid_: main keyboard, active keyboard

**Compositor link**: The compositor-specific connection supplying keyboard layout state and switching. _Avoid_: compositor adapter

**Compositor probe**: Session-level detection that selects the supported compositor implementation. _Avoid_: compositor detection trait, session detector

**Track identity**: The combined track ID, URL and title used to distinguish a changed track from a refresh of the same track. _Avoid_: track key, cache key
