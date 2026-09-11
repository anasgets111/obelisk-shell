---@meta
-- The reactive layer: `Signal` and the globals that make or read one.
--
-- HAND-WRITTEN and unchecked, like `globals.lua`: no roster test covers it, so drift from
-- `renderer/src/lua/signal.rs` appears only when `just types` reports a false `dev-config` error.
--
-- These stubs serve lua-language-server only; the engine never loads this directory. They stay
-- outside `dev-config/obelisk/` because `supervisor/src/watcher.rs` reloads on any config-tree
-- `.lua`
-- save, so a stub there would restyle the bar on every edit.

---@class Signal<T>: userdata
---Read-only reactive `T`, resolved at layout time on every pass; a node property holding its handle
---follows the value (ADR-0044 decision 1).
---
---The following three details were measured against `lua-language-server` 3.19.1.
---
---`: userdata` makes node properties reject payload tables. `---@class` accepts any table-shaped
---value, so `string|Signal` let `text.content` receive any IDL table, including a notification's
---`body` span array, which froze the shell on its last good scene. Built-in `userdata` rejects
---tables and matches runtime signals, as `components/icon_button.lua`'s `is_signal` tests.
---Properties use [`Bound`], not this class, so `string|Signal` remains permissive even with
---`: userdata`.
---
---`<T>` types the callback: `obelisk.network:map(function(n) ... end)` gives `n` the
---`NetworkState` type, making a misspelled field an edit-time `undefined-field`, not runtime `nil`.
---
---Methods must be `---@field`s. With `function Signal:map(fn)` and `---@param fn fun(value: T)`,
---the class's `T` does not bind, so the annotation reads correctly but checks nothing.
---@field get fun(self: Signal<T>): T The value now, as a plain Lua value. Not reactive: the result is indistinguishable from a literal and nothing updates it until the next config edit. `nil` for a capability that has not pushed yet.
---@field map fun(self: Signal<T>, fn: fun(value: T): any): Signal<any> A new computed signal applying `fn` to this one's value on every read. `fn` must be side-effect-free, and shares the 5ms budget with the rest of the graph. The original is untouched, so one source can feed several maps. ponytail: the result is `Signal<any>` rather than the mapped type, because a `---@field` cannot introduce a second type parameter -- one hop is typed, a chain past it is not.

---A node property accepting either a literal or a signal carrying one.
---
---It is `userdata`, not `Signal`, because a `---@class` in a union admits tables (see [`Signal`]).
---Any userdata satisfies it; a signal is the only userdata a config can hold.
---@alias Bound userdata

---@class StateSignal<T>: Signal<T>
---What `state(name, initial)` returns: the one signal a config writes. `T` comes from `initial`, so
---`state("panel_open", false)` refuses `:set("open")`.
---@field set fun(self: StateSignal<T>, value: T) Stores a new value and marks the scene dirty, so the next pass re-resolves every node reading this signal. Marshal-checked: a NaN, an infinity or an oversized string is refused rather than truncated.

---Reactive state keyed by a name that outlives one evaluation.
---
---An in-place reload returns the prior signal, keeping an open dropdown open across an unrelated
---save. Editing `initial` re-seeds it because that write is later than `:set()` (ADR-0044 decision
---5 and amendment); a table `initial` is never an edit because tables compare by identity and each
---evaluation creates a new one.
---
---The name is also the target of `obelisk set <name> <value>`, `obelisk toggle <name>` and
---`obelisk toggle <name> <value>` (to the value, or back to `initial` when it already holds it), a
---compositor keybind's way in (ADR-0112). The write behaves like `:set()` and is refused if
---undeclared.
---@generic T
---@param name string The identity. Two calls with one name are one signal.
---@param initial T The value on the first evaluation that names it, and what fixes the signal's type: `state("panel_open", false)` refuses a later `:set("open")`.
---@return StateSignal<T> # The same signal on every evaluation that names it, so a handle captured last reload is still the live one.
function state(name, initial) end

---@class PersistentTable
---A named JSON file. Every key but `set` is a signal over its stored value, `nil` until the first
---push and for a missing key.
---@field set fun(self: PersistentTable, key: string, value: any) Stores one key and saves the file a second after the last write. Any JSON value, tables included. `nil` deletes the key.
---@field [string] Signal<any>

---A persisted table: a named JSON file read as signals.
---
---The framework owns no location (ADR-0136). Build `path` from `obelisk.config_dir`,
---`os.getenv("XDG_STATE_HOME")`, or anything else; use two stores for separate settings and cache
---files. The file appears on first save.
---
---Re-declaring a file returns one table. Reload re-runs the call;
---`defaults` fills only absent keys, so adding one does not reset the user's value.
---@param spec { path: string, name: string, defaults?: table } `path` is an absolute directory, `name` one file name, `defaults` the keys to seed it with.
---@return PersistentTable # The same table for every declaration of one file.
function persistent_table(spec) end

---A signal recomputed from several side-effect-free dependencies; CPU time is capped at 5ms across
---the graph (ADR-0021).
---ponytail: `fn` parameters are untyped. Typing each dependency needs an overload per arity, while
---`dev-config` uses one to three and nothing prevents four. Prefer typed `map` for one source; use
---this for two or more.
---@param dependencies Bound[] In the order `fn` receives them. A dense array, and re-reading any one of them re-runs `fn`.
---@param fn fun(...): any Receives one argument per dependency, in order.
---@return Signal<any> # Read-only, like `map`. `:set()` refuses it: the dependencies are the writers.
function computed(dependencies, fn) end

---Whether the pointer is inside the node declaring this slot. `false` until it is;
---the engine writes it, and `:set()` refuses it (ADR-0062).
---@param name string The slot. Naming it on a node's `hover` marks that node's box as this region; two calls with one name are one signal (ADR-0062 decision 2).
---@return Signal<boolean> # Whether the pointer is inside that slot's node.
function hover(name) end

---The hovered region's absolute `{ x, y, width, height }` in surface logical coordinates. It keeps
---the last rect after pointer exit, so a closing `popup`'s `anchor_rect` stays non-zero.
---@param name string The same slot `hover` takes. Reading this one does not register a region; the `hover` property does that.
---@return Signal<Rect> # The region's absolute rect in its surface's logical coordinates.
function hover_rect(name) end

---`source` after it has held a new value for `ms` (ADR-0146). Reads answer the old value until then;
---a source that returns to it before the hold elapses changes nothing. Two jobs in one shape: a
---close-hold that keeps a surface mapped while its exit tween runs, `visible = computed({ open,
---delay(open, ms) }, function(now, was) return now or was end)`, and a trailing debounce.
---@generic T
---@param source Signal<T> Any signal or capability.
---@param ms integer The hold, `[1, 60000]` ms, rounded to whole milliseconds.
---@return Signal<T> # Read-only; the source is the writer.
function delay(source, ms) end

---`true` for `ms` after `source` changes value, `false` the rest of the time (ADR-0153). The other
---half of `delay`'s shape: that one answers the old value until a change settles, this one says a
---change just happened. It is how a one-shot animation fires, since a config cannot call
---`restart()`: `animate = pulse(clicks, 400):map(function(on) return on and { opacity = { ... } } or {} end)`
---starts a sequence when the window opens and drops it when the window closes (ADR-0152). A change
---while the window is open restarts it. Gate the direction with `computed` when only one edge
---should fire: `computed({ pulse(plugged, ms), plugged }, function(fired, on) return fired and on end)`.
---@param source Signal<any> Any signal or capability.
---@param ms integer The window, `[1, 60000]` ms, rounded to whole milliseconds. Make it at least as long as what it drives.
---@return Signal<boolean> # Read-only; the source is the writer.
function pulse(source, ms) end

---The laid-out `{ x, y, width, height }` of the node declaring `geometry = geometry(name)`, in its
---surface's logical coordinates, the same space `on_click` and `hover_rect` report (ADR-0147). The
---layout pass and tween ticks write it; Lua cannot. A pass that changes it earns one follow-up
---pass, so a binding on it settles right after the node it measures; a tick's write earns none, and
---a binding fed by its own measurement stops after that one pass. Zero until the first layout.
---This is QML's `item.height` for a reveal that slides a card by its own height.
---@param name string The slot. Naming it on a node's `geometry` makes that node the one measured; two calls with one name are one signal.
---@return Signal<Rect> # The node's absolute rect in its surface's logical coordinates.
function geometry(name) end

---@class ScrollSignal: Signal<number>
---What `scroll(name)` returns. The engine writes the offset; config can request
---which child to show.
---@field reveal fun(self: ScrollSignal, index: integer) Asks the next layout pass to scroll the viewport so its `index`-th visible child (1-based, a `list`'s generated items counted in source order) is inside it, moving the least distance that does so and nothing at all when it already is. One-shot: the pass that honours it consumes it, and the wheel takes over again from there. Past the end lands on the end; an index with no child changes nothing. This is how a keyboard selection driven by `on_navigate` keeps its row in view (ADR-0112).

---How far a container has scrolled along its main axis, in logical pixels. `0` is top or left. The
---wheel writes it and layout clamps it (ADR-0069); `:reveal(index)` is config's only request.
---@param name string The slot. Naming it on a `row`, `column` or `list`'s `scroll` makes that node the viewport.
---@return ScrollSignal # The offset along the viewport's main axis, in logical pixels.
function scroll(name) end
