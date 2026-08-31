---@meta
-- The remaining engine globals, and the stdlib as ADR-0048 actually left it.
--
-- The sandbox is the reason this file matters most. The config VM loads only
-- `COROUTINE | TABLE | STRING | UTF8 | MATH | PACKAGE | OS`, and then replaces `os` with a table
-- holding four calls. Without the `runtime.builtin` disables in `.luarc.json` plus the `os`
-- declaration below, the language server would offer `io.open`, `os.execute` and `debug.getinfo`,
-- none of which exist at runtime. A red squiggle beats reading a stack trace.

---The font chain, in fallback order. Called at the top level of `shell.lua`, before anything
---measures text. Both readers fall back per glyph across the whole chain, so one declaration
---covers body text and Nerd Font private-use glyphs: the codepoint picks the face, not the node.
---
---Read once, at startup. Editing it re-evaluates like any other change and does nothing until the
---shell restarts, because a chain change invalidates every measurement in the shell (ADR-0043).
---
---Refused if an entry is not a string, or if the table has a hole or a named key: `#` is undefined
---on a sparse table, so a hole would silently lose the tail. A family no font matches is skipped
---with a diagnostic, so a typo costs that entry and not the chain.
---@param chain string[]
function fonts(chain) end

json = {}

---Decodes JSON to a Lua value. Never raises, on any input.
---
---Returns `nil` plus a message on a decode error. A JSON `null` decodes to `nil` as well, since it
---goes through the same mapping every capability payload does, so a successful null and a failure
---are indistinguishable. Both mean "no data" (ADR-0057). A `null` array element leaves a hole and
---`ipairs` stops at it.
---@param text string
---@return any value, string? error
function json.decode(text) end

process = {}

---@class ProcessHandle
local ProcessHandle = {}

---Kills the process. Safe to call after it has already exited.
function ProcessHandle:kill() end

---Spawns a process and streams its output. Never blocks the shell.
---
---`out_cb` fires once per line with the newline stripped, because the supervisor reads the child
---through `BufReader::lines()`. A pretty-printed JSON document therefore arrives in pieces, and
---only `exit_cb` knows the buffer is whole: accumulate in one, decode in the other.
---@param cmd string
---@param args string[]
---@param out_cb fun(line: string, stream: "stdout"|"stderr") Both streams reach the same callback; branch on `stream`.
---@param exit_cb fun(code: integer?) `nil` when the process was killed by a signal rather than exiting.
---@return ProcessHandle
function process.run(cmd, args, out_cb, exit_cb) end

---@class oslib
---The four `os` calls ADR-0048 keeps, each of which reads process-local state and returns without
---a syscall that waits. Everything else in the library is gone, `os.execute` and `os.remove`
---included: the 5ms CPU cap is an instruction-count hook, and a thread parked in a syscall
---executes no instructions, so a blocking call cannot be caught and would wedge the Wayland thread.
os = {}

---@param format? string
---@param time? integer
---@return string|table
function os.date(format, time) end

---@param t? table
---@return integer
function os.time(t) end

---@return number
function os.clock() end

---@param name string
---@return string?
function os.getenv(name) end
