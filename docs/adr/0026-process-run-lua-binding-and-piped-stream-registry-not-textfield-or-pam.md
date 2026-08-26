# `process.run`'s Lua binding and piped stream registry ship; `textfield`/PAM stay deferred

Phase 15's title ("`process.run`, Stream Piping & Secure Input") and build-steps.md's own text
list three items. This phase implements only item 1 (`process.run`'s Lua binding and
non-blocking stdout/stderr piping, closing ADR-0018 items 1-2). Items 2 (`textfield`
`secure_submit`) and 3 (a real PAM conversation) are out of scope, for reasons grounded in this
codebase's current state, not just build-steps.md's own phase split:

1. **`textfield` needs a scene node that doesn't exist.** `renderer/src/layout/scene.rs`'s
   `ensure_supported_kind` explicitly matches only `"surface" | "rect" | "row" | "column" |
   "text" | "icon" | "button"` and falls through to `LayoutError::UnsupportedNodeKind` for
   anything else, `"textfield"` included -- confirmed by
   `renderer/src/layout/mod.rs`'s own doc comment ("Scope ceilings recorded in docs/adr/0023:
   `list`/`textfield` are unsupported node kinds") and a live regression test,
   `scene.rs`'s `surface_from(r#"...textfield {}..."#)` asserting exactly
   `LayoutError::UnsupportedNodeKind(k) if k == "textfield"`. `textfield` also needs
   `wp-text-input-v3` protocol support, and `renderer/Cargo.toml`'s dependency tree has no such
   crate at all -- `wayland-protocols`'s `unstable` feature and `wayland-protocols-wlr`'s
   `client` feature are the only protocol surface currently pulled in, neither of which covers
   text input. Two real prerequisites, neither built; building `secure_submit`'s wire plumbing
   ahead of a node that can't even lay out yet would be speculative.
2. **A real PAM conversation needs a PAM crate choice this codebase has explicitly deferred.**
   build-steps.md's own Phase 15 item 3 text calls the choice "unresearched" and demands its own
   spike + ADR before wiring it in -- separate work, not a call this phase is positioned to make
   correctly in passing.

## The spec

`docs/oblisk-supervisor-services-dbus.md` § 12's contract: `process.run`-spawned children are
non-blocking, line-buffered stdout/stderr into Lua callbacks ("No Lua Blockage"), each in its own
process group, tracked by the Supervisor, `SIGTERM`-then-`SIGKILL`-after-100ms on a configuration
reload. `docs/oblisk-idl-api-specs.md` § 3.2/3.3: `process.run(cmd, args, out_cb, exit_cb)`
returns a `ProcessHandle` userdata with `process_handle:kill()`.

## Decisions already made for this phase (recorded here for traceability, not re-litigated)

**Where Lua lives, and why no cross-thread bridging is needed here.**
`renderer/src/socket.rs`'s `RendererClient` owns the one `Loader` (and its `mlua::Lua` VM) for
the process's whole life, and `dispatch_loop` (frame I/O) runs on the same dedicated
current-thread tokio runtime. A `process.run` Lua closure therefore executes on the same thread
that will later write the outbound request out -- unlike Phase 14's Wayland-thread work, no
`std::sync::mpsc` bridge is needed, only an in-thread `tokio::sync::mpsc` queue so the
synchronous Lua closure (which has no `&mut write_half` in scope) can hand its request to
`dispatch_loop`'s own `tokio::select!`.

**Wire protocol reuses `CommandEnvelope`, no new request type.** `capability: "process", action:
"run"` / `"kill"`, carried in the existing `RendererFrame::Command(CommandEnvelope)` -- already
decoded by `supervisor/src/socket.rs` and reaching `supervisor/src/main.rs`'s inbound match arm,
which previously just `eprintln!`'d it; that line is now the real `("process", "run")`/
`("process", "kill")` dispatch.

**`CommandEnvelope.id` is the process handle's stable id, assigned by the Renderer.**
`process.run` must return a `ProcessHandle` to Lua synchronously, before any socket round trip
can complete, so the id can't come back from the Supervisor -- it's a monotonic counter scoped to
one `ProcessRegistry` (one per `RendererClient`/Lua VM instance; no persistence across reloads
needed, since a fresh `RendererClient` and VM is the only case that matters). Both `"run"` and the
later `"kill"` (`process_handle:kill()`) carry the same `id`, keying the Supervisor's registry
alongside `generation_id`.

**Supervisor-side registry is channel-actor, not a shared mutex.** Every cross-task/cross-thread
piece of *spawn-tracking* state so far (`challenges`, `audio_apps`, `reload_events`,
`inbound_frames`) funnels into `main()`'s single `tokio::select!` via an mpsc channel, not a
shared mutex (`socket::GenerationRegistry`'s own `connections` map is a pre-existing
`Arc<Mutex<...>>`, but it solves a different problem -- sharing live connections across the
listener's accept loop and every connection task -- not spawn-tracking). `processes:
HashMap<(u32, u64), tokio::process::Child>` (keyed by `generation_id`, `id`) lives as a plain
local in `main()`, mutated only from inside `main()`'s own `select!` arms -- the same shape
ADR-0018 explicitly left for this phase's real caller to invent.

**Piped stdio needs a new spawn primitive, not a change to `spawn_group_leader`.**
`process::spawn_group_leader` inherits stdio today, load-bearing for the boot Renderer spawn and
every PBA candidate spawn (both rely on their stdout/stderr reaching the Supervisor's own
terminal). `process.run` needs piped stdout/stderr instead, so a new `spawn_group_leader_piped`
function is added; `spawn_group_leader`'s signature and behavior are untouched, and every
existing call site keeps inheriting stdio exactly as before.

## Decision

**`shared/src/lib.rs`**: `ProcessStream` (`Stdout`/`Stderr`, a real two-variant enum -- Baseline
Smells rejects a bare string tag here, and `ReevaluateReport` already sets the "real enum, not
stringly-typed" precedent this follows), `ProcessOutputLine { id, stream, line }`, and
`ProcessExited { id, code: Option<i32> }` (absent exactly when `std::process::ExitStatus::code()`
itself would return `None` -- killed by signal, or never spawned at all). Both join
`SupervisorFrame` as `ProcessOutput`/`ProcessExited`, same adjacently-tagged convention as every
other variant, each with the standard round-trip test.

**`supervisor/src/process/mod.rs`**: `spawn_group_leader_piped(cmd, args, envs)` -- identical to
`spawn_group_leader` except `.stdout(Stdio::piped()).stderr(Stdio::piped())`; stdin stays
inherited (unchanged from `spawn_group_leader`, and nothing in this phase's spec asks for piped
stdin). `spawn_group_leader` itself is untouched.

**`supervisor/src/main.rs`**: `processes: HashMap<(u32, u64), Child>` and a `process_done_tx` /
`process_done` mpsc channel (`(generation_id, id)` completion reports), both plain locals wired
into `main()`'s existing `select!`. Four small functions carry the actual logic, each
independently testable against real spawned processes rather than only reachable through the
full event loop:

- `spawn_and_register_process`: calls `spawn_group_leader_piped`, takes the piped
  `ChildStdout`/`ChildStderr` off the `Child` before inserting it into `processes`, so the
  registry can still own the `Child` (for `kill`/supersede-reap) while a separate task reads its
  output. Logs and returns `None` on spawn failure.
- `stream_process_output` (a `tokio::spawn`ed task, not routed back through `main()`'s
  `select!`): concurrently reads both piped streams line-by-line via
  `tokio::io::AsyncBufReadExt::lines()`, forwarding each as `SupervisorFrame::ProcessOutput`
  through a cloned `GenerationRegistry` handle directly -- the same `registry.clone()`-into-a-task
  pattern `SocketCandidateLink` already established. Once both streams hit EOF (the process has
  exited or is exiting -- pipe closure is the exit signal here, not a `wait()` this task doesn't
  have `&mut Child` to call), it reports `(generation_id, id)` on `process_done_tx`. This task
  does not itself know the exit code; it only knows the process is done producing output.
- `kill_registered_process` (`("process", "kill")`'s handler): removes `(generation_id, id)` from
  `processes` and calls the already-built `process::reap_process_group`, this phase's promised
  real caller per ADR-0018. `reap_process_group`'s returned `ExitStatus` already carries the real
  code (`None` here in the ordinary case, since `SIGTERM`/`SIGKILL` are signal deaths) -- reused
  directly for the `ProcessExited` sent back to Lua, so a killed process's `exit_cb` still fires
  with an honest code instead of a synthesized one.
- `reap_exited_process` (`process_done`'s handler): removes `(generation_id, id)` from
  `processes` and calls `child.wait()` -- safe to await inline here (not a long block) because
  the streams already closed, meaning the process has already exited or is exiting right now; the
  real `ExitStatus::code()` becomes the `ProcessExited` sent back to Lua.

Both handlers tolerate a missing registry entry silently (`kill` on an already-naturally-exited
process, or the reverse race) -- this is exactly the leak ADR-0018 flagged `reap_process_group`
already guarding against via `ESRCH`-as-success, and the reason the completion channel exists at
all: without it, a naturally-exiting process's entry would never leave `processes`.

On generation supersede (the existing `reap_process_group(&mut authoritative.child, ...)` call,
right before `authoritative` is reassigned), `reap_generations_processes` reaps and drops every
`processes` entry belonging to the superseded `generation_id` -- § 12's "on configuration reload,
SIGTERM the process group" applied to every process the outgoing generation's Lua spawned, not
just its own Renderer process. No `ProcessExited` is sent for these: the superseded generation's
own connection is being torn down in the same swap, so there is no live Lua VM left to receive
it.

**`renderer/src/lua/process.rs`** (new module, alongside `nodes.rs`/`signal.rs`): `ProcessRegistry`
(`Rc<RefCell<...>>`-backed, matching `signal::Signal::Live` and
`supervisor/src/audio/mixer.rs`'s `Rc<RefCell<MixerState>>` -- confined to the one socket thread,
no `Arc<Mutex<...>>`) owns the id counter, the pending-callback map (`id -> {out_cb, exit_cb}`),
the Renderer's own `generation_id`, and an `mpsc::UnboundedSender<CommandEnvelope>`.
`ProcessRegistry::run` builds the full `CommandEnvelope` itself (capability `"process"`, action
`"run"`, `arguments: [cmd, args]`) rather than routing through an intermediate request type --
`CommandEnvelope` already is the wire shape, so this phase doesn't invent a second one just to
convert it back before sending. `process.run(cmd, args, out_cb, exit_cb)` is registered as a
Lua-callable closure typed `(String, Vec<String>, Function, Function)`; `mlua`'s own argument
type-checking on that signature is § 3.2's validation ("cmd is string, args array table of
strings, callbacks are Lua functions") in full -- no extra hand-written validation needed.
`ProcessHandle` (userdata) wraps `{ id, registry }`; `:kill()` sends a `"kill"` `CommandEnvelope`
carrying the same `id` and no arguments.

**Callback calling convention (a judgment call this codebase's spec docs don't pin down):**
`out_cb(line, stream)` where `stream` is the Lua string `"stdout"`/`"stderr"` (the wire type stays
a real `ProcessStream` enum -- Primitive Obsession is a Rust-side concern; Lua has no enums, and a
plain string is the natural, idiomatic shape for a Lua callback argument, matching how
`layer`/`monitor` etc. already cross the boundary as strings elsewhere in this IDL).
`exit_cb(code)` where `code` is a Lua integer or `nil`, `Option<i32>`'s natural `IntoLua` mapping
(§ 1.1's own table already specifies this for `Option<T>` generally).

**`renderer/src/socket.rs`**: a `process_outbound_tx`/`process_outbound_rx`
`mpsc::UnboundedChannel<CommandEnvelope>` pair created in `run()`; `ProcessRegistry` is
constructed there (with the Renderer's own `generation_id_from_env()` value) and registered onto
the loader via a new `Loader::register_process` method (mirroring `create_table`/`set_global`'s
existing "expose one piece of the private `Lua` for a caller outside `lua/mod.rs`" pattern).
`RendererClient` gains a `process_registry: lua::process::ProcessRegistry` field so
`dispatch_loop` can reach it from the read-half arm. `dispatch_loop` gains a fourth
channel argument, `process_outbound_rx`, drained in the same `tokio::select!` via the same
`write_json_frame` helper every other outbound frame already uses, and two new inbound match
arms: `SupervisorFrame::ProcessOutput` calls `process_registry.dispatch_output`;
`SupervisorFrame::ProcessExited` calls `process_registry.dispatch_exit` (which removes the
pending entry -- the callback pair's last use). Both silently ignore an unknown `id` (a stale
frame from a since-restarted generation, or a race no code path here actually produces today, but
not worth treating as a decode-level protocol desync the way an unparseable frame already is).

## Tested against

TDD, real seams, no mocks, matching this codebase's established discipline
(`supervisor/src/process/mod.rs`'s own existing tests, `renderer/src/lua/signal.rs`'s real-`mlua::Lua`
tests):

- `shared/src/lib.rs`: round-trip tests for `ProcessOutputLine`/`ProcessExited`/`ProcessStream`
  inside `SupervisorFrame`, matching every existing variant's test shape exactly.
- `supervisor/src/process/mod.rs`: `spawn_group_leader_piped` against a real `sh -c 'echo line1;
  echo line2 >&2; exit 3'` child, reading both piped streams and the real exit code back, plus a
  process-group-membership assertion mirroring `spawn_group_leader`'s own existing coverage (the
  piped variant must not have silently dropped the process-group behavior the whole primitive
  exists for).
- `supervisor/src/main.rs`: `process_run_args` (pure `CommandEnvelope.params.arguments` parsing)
  directly; `spawn_and_register_process`, `stream_process_output`, `kill_registered_process`,
  `reap_exited_process`, and `reap_generations_processes` each against real spawned `sh`
  processes and a real `HashMap`/`GenerationRegistry`, covering: successful spawn registers and
  returns piped handles; a nonexistent binary registers nothing; output lines from both streams
  arrive as the right `ProcessOutput` frames and completion fires once both close; kill reaps,
  removes, and reports a signal-death (`None`) code; a natural exit removed via the completion
  path reports its real numeric code; killing/reaping an unregistered id is a silent no-op; the
  supersede-time sweep reaps only the matching generation's entries, proven via `/proc`, matching
  `process::mod`'s own established verification style.
- `renderer/src/lua/process.rs`: against a real `mlua::Lua` VM (no network) -- `process.run`
  returns a `ProcessHandle` and queues a well-formed `"run"` `CommandEnvelope`;
  `process_handle:kill()` queues a `"kill"` envelope carrying the same `id`; `dispatch_output`
  invokes the registered `out_cb` with `(line, "stdout"/"stderr")`; `dispatch_exit` invokes
  `exit_cb` with the code (or `nil`) and then forgets the id, so a second `dispatch_exit` for the
  same id is a no-op; mlua's own type-checking rejects a non-function callback / non-string `cmd`
  argument.
- `renderer/src/socket.rs`: `dispatch_loop`'s new arms, matching this file's existing per-branch
  test style -- a queued outbound `CommandEnvelope` is written out over the wire as
  `RendererFrame::Command`; an inbound `ProcessOutput`/`ProcessExited` frame reaches the
  registered Lua callbacks (asserted by having the callback push into a Lua table the test reads
  back afterward, the same technique `rescue_state`'s probe script already uses).

Not automated, matching this codebase's own established ceiling for this
(ADR-0024/0025's precedent): a real Supervisor and Renderer as two separate processes actually
spawning a third `process.run`ed process together over a real `$XDG_RUNTIME_DIR` socket.

## Upgrade path, in order

(a) `textfield`'s scene node and `wp-text-input-v3` binding are the real prerequisites item 2
needs before `secure_submit` wiring is anything but speculative -- both still absent; (b) the PAM
crate spike + its own ADR unblocks item 3; (c) once either lands, this phase's `process` capability
dispatch in `main.rs` is the concrete precedent for wiring a second capability's actions into the
same `("capability", "action")` match, not a generic dispatcher framework built ahead of a second
real case (this phase deliberately doesn't build one, per its own "no speculative generality"
scope).

This does not contradict `docs/oblisk-supervisor-services-dbus.md` § 12, `docs/oblisk-idl-api-
specs.md` § 3.2/3.3, or ADR-0018: all describe the target shape this phase now delivers in full for
`process.run`/stream piping specifically, while leaving `textfield`/PAM exactly where ADR-0015 and
build-steps.md's own Phase 15 item 3 text already left them.

## Addendum (review fixes)

Two Correctness findings, both confirmed and fixed:

- **`stream_process_output`'s EOF-based completion signal doesn't mean the process has exited.** A
  process can close or redirect its own stdout/stderr while continuing to run (a daemonizing
  child, `exec 1>&- 2>&-`). The original `process_done` handler awaited `child.wait()` inline
  inside `main()`'s single top-level `select!`, so such a process would wedge the entire
  Supervisor -- every inbound command, every reload, every generation swap -- for as long as it
  kept running. Fixed by splitting `process_done`'s handler in two: `take_exited_process` (sync,
  removes the registry entry, safe inline) and `wait_and_report_exit` (the actual `wait()`,
  always run as a detached `tokio::spawn`ed task reporting back via a cloned
  `GenerationRegistry`, the same shape `stream_process_output` itself already uses).
- **Two paths never sent `ProcessExited`, leaking the Renderer-side pending callback pair
  forever**: a malformed `process.run` command (parse failure in `process_run_args`), and
  `KillOutcome::ReapFailed` (the registry entry is already removed before the reap attempt, so no
  later event could ever report this id done either). Both now send `ProcessExited { code: None }`
  -- an honest "never really ran" / "unknown outcome" signal, not a synthesized code.

One Standards finding, fixed: the claim "there is no `Arc<Mutex<...>>` anywhere in this codebase"
(both here and in `main.rs`'s doc comments) was false -- `socket::GenerationRegistry`'s own
`connections` map already is one, predating this phase. Reworded to the narrower, true claim:
no *spawn-tracking* state uses a shared mutex. Also renamed `main.rs`'s `ProcessRegistry` type
alias to `LiveProcesses` -- it collided in name (across crates, so it still compiled) with
`renderer/src/lua/process.rs`'s unrelated `ProcessRegistry` (the pending-callback registry).
