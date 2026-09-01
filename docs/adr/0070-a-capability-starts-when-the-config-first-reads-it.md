# A capability starts when the config first reads it

`run_supervisor` builds every capability controller before it reads the config, and the config is
never consulted about any of them. An `oblisk` process with a config that mentions nothing still
claims three session-wide roles (the polkit authentication agent, `org.freedesktop.Notifications`,
`org.kde.StatusNotifierWatcher`), opens the PipeWire connection, two niri IPC sockets and a
dedicated Wayland connection, subscribes to every BlueZ device and every NetworkManager device,
scans `/dev/video*` and every `.desktop` file on the machine, and wakes once a second forever.

Only `sysinfo` and `updates` wait for anything, and they wait for a poll interval rather than for
interest: their tasks spawn at boot and park on `Duration::ZERO`. That is not gating, it is a
missing default.

## Decision 1: reading `oblisk.<name>` is what starts `<name>`

The `oblisk` table no longer carries its capability members. They live in a side table, and
`oblisk`'s `__index` moves one across on first read, sending `RendererFrame::StartCapability` as it
goes. The second read finds the member on the table itself and the metamethod never fires again.

Reading is the right trigger because it is the only thing a config can do to a capability that it
cannot do to a capability it does not use. `oblisk.audio:invoke(...)`, `oblisk.audio:map(f)`,
`computed({ oblisk.audio }, f)` and `content = oblisk.audio` are all indexing `oblisk` first, so one
hook catches every spelling including the ones a future IDL adds.

The alternative was an explicit roster in `shell.lua`, a `capabilities { "audio", "network" }`
declaration beside the existing `fonts { }`. It is simpler to implement and it was rejected because
it is a second list to keep in sync with the first, and the failure mode is silence: a config that
reads `oblisk.network` without listing it gets a signal that stays `nil` forever, which looks
exactly like a machine with no Wi-Fi.

## Decision 2: starting is one-way

A capability that has started stays started for the life of the Supervisor process. A reload that
drops the last reader of `oblisk.bluetooth` does not stop the BlueZ subscription.

Stopping is where all the cost is. It means releasing a bus name that another process may have
taken in the meantime, draining requests already in flight, deciding what happens to a
`last_snapshots` entry and its revision counter, and answering what a `StateSnapshot` arriving after
the stop means. It buys back only what a config already had before the edit, which is what the
process does today for every capability unconditionally. YAGNI.

The consequence worth naming: a config that reads `oblisk.privacy` under an `if` that is true once
leaves the camera watch running until the session ends.

## Decision 3: a generation swap re-sends every start

`StartCapability` is idempotent on the Supervisor side -- a name whose controller exists is
logged and dropped. Each generation sends its own starts, because each generation is a separate
process with its own Lua VM and its own `__index`, and a candidate must not inherit the previous
generation's reads.

That also means a candidate reads a `nil` signal for the milliseconds between its first read and
the first `StateSnapshot` the newly-started controller pushes. That is not new: ADR-0037 already
seeds every capability to `nil` until its first push, and `last_snapshots` replays the current value
to a capability whose controller was already running.

## Decision 4: construction happens inline on the Supervisor's select loop

`NetworkController::new` makes a `GetAllDevices` call and binds every device it finds;
`BluetoothController::new` does the same against BlueZ. Awaiting that in the `StartCapability` arm
stalls the loop for its duration.

Accepted, because that is exactly what the Supervisor does today, in the same order, before the loop
starts. Moving it into the loop changes when it costs, not what it costs. The frames behind it are
delayed, not dropped.

ponytail: the ceiling is a config that reads eight capabilities in its first evaluation and pays for
all eight serially before any of them push. The upgrade path is to `tokio::spawn` the construction
and deliver the built controller back over a channel, which costs an `Option` transition per
capability that the inline form does not.

## Decision 5: `secure_submit` is a read too

polkit is not a capability. It has no roster entry, no `StateSnapshot` and no `oblisk.polkit`
member, so decision 1's hook cannot see it. What a config does declare is
`secure_submit = { capability = "polkit", action = "authenticate" }` on a `textfield`, which is
ADR-0005's rule that a secure submit targets a capability rather than Lua.

So every applied scene's `secure_submit_targets` are started by name, through the same deduplicating
sender. A config with a polkit prompt in it registers the agent; a config without one does not.

## Decision 6: failing to register the polkit agent is not fatal

`register_agent` was the fourth statement of `run_supervisor` and propagated with `?`. "An
authentication agent already exists for the given subject" is the normal answer on a machine running
any other desktop, and it stopped the shell from starting at all. `current_session_subject` was one
line earlier and equally fatal, on a `$XDG_SESSION_ID` that pam_systemd happens not to have set.

Both now log and continue, matching what `tray`, `notifications` and `mpris` already do when their
name is taken. The agent that loses the race does not get challenges, which is the whole of the
degradation.

## Decision 7: a config may declare no surfaces

`return {}` and an empty file were both refused, so "run nothing" was not a state this engine had,
and decision 1 could not be tested end to end. Zero surfaces is now a legal evaluation.

Nothing downstream needed changing, which is the evidence that the refusal was arbitrary:
`candidate_has_staged` is an `all` over an empty iterator, `run_pba`'s collection loop is
`while collected.len() < expected.len()`, and `expand_instances` over no specs yields no instances.
A zero-surface generation completes its PBA handshake immediately and sits on the socket waiting for
a reload, which is the correct behaviour for a config that declares nothing.
