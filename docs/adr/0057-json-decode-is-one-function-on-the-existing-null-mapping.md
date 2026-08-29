# `json.decode` is one function on the engine's existing null mapping

`process.run`'s `out_cb` hands Lua a string and Lua could not read it. That is what left every
subprocess reporting structured output unreachable from a config: `lsblk --json`, `busctl
--json=short`, `niri msg -j`, any HTTP fetch through `curl`. `build-steps.md` section 6 ranked
fixing it second, behind only a button index on `on_click`.

The § 3.3 banner recording the gap also proposed a fix, and the fix was wrong: "A pure-Lua decoder
is about a hundred lines and belongs in the config, not the engine." That was written without
checking whether the engine already had a JSON-to-Lua converter. It has had one since Phase 19 item
16, because every capability payload arriving from the Supervisor is a `serde_json::Value` that has
to become a Lua table before a `state` signal can hold it. `serde_json` is already a `renderer`
dependency for the control socket, and `mlua` already carries its `serde` feature. The decoder is
`serde_json::from_str` handed to a function that was already there and already tested.

## Decision 1: the decoder is in the engine, on the one existing mapping

`renderer/src/lua/json.rs` holds `to_lua`, which both a pushed capability payload and `json.decode`
go through. Nothing else in the engine converts JSON to Lua.

Sharing it is the whole point, not a tidiness argument. That function does not use mlua's serde
defaults: it turns off `serialize_none_to_null` and `serialize_unit_to_null`, so a JSON `null`
becomes an absent key rather than a lightuserdata sentinel. The sentinel is truthy, so with the
defaults `if payload.icon_path then` takes the branch that assumes a real value. A second decoder
built in Lua, or built in Rust against `lua.to_value`, would disagree with the first about `null`,
and a config author would meet one mapping on `oblisk.tray.items` and the other on the output of
`lsblk --json` with nothing in the language to tell them apart. The cost of the shared mapping
carries over too: a `null` array element leaves a hole and `ipairs` stops at it.

### Why not jq

It works, and for a single field it costs nothing to write. It also means a `sh -c` pipeline, so
config data gets quoted into a shell string; a second process per poll; a hard runtime dependency on
jq for a config that otherwise needs none; and jq itself written inside a Lua string, where a typo
surfaces as empty output rather than an error. The part that decides it is that `jq -r` still
returns *text*. Lua then splits on a delimiter, and the fields that break delimiter splitting are
exactly the ones a bar displays: window titles and track names containing tabs, newlines, and every
kind of dash. The config ends up parsing anyway, worse.

### Why not jql, or any other query crate

Wrong shape. Those evaluate a query language over JSON. Lua is already the query language here, and
what it needs is a table to index. A query crate would be a new dependency doing a subset of what
`serde_json` and `to_lua` already do together.

### Why not a pure-Lua decoder in the config

It is the option the § 3.3 banner argued for and it is the one that guarantees the `null`
disagreement in decision 1. It is also a hundred lines of hand-written parser per config, running
inside the Wayland dispatch thread, against untrusted subprocess output.

## Decision 2: failure is `nil` plus a message, not a raise

`cjson` raises. `io.open` returns `nil` plus a message. This follows `io.open`, because here a decode
failure is a routine path rather than an exceptional one. `out_cb` fires once per line with the
newline stripped, since `supervisor/src/process/registry.rs` reads the child through
`BufReader::lines()`. So a config either decodes a growing buffer on every line, or decodes at exit
whatever a failing subprocess printed to stdout instead of JSON. Both are ordinary. Raising would put
a `pcall` around every call site to handle the expected case.

The argument is an `mlua::LuaString` rather than a Rust `String` for the same reason: a subprocess
emitting a non-UTF-8 byte reaches serde as a decode error the config can read, instead of an mlua
argument-conversion error raised straight past its `if err then` check.

The convention has to hold on *every* failure or it is not one, since a config that must `pcall`
anyway has gained nothing. So a failure converting the parsed value into Lua is folded into the same
`nil`-plus-message pair rather than propagated, with wording that says which of the two happened,
because bad input is the config author's problem and a conversion failure is this engine's. The only
thing left that can raise is Lua failing to allocate the message string, which no config could handle
regardless.

## Decision 3: one return value on success, two on failure

Matching Lua's own idiom, where success and failure return different arities (`io.open` returns one
value, then three), rather than `dkjson`'s fixed three on both. A trailing `nil` on success looks
harmless and breaks the most obvious way to accumulate decoded values. With three arguments,
`table.insert` reads the second as a *position*, so `table.insert(t, decoded, nil)` raises "bad
argument #2 to 'insert' (number expected, got table)" and inserts nothing.

## Decision 4: there is no `json.encode`

Nothing needs it. A writer nobody calls is a writer nobody has checked round-trips. ponytail: a
config cannot build a JSON argument to hand a subprocess, so anything wanting `--json` *input*
concatenates strings by hand. Upgrade: an `encode` beside `decode`, through the same options in
reverse.

## Consequence: a bare top-level `null` is ambiguous

`json.decode("null")` succeeds and returns `nil`, which `if t then` reads as failure. Only the second
return tells the two apart, and it is absent on success. Both cases mean "no data", so the confusion
is harmless in practice, and the alternative fix is a sentinel for a successful null, which
reintroduces exactly the truthy-lightuserdata bug decision 1 exists to avoid.
`decode_of_a_bare_top_level_null_is_indistinguishable_from_a_decode_failure` pins it, and
`dev-config/oblisk/shell.lua` reads the second return rather than dropping it for this reason.
