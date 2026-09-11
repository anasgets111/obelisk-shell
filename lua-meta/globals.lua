---@meta
-- The remaining engine globals, and the stdlib as ADR-0048 left it.
--
-- HAND-WRITTEN. `just stubs` does not touch it: unlike `nodes.lua`, globals register one at a time
-- in `renderer/src/lua/`, so no roster test exists. `just types` checks `dev-config` against it;
-- edit it with the Rust or drift only appears as a config diagnostic.
--
-- The config VM loads only `COROUTINE | TABLE | STRING | UTF8 | MATH | PACKAGE | OS`, then replaces
-- `os` with four calls. `.luarc.json` disables the other builtins; without that and this `os`
-- declaration, the language server offers runtime-missing `io.open`, `os.execute`, and
-- `debug.getinfo`.

---The fallback chain is read at `shell.lua`'s top level before text measurement. Both readers fall
---back per glyph across it, so one declaration covers body text, CJK and emoji: the codepoint picks
---the face. A node that wants a different family says so with `text.font`, and this chain stays
---behind it as coverage (ADR-0144). Read once, at startup: editing it re-evaluates like any other
---change and does nothing until the shell restarts, because a chain change invalidates every
---measurement (ADR-0043).
---
---Non-string entries, holes, and named keys are refused. `#` is undefined on sparse tables, so a
---hole would lose the tail. An unmatched family is skipped with a diagnostic; typos cost one entry.
---@param chain string[] Family names in fallback order, densest first. A dense array: a hole truncates it.
function fonts(chain) end

---fzf's score for `needle` against `haystack`, and where the match begins; `nil` for no match, so
---`if value then` is the filter. A run, a word boundary, a camelCase hump and a digit each pay a
---bonus, which is what ranks "Visual Studio Code" above every other name holding v, s and c.
---
---Smart case: an all-lowercase needle matches either case, one uppercase character makes the whole
---comparison exact. Ordering is the caller's; this scores one pair (ADR-0201).
---
---Scores compare only between candidates scored against the same needle, and only within one
---alphabet: a non-ASCII haystack takes a cruder greedy scorer whose numbers do not line up with the
---ASCII path's. Both are on fzf's scale, so fzf's own thresholds carry over.
---@param haystack string The text to search, such as an application's name and comment joined.
---@param needle string What the user typed, already trimmed. Empty scores 0 rather than failing.
---@return integer? score, integer? start `start` is a 0-based index into `haystack`.
function fuzzy(haystack, needle) end

json = {}

---Decodes JSON without raising. Errors return `nil` plus a message; JSON `null` also returns `nil`
---through the capability-payload mapping; both mean "no data" and are indistinguishable (ADR-0057).
---A `null` array element leaves a hole, and `ipairs` stops there.
---@param text string The JSON document. Any input is safe, including an empty string.
---@return any value, string? error
function json.decode(text) end

process = {}

---Declares what `obelisk call <name>` runs (ADR-0197).
---
---The outward twin of `obelisk.<cap>:invoke(...)`: a keybind writes a `state` when it wants the
---shell to look different and calls an action when it wants it to *do* something, because rendering
---may not have side effects and a `state` write reaches no config code.
---
---`name` is one opaque string. `"rec.toggle"` groups it for a reader the way a module path does and
---nothing splits on the dot, so any character its config wrote is allowed.
---
---Registrations last one evaluation: declare at the top level, not inside a callback that fires
---more than once. Two declarations of one name in the same evaluation are an error rather than the
---last one winning, since which won would otherwise depend on `require` order.
---
---What the handler returns is converted to JSON and printed by the caller; returning nothing and
---returning `nil` are the same answer. Raising inside it, or returning something that will not
---convert, reaches the caller as a failure and its exit code.
---@param name string
---@param handler fun(...: any): any?
function action(name, handler) end

---@class ProcessHandle
local ProcessHandle = {}

---Safe after the process has already exited.
function ProcessHandle:kill() end

---Spawns a process and streams output without blocking the shell.
---
---`out_cb` fires once per newline-stripped line from `BufReader::lines()`; pretty JSON arrives in
---pieces. Accumulate in `out_cb` and decode in `exit_cb`, the only one that knows it is complete.
---@param cmd string The executable. Resolved on `PATH`; no shell, so no globbing, no pipes and no quoting rules.
---@param args string[] One element per argument, already split. Passing `"a b"` is one argument containing a space.
---@param out_cb fun(line: string, stream: "stdout"|"stderr") Both streams reach the same callback; branch on `stream`.
---@param exit_cb fun(code: integer?) `nil` when the process was killed by a signal rather than exiting.
---@return ProcessHandle # Live immediately. The process is already running when this returns.
function process.run(cmd, args, out_cb, exit_cb) end

---Spawns a program and lets go of it completely.
---
---The program gets its own session and is reparented to `init`, so it is not this shell's child in
---any process tree, no reload can reap it, and killing the shell leaves it running. That is what a
---launcher wants: an editor opened from one should outlive the config edit that follows.
---
---There is no handle, no output and no exit code, because none of those survive letting go. Use
---[`process.run`] for anything whose output or exit you need, and this for anything you are
---handing to the user (ADR-0188).
---
---Its three standard streams go to `/dev/null`: nothing is reading them, and leaving them
---inherited lets a program write over the shell's own log long after it stopped being related.
---@param cmd string The executable. Resolved on `PATH`; no shell, so no globbing, no pipes and no quoting rules.
---@param args string[] One element per argument, already split. Passing `"a b"` is one argument containing a space.
function process.detach(cmd, args) end

---@class SessionProcessHandle
---One program declared with [`session_process`]. Every field is a signal over this program's entry
---in `obelisk.processes`, and the three methods are the only ways to move it: there is no handle to
---hold, because holding one is exactly what a config cannot do across a reload.
---@field running Signal<boolean> Whether it is up now. The other fields describe the current run while this is true and the finished one while it is false.
---@field pid Signal<integer?> Its process id, which is also its process group. `nil` until the first `start`, and kept after an exit.
---@field started_at Signal<integer?> Unix seconds when the current or last run began. Subtract it from `obelisk.system`'s clock for elapsed time; nothing here needs a second timer.
---@field exit_code Signal<integer?> How the last finished run ended. `nil` while running, before the first run, and when a signal ended it rather than an exit.
---@field start_error Signal<string> Why the last `start` produced no process -- usually a command that is not on `PATH`. Empty when it spawned. Without reading this, a config waiting on `running` waits forever.
local SessionProcessHandle = {}

---Runs the program, replacing whatever the last run left behind.
---
---A name already running is left alone rather than started twice; `running` says which case this
---was. Nothing is returned: the outcome arrives as state, like every other capability
---(docs/lua-api.md § 3).
---@param cmd string The executable. Resolved on `PATH`; no shell, so no globbing, no pipes and no quoting rules.
---@param args? string[] One element per argument, already split. Omitted means a bare command.
function SessionProcessHandle:start(cmd, args) end

---Sends one signal to the program itself, not its group: a pause belongs to the program that was
---named, not to helpers it happened to spawn.
---
---Silently does nothing when it is not running, because acting on state one push old is ordinary.
---@param signal "TERM"|"INT"|"HUP"|"QUIT"|"USR1"|"USR2"|"KILL"|"STOP"|"CONT" Named without its `SIG` prefix. An unknown name is refused rather than guessed at.
function SessionProcessHandle:signal(signal) end

---Asks the program's whole group to stop with the signal its declaration named, escalating to
---`SIGKILL` five seconds later. Session shutdown does the same thing to every declared program.
function SessionProcessHandle:stop() end

---Declares a program whose lifetime is the session's rather than this generation's.
---
---[`process.run`]'s child belongs to the generation that spawned it: a config edit that changes
---topology swaps generations, and the swap reaps that child's process group. Right for a helper
---that answers a question and exits, wrong for anything the user would notice stopping -- a
---recorder mid-file, a stream a widget is reading. This declares the second kind. The Supervisor
---holds it, does not restart on a config edit, and answers for it in `obelisk.processes`.
---
---What is given up in exchange is output: stdio is inherited rather than piped, because a program
---that outlives the generation that started it has no callback left to deliver a line to. A config
---that wants a program's output wants `process.run`.
---
---Re-declaring a name returns the same handle and keeps a running program running, so this call
---belongs at a module's top level. Only `stop_signal` is re-read, which is what lets that be
---edited without stopping anything.
---@param spec { name: string, stop_signal?: "TERM"|"INT"|"HUP"|"QUIT"|"USR1"|"USR2"|"KILL"|"STOP"|"CONT" } `name` keys the program in `obelisk.processes`; `stop_signal` is how it wants to be asked to finish, `"TERM"` by default. A program that writes a file it has to close on the way out says so here.
---@return SessionProcessHandle # The same handle for every declaration of one name.
function session_process(spec) end

---@class oslib
---The four `os` calls ADR-0048 keeps read process-local state without a waiting syscall. The rest,
---including `os.execute` and `os.remove`, is gone: the 5ms CPU cap counts instructions; a thread
---parked in a blocking syscall executes none, cannot be caught, and would wedge the Wayland thread.
os = {}

---@param format? string `strftime` directives, or `"*t"` for a table. Defaults to `"%c"`. A leading `!` reads UTC.
---@param time? integer Unix seconds to format. Defaults to now.
---@return string|table # A string, or a table when `format` starts with `"*t"`.
function os.date(format, time) end

---@param t? table A `os.date("*t")`-shaped table to convert. Omitted means now.
---@return integer # Unix seconds. Wall clock, so it moves when the clock is set; use `os.clock` for durations.
function os.time(t) end

---@return number # CPU seconds used by this process, as a float. Monotonic and immune to a clock change, which is what makes it the one to subtract.
function os.clock() end

---@param name string The variable to read.
---@return string? # Its value, or `nil` when unset. The shell's own environment, not the compositor's.
function os.getenv(name) end
