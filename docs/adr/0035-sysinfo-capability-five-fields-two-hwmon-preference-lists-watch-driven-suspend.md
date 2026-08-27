# ADR-0035: `sysinfo` capability — IDL's five fields over the prose specs' three, two hwmon preference lists, watch-channel-driven suspend

## Status

Accepted

## Context

`docs/oblisk-supervisor-services-dbus.md` §11 and `docs/oblisk-hardware-event-pipeline.md`
§7 both describe `sysinfo` loosely: "CPU%, RAM usage, and hardware temperature," three
Lua-configurable intervals, suspend the matching background task at `interval=0`. Neither
names exact fields. `docs/oblisk-idl-api-specs.md` §2.12 — the actual wire-format IDL — is
more specific and, on temperature, disagrees with the natural reading of the prose specs:
it declares `temp_cores` as a per-core array plus a separate `temp_gpu` scalar, not one
"the" temperature value. `docs/oblisk-reference-fixtures.md` §8 has a real fixture widget
built against exactly that shape (`list { source = bind(sysinfo.temp_cores), itemfn =
function(temp, index) ... } }`), confirming the IDL over the prose. This ADR treats the
IDL as authoritative wherever it's more specific than the two prose docs, the same
precedent ADR-0033 already established for notifications.

§11/§7 are unbuilt (ADR-0034 says so explicitly while scoping updates as a separate
service). This is greenfield: no prior design, no prior ADR.

## Decisions

**Five fields, three intervals — `swap_percent` rides `ram_interval`, `temp_gpu` rides
`temp_interval`.** The IDL declares `cpu_percent`, `ram_percent`, `swap_percent`,
`temp_cores`, `temp_gpu` but `configure({cpu_interval, ram_interval, temp_interval})` only
three knobs. `swap_percent` is computed alongside `ram_percent` from the same
`/proc/meminfo` read on the RAM task's tick; `temp_gpu` is read alongside `temp_cores` from
the same hwmon scan on the temp task's tick. Three tasks, not five — no field gets an
interval the IDL never gave it a way to configure.

**CPU%**: `/proc/stat`'s first line, standard 10-field layout (`user nice system idle
iowait irq softirq steal guest guest_nice`). `percent = 100 * busy_delta / total_delta`,
`busy = total - (idle + iowait)` — the standard `top`/`htop` convention. The task keeps its
previous sample in its own loop-local state across ticks; the configured interval *is* the
sampling window, no extra intra-tick sleep. First tick after a cold start computes nothing
and only stores the sample. The task also discards its previous sample the instant it goes
dormant (`interval` set back to `0`), not only at cold start: `/proc/stat`'s counters are
cumulative since boot, so a delta against a sample from before an arbitrarily long dormant
gap is mathematically valid as "average busy% across that whole gap" — but that's not what
a freshly-resumed live gauge should report as its first value. The first implementation
kept the stale sample across a suspend and got exactly this wrong (caught in code review,
not before): it silently published an hours-old averaged reading as the first `cpu_percent`
after resuming. The same cold-start treatment now applies on every dormant→ticking
transition, not just the very first one.

**RAM/swap**: `MemTotal`/`MemAvailable` (`used = total - available`, `percent = 100 *
used/total`) — no hand-rolled Buffers/Cached estimate; `MemAvailable` already is that
estimate, reinventing it would be second-guessing the kernel for no gain. `SwapTotal`/
`SwapFree` the same way.

**Temperature: two independent hwmon chip-name preference lists, one shared directory
scan, resolved once at controller construction.** `["k10temp", "coretemp"]` for
`temp_cores`, `["amdgpu", "nouveau", "nvidia"]` for `temp_gpu`. Chips don't hotplug for
onboard sensors, so resolution happens once (mirrors `IdleController`'s degrade-once-at-
construction precedent) rather than re-scanning every tick. `temp_cores` is every
`tempN_input` on the winning CPU chip whose label matches `Core \d+`, sorted by trailing
core index, excluding the package-level aggregate sensor (`temp1_input`/"Package id 0" on
`coretemp`) — falls back to `acpitz`'s single sensor as a one-element array if neither
`k10temp` nor `coretemp` is present, since `acpitz` is the generic ACPI thermal zone every
machine has. `temp_gpu` is the winning GPU chip's primary sensor, or the IDL's own `-1`
sentinel if none of the three names match — verified live on the dev machine this was
designed against, which has no discrete GPU hwmon chip at all (integrated graphics, no
separate thermal zone): a real negative-path test, not a mocked one. Wifi (`mt7921_phy0`),
NVMe, and battery hwmon chips seen on that same machine are deliberately excluded from
both lists — neither preference list names them, and nothing in the spec or IDL calls for
exposing them.

**Suspend at `interval=0` is a real dormant-`await`, not a polling no-op.** One long-lived
tokio task per interval (three total), each driven by a `tokio::sync::watch<Duration>`
holding its current interval, updated by `configure`. At `Duration::ZERO` the task's loop
awaits only `watch.changed()` — no `tokio::time::interval` armed at all, zero wakeups.
Nonzero races `interval.tick()` against `watch.changed()`. All three start at
`Duration::ZERO`; nothing polls until Lua calls `configure` at least once — matches every
other opt-in mechanism already in this codebase (idle thresholds, DND, notification
sounds: nothing fires until asked). This makes "genuinely suspended" a real branch in the
code, not a timing assumption resting on `tokio::time::interval`'s own missed-tick
behavior.

**One capability, three producers, one shared `Arc<Mutex<SysinfoState>>`.** First capability
in this codebase where more than one independent task writes into the same capability's
payload — every other Phase 16 controller has exactly one poll/event cadence feeding its
`StateSnapshot`. Each task updates only its own fields under the lock on its own tick, then
signals one shared unbounded `mpsc<()>`; `main.rs`'s `select!` gains one arm that locks,
clones the combined state, and pushes — same "`build_state()` snapshot of already-live
data" shape `tray`/`notifications` already use, just with three writers instead of one.
`revision` bumps once per push regardless of which field(s) actually changed, matching
`bump_revision`'s existing per-capability (not per-field) granularity — nothing about three
producers changes what a revision means. Pre-first-sample fields default to `0` for the
three percent fields (no second sentinel convention needed alongside `temp_gpu`'s IDL-
mandated `-1`); no `StateSnapshot` is pushed at all until at least one field has a real
value, matching every capability's existing "doesn't exist in Lua until its first push"
behavior.

**`configure(cfg)` takes a table, the first capability action in this codebase to do so.**
Every existing `parse_*_args` reads positional `arguments[N]`; here `arguments[0]` is a
JSON object (the same mlua table → `serde_json::Value` object conversion `StateSnapshot`
payloads already rely on in reverse). `parse_configure_args` returns `Option<{cpu_interval:
Option<u64>, ram_interval: Option<u64>, temp_interval: Option<u64>}>` — a present key
overrides that task's watch value, an absent key leaves it unchanged, letting Lua touch one
interval without restating the other two. A wrong-typed present key drops the whole call
with one `eprintln!`, matching every existing malformed-shape handling — no partial-apply.
Units are whole seconds, matching `idle:register`'s existing convention; the IDL confirms
this explicitly for `sysinfo` too.

**Config is Supervisor-global, not renderer-generation-scoped.** Same category as
network/bluetooth/tray (one shared state, no per-generation cleanup hook needed on reload
or crash) — unlike idle's thresholds/inhibit, which are genuinely tied to which generation
registered them.

**Sysfs/procfs paths are parameters, never hardcoded** (`docs/oblisk-tdd-test-harness.md`'s
explicit mandate). `cpu.rs`/`ram.rs` read functions take `proc_root: &Path` (real default
`"/proc"`); `temp.rs`'s chip-resolution and read functions take `hwmon_root: &Path` (real
default `"/sys/class/hwmon"`). Tests build real fake-root trees under
`tempfile::tempdir()` and point the functions at them — real filesystem I/O against a fake
root, not string-literal mocking. This is a different testing convention from
`audio::mixer`'s `PropsLookup`-trait-over-a-`HashMap` pattern (which suits a single
property dict, not a directory tree that needs real `read_dir` traversal), applied here
for the first time because sysfs/procfs specifically is a doc-mandated exception.

**Module layout: `supervisor/src/hardware/sysinfo/{cpu,ram,temp,controller}.rs`**, mirroring
`hardware/idle/{notify,inhibit,controller}.rs`'s split-by-concern precedent. No new
cross-controller "adapter" trait — ADR-0034 already rejected one for this exact reason and
nothing here forces the question again.

## Consequences

- `docs/oblisk-supervisor-services-dbus.md` §11 and `docs/oblisk-hardware-event-pipeline.md`
  §7 both undersell the real field count (three metrics in prose vs. five fields in the
  IDL) — a doc-sync task, not a blocker, same posture ADR-0032 took for idle's write
  actions.
- `temp_gpu` reading `-1` is the expected, documented outcome on any machine without a
  matching hwmon chip (most laptops with integrated-only graphics) — not a bug to chase or
  a reason to widen the preference list speculatively.
- A future fourth or fifth independently-configurable metric on this capability has real
  precedent to extend (add a task, add fields to `SysinfoState`, reuse the one shared
  signal channel) instead of inventing a new multi-producer shape from scratch.

## Upgrade path

- A wider GPU chip preference list (e.g. `i915`/`xe` for Intel integrated graphics thermal
  zones, if one ever reports through hwmon on a real tested machine) — one line to add, not
  a redesign.
- Per-core `temp_cores` labels beyond the current "Core N" match, if a chip's labeling
  scheme diverges from `coretemp`'s/`k10temp`'s — the regex/prefix match is the one place
  that would need to grow.
