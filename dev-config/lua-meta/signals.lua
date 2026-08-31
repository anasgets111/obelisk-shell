---@meta
-- The reactive layer: `Signal` and the five globals that make or read one.
--
-- These stubs are for lua-language-server only. The engine never loads this directory, and it
-- deliberately sits outside `dev-config/oblisk/` because `supervisor/src/watcher.rs` reloads the
-- shell on any `.lua` file inside the config tree, so a stub living there would restyle the bar
-- every time you saved one.

---@class Signal
---Read-only reactive value. Resolves at layout time on every pass, so a handle left in a node
---property makes that node follow the value (ADR-0044 decision 1).
local Signal = {}

---The value now, as a plain Lua value. Not reactive: the result is indistinguishable from a
---literal and nothing updates it until the next config edit.
---@return any
function Signal:get() end

---A new computed signal applying `fn` to this one's value on every read.
---@param fn fun(value: any): any
---@return Signal
function Signal:map(fn) end

---@class StateSignal: Signal
---What `state(name, initial)` returns. The one signal a config writes.
local StateSignal = {}

---Stores a new value and marks the scene dirty, so the next pass re-resolves every node reading
---this signal. Marshal-checked: a NaN, an infinity or an oversized string is refused.
---@param value any
function StateSignal:set(value) end

---Reactive state the config owns, keyed by a name that outlives any single evaluation.
---
---An in-place reload hands back the signal the last evaluation built, so an open dropdown stays
---open across an unrelated save. Editing `initial` re-seeds it, because the edit is a later write
---than the `:set()` it lands on (ADR-0044 decision 5 and its amendment). A table `initial` is
---never an edit, since tables compare by identity and every evaluation builds a fresh one.
---@param name string The identity. Two calls with one name are one signal.
---@param initial any The value on the first evaluation that names it.
---@return StateSignal
function state(name, initial) end

---A signal recomputed from several dependencies. `fn` must be side-effect-free, and its CPU time
---is capped at 5ms across the whole dependency graph (ADR-0021).
---@param dependencies (Signal|StateSignal)[]
---@param fn fun(...): any Receives one argument per dependency, in order.
---@return Signal
function computed(dependencies, fn) end

---Whether the pointer is inside the node that declared this slot. `false` until it is. Read-only:
---`:set()` refuses it, because the engine is the writer (ADR-0062).
---@param name string
---@return Signal
function hover(name) end

---The hovered region's absolute rect, `{ x, y, width, height }`, in its surface's logical
---coordinates. Keeps the last rect it was given when the pointer leaves, so a `popup`'s
---`anchor_rect` stays non-zero while it closes.
---@param name string
---@return Signal
function hover_rect(name) end

---How far a container has been scrolled along its main axis, in logical pixels. `0` at the top or
---left. Read-only: the wheel writes it and the layout pass clamps it (ADR-0069).
---@param name string
---@return Signal
function scroll(name) end
