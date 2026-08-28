# The config VM drops the blocking parts of the Lua stdlib

mlua's `Lua::new()` loads `StdLib::ALL_SAFE`, which is everything except `debug` and `ffi`. So a
config has `io` and `os` in full, including `io.open`, `io.read`, `os.execute`, and `os.exit`.

ADR-0039 moved the Lua VM onto the Wayland thread. Every one of those calls blocks that thread. A
config with `os.execute("notify-send hi")` in a `computed` freezes every surface on every monitor
until the child exits, and ADR-0021's 5ms CPU cap does not save it: the cap is an instruction-count
hook, and a thread parked in a blocking syscall executes no instructions.

That is the whole argument. This is not about malicious configs. A shell config is written by the
person running it on their own machine, and there is nothing here to defend against. It is about a
one-line mistake taking the compositor's frame loop down with it.

## Decision: remove `io`, and cut `os` to the calls that cannot block

Construct the VM with an explicit `StdLib` set rather than `ALL_SAFE`, dropping `IO` and `OS`. Then
re-register the four `os` functions worth keeping, which read process-local state and return
immediately:

| Kept | Why |
| :--- | :--- |
| `os.time` | clock reads, no syscall that waits |
| `os.date` | formatting, same |
| `os.clock` | process CPU time |
| `os.getenv` | reads the existing environment block |

Everything else in `os` goes: `execute`, `exit`, `remove`, `rename`, `tmpname`, `setlocale`. All of
`io` goes.

`process.run` is already the answer for running a command, and it is the right one: non-blocking,
callback-delivered, envelope-guarded, generation-stamped, and reaped by the Supervisor
(ADR-0018, ADR-0026). Leaving `os.execute` in place gives a config a second way to run a program that
bypasses the envelope, the generation guard, and the process registry, and stalls the frame loop as
its reward. The narrower surface is also the better-supported one.

`os.exit` deserves its own sentence: a config calling it terminates the Renderer mid-generation with
no PBA teardown, which the Supervisor would see as a crash and treat as one.

## What this costs, honestly

Reading a file from Lua becomes impossible. That is a real loss, and configs want it: a theme file, a
value out of `/sys`, a cached token.

Take the loss for now. The ten capabilities already cover the hardware state a shell actually reads,
`require` covers loading Lua data files as of ADR-0047, and `process.run "cat"` covers the rest
badly but adequately. When something concrete needs it, the answer is a non-blocking
`oblisk.read_file` returning through the same callback path `process.run` uses, not restoring a
blocking `io`.

Building that now would be building the second file-reading mechanism before the first has a caller.

## Rejected: keep the stdlib and document the hazard

Leave `io` and `os` in place, note in the docs that blocking calls stall rendering, and trust config
authors.

Rejected because the failure is invisible in a way documentation does not fix. A config that blocks
for 40ms does not error, does not log, and does not look broken while being written. It shows up as
occasional stutter that a user will attribute to the compositor, the GPU driver, or Oblisk itself
long before they suspect the two-line function they wrote last week.

## Rejected: run Lua on its own thread again so blocking is contained

Undo ADR-0039's consolidation for this.

Rejected because ADR-0039 weighed exactly this cost and took it deliberately, and the payment was
worth more than the protection: two channels deleted, one `FontSystem` deleted, and an entire class
of request-and-response plumbing avoided for surface creation, input dispatch, and output geometry. A
thread boundary that exists only to contain `os.execute` is a bad trade when deleting `os.execute` is
one line.

## Consequences

`Loader::new` stops calling `Lua::new()` and calls `Lua::new_with(...)` with the explicit set, then
registers the four kept `os` functions before `register_node_constructors`.

`debug` and `ffi` were already absent under `ALL_SAFE` and stay absent. `coroutine`, `string`,
`table`, `math`, and `utf8` are untouched: none of them block, and `coroutine` in particular is worth
keeping for configs that want to structure their own logic.

`oblisk-idl-api-specs.md` § 1 gains a short statement of what the config VM contains, since "Lua 5.4"
alone no longer describes it.
