---
name: diagnosing-bugs
description: Diagnosis loop for hard Obelisk bugs and performance regressions, in the Rust framework or the Lua shell.
---

# Diagnosing bugs

Stop guessing. Build a loop, hypothesize, measure, fix.

## 0. Read and redact
- Place the bug: framework (Rust) or shell (`dev-config` Lua). A framework bug reproduces without `dev-config`.
- Read `CONTEXT.md` and the ADRs for the area.
- Redact secrets. Quote only the log lines carrying the signal.

## 1. Build a feedback loop
No signal that goes red on this bug, no diagnosis. Cheapest first:

1. A failing `cargo test -p <crate> <filter>` at the seam.
2. `obelisk check -c <dir>`: evaluates a config with no display.
3. The live session: `cargo build --workspace --release`, stop the running `obelisk`, copy both binaries over
   `$CARGOBIN`, restart with `obelisk -d`, read `obelisk log -f`.
4. Probes: `OBELISK_DUMP_LAYOUT=<surface@output>` for geometry, `WAYLAND_DEBUG=1` for protocol traffic,
   `busctl`/`dbus-monitor` for backends, `obelisk call <action>` for Lua state.
5. Only a human can see it (a frame, a flicker): numbered steps for the user, one y/n question each.

Done when one command reproduces the exact symptom, in seconds, every time.

## 2. Reproduce and minimize
Confirm it is the user's exact symptom. Cut inputs, callers, nodes and config one at a time; keep only what
the failure needs.

## 3. Hypothesize
Show the user 3-5 ranked, falsifiable hypotheses: "If X causes it, changing Y turns it green."

## 4. Instrument
One variable at a time. Tag temporary `eprintln!`/`print()` probes with `[DEBUG]`.

## 5. Fix
The minimized repro becomes a permanent test with an inline fixture, never `dev-config`. Watch it fail, fix,
watch it pass.

## 6. Cleanup
- [ ] Loop and regression test green.
- [ ] `git grep DEBUG` empty.
- [ ] Commit message names the confirmed hypothesis.
