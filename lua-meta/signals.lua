---@meta
-- The reactive layer: `Signal` and the five globals that make or read one.
--
-- HAND-WRITTEN and unchecked, on the same terms as `globals.lua`: no roster test stands behind it,
-- so a signature that drifts from `renderer/src/lua/signal.rs` is caught only when `just types`
-- reports the drift as a false error on `dev-config`.
--
-- These stubs are for lua-language-server only. The engine never loads this directory, and it
-- deliberately sits outside `dev-config/oblisk/` because `supervisor/src/watcher.rs` reloads the
-- shell on any `.lua` file inside the config tree, so a stub living there would restyle the bar
-- every time you saved one.

---@class Signal<T>: userdata
---Read-only reactive value carrying a `T`. Resolves at layout time on every pass, so a handle left
---in a node property makes that node follow the value (ADR-0044 decision 1).
---
---Three things about this declaration are load-bearing, and all three were measured against
---`lua-language-server` 3.19.1 rather than assumed.
---
---`: userdata` is what makes a node property reject a payload table. A `---@class` accepts any
---table-shaped value, in a union or alone, whether or not it declares required fields -- so while
---every slot in `nodes.lua` read `string|Signal`, `text.content` accepted literally any table, and
---a notification's `body` span array reached the engine with nothing between the correct stub and
---the shell freezing on its last good scene. Only the built-in `userdata` refuses a table, and it
---is the honest spelling anyway: a signal *is* userdata at runtime, which is exactly what
---`components/icon_button.lua`'s `is_signal` tests. Property slots therefore name [`Bound`], not
---this class -- `string|Signal` stays permissive even with `: userdata` on the class.
---
---`<T>` is what types the callback. `oblisk.network` is a `Signal<NetworkState>`, so the `n` in
---`oblisk.network:map(function(n) ... end)` is a `NetworkState` and a misspelled field is an
---`undefined-field` at edit time rather than a `nil` at runtime.
---
---The methods are `---@field`s and must stay that way. Written as `function Signal:map(fn)` with
---`---@param fn fun(value: T)`, the class's own `T` does not bind and the annotation silently does
---nothing -- it reads correctly and checks nothing, the worst of both.
---@field get fun(self: Signal<T>): T The value now, as a plain Lua value. Not reactive: the result is indistinguishable from a literal and nothing updates it until the next config edit. `nil` for a capability that has not pushed yet.
---@field map fun(self: Signal<T>, fn: fun(value: T): any): Signal<any> A new computed signal applying `fn` to this one's value on every read. `fn` must be side-effect-free, and shares the 5ms budget with the rest of the graph. The original is untouched, so one source can feed several maps. ponytail: the result is `Signal<any>` rather than the mapped type, because a `---@field` cannot introduce a second type parameter -- one hop is typed, a chain past it is not.

---A node property that accepts either a literal or a signal carrying one.
---
---Spelled `userdata` rather than `Signal` deliberately, and the difference is the whole reason
---this alias exists: see [`Signal`] for what a `---@class` in a union lets through. Any userdata
---satisfies it, which costs nothing here because a signal is the only userdata a config can hold.
---@alias Bound userdata

---@class StateSignal<T>: Signal<T>
---What `state(name, initial)` returns. The one signal a config writes. `T` is inferred from
---`initial`, so `state("panel_open", false)` refuses a `:set("open")`.
---@field set fun(self: StateSignal<T>, value: T) Stores a new value and marks the scene dirty, so the next pass re-resolves every node reading this signal. Marshal-checked: a NaN, an infinity or an oversized string is refused rather than truncated.

---Reactive state the config owns, keyed by a name that outlives any single evaluation.
---
---An in-place reload hands back the signal the last evaluation built, so an open dropdown stays
---open across an unrelated save. Editing `initial` re-seeds it, because the edit is a later write
---than the `:set()` it lands on (ADR-0044 decision 5 and its amendment). A table `initial` is
---never an edit, since tables compare by identity and every evaluation builds a fresh one.
---
---The name is also what `oblisk set <name> <value>` and `oblisk toggle <name>` address from outside
---the shell -- a compositor keybind's way in (ADR-0112). The write lands here exactly as `:set()`
---would, and is refused by name when no evaluation declared the state.
---@generic T
---@param name string The identity. Two calls with one name are one signal.
---@param initial T The value on the first evaluation that names it, and what fixes the signal's type: `state("panel_open", false)` refuses a later `:set("open")`.
---@return StateSignal<T> # The same signal on every evaluation that names it, so a handle captured last reload is still the live one.
function state(name, initial) end

---A signal recomputed from several dependencies. `fn` must be side-effect-free, and its CPU time
---is capped at 5ms across the whole dependency graph (ADR-0021).
---ponytail: `fn`'s parameters are untyped, unlike `map`'s. One type parameter per dependency
---would need an overload per arity, and the arity here runs from one to three across `dev-config`
---with nothing stopping a fourth. `map` covers the single-dependency case with its parameter
---typed, so prefer it where one source is enough and reach for this when two must be combined.
---@param dependencies Bound[] In the order `fn` receives them. A dense array, and re-reading any one of them re-runs `fn`.
---@param fn fun(...): any Receives one argument per dependency, in order.
---@return Signal<any> # Read-only, like `map`. `:set()` refuses it: the dependencies are the writers.
function computed(dependencies, fn) end

---Whether the pointer is inside the node that declared this slot. `false` until it is. Read-only:
---`:set()` refuses it, because the engine is the writer (ADR-0062).
---@param name string The slot. Naming it on a node's `hover` marks that node's box as this region; two calls with one name are one signal (ADR-0062 decision 2).
---@return Signal<boolean> # Whether the pointer is inside that slot's node.
function hover(name) end

---The hovered region's absolute rect, `{ x, y, width, height }`, in its surface's logical
---coordinates. Keeps the last rect it was given when the pointer leaves, so a `popup`'s
---`anchor_rect` stays non-zero while it closes.
---@param name string The same slot `hover` takes. Reading this one does not register a region; the `hover` property does that.
---@return Signal<Rect> # The region's absolute rect in its surface's logical coordinates.
function hover_rect(name) end

---@class ScrollSignal: Signal<number>
---What `scroll(name)` returns. The offset is the engine's to write; what a config may say is which
---child it wants to see.
---@field reveal fun(self: ScrollSignal, index: integer) Asks the next layout pass to scroll the viewport so its `index`-th visible child (1-based, a `list`'s generated items counted in source order) is inside it, moving the least distance that does so and nothing at all when it already is. One-shot: the pass that honours it consumes it, and the wheel takes over again from there. Past the end lands on the end; an index with no child changes nothing. This is how a keyboard selection driven by `on_navigate` keeps its row in view (ADR-0112).

---How far a container has been scrolled along its main axis, in logical pixels. `0` at the top or
---left. Read-only: the wheel writes it and the layout pass clamps it (ADR-0069); `:reveal(index)`
---is the one ask a config can make of it.
---@param name string The slot. Naming it on a `row`, `column` or `list`'s `scroll` makes that node the viewport.
---@return ScrollSignal # The offset along the viewport's main axis, in logical pixels.
function scroll(name) end
