# Memory budget: declared fonts, atlas eviction, and PSS as the measurement

"Achieve Noctalia's numbers" was set as a goal on 2026-08-28. That needs a real target, an honest
measurement, and a decision on the two things in this stack that would blow it.

## What Noctalia actually published

Worth stating before adopting it as a target. The v5 announcement gives one qualitative claim: the
old Qt/QML stack ate "roughly 300 MB of RAM per monitor" and v5 is "down to about one-sixth of
that", implying roughly 50 MB per monitor. No measurement method, no tool, no distinction between
RSS and PSS. An independent writeup reports whole-session figures instead (roughly 165 MB idle,
roughly 500 MB cold boot, roughly 900 MB while screen recording) covering compositor plus shell plus
everything else, again with no stated method.

So the number is a direction, not a benchmark. Inheriting it verbatim as a pass/fail threshold would
be measuring Oblisk against a figure nobody can reproduce.

**Target: 50 MB PSS per monitor for the shell's own processes at steady state**, measured as below.
Adopted as Oblisk's own falsifiable budget, matching the order of magnitude Noctalia claims, and
owned here rather than borrowed.

## Decision 1: measure PSS, not RSS, and report three numbers

Oblisk is a supervisor process plus one renderer process per generation, briefly two renderers
during a PBA handoff. Summing RSS across them double-counts every shared page: the shared libc, the
GPU driver's shared objects, memory-mapped font files, and (during a handoff) two renderers running
the same binary. A naive sum overstates real pressure and makes the handoff look like 2x when it is
not.

1. **Steady state.** Sum PSS across supervisor and renderer via `/proc/[pid]/smaps_rollup`. This is
   the number compared against the 50 MB per monitor budget. `smaps_rollup` gives the aggregate
   without parsing every mapping, which is cheap enough to sample on a timer.
2. **Per-renderer USS** (`Private_Clean + Private_Dirty`). What one generation uniquely costs,
   independent of pages amortized across processes.
3. **Handoff peak**, sampled specifically in the window where two renderers are alive. Reported
   separately from steady state, never folded into it. The two renderers share their binary, their
   libraries, and most font mappings, so this is well under 2x and should be stated rather than
   assumed.

GPU memory is not in any of these. EGL and dmabuf buffers are usually GEM objects in the driver's
accounting, invisible to `VmRSS`, though on an integrated GPU they are the same physical RAM. Read
them from DRM fdinfo (`/proc/[pid]/fdinfo/*`, `drm-*-memory` fields), not from `smaps`. A software
rasterizer inverts this: under llvmpipe those buffers are ordinary heap and do land in RSS, so a CI
number and a real-GPU number are not comparable.

## Decision 2: fonts are declared in config, not discovered from the system

This is the largest risk to the budget and the one with the clearest fix.

`cosmic_text::FontSystem::new()` calls `Database::load_system_fonts()`, which eagerly parses face
metadata for every font on the system. The file bytes are memory-mapped rather than read (`fontdb`
uses `Source::File`), so glyph outlines page in lazily, but the metadata parse is unconditional and
covers the full system set, commonly over a thousand faces. This is already visible in this
codebase: ADR-0023 item 8 recorded `FontSystem::new()` costing roughly one second, and cosmic-text's
own docs put it at "up to a second". Most of that is cold-cache disk I/O over fonts the shell will
never draw with.

There is no API to scope `load_system_fonts()` to chosen families. The levers are to prune
`db_mut()` afterwards, which pays the scan anyway, or to skip it and load specific files through
`load_font_file` / `load_fonts_dir`.

**Decision: skip `load_system_fonts()`.** The config declares its fonts, and the renderer loads
those families plus a declared fallback chain. A shell draws its own interface and knows its own
typefaces; discovering a thousand faces to use four is the wrong default for a process that wants to
fit in 50 MB.

```lua
fonts = {
    ui       = "Inter",
    mono     = "JetBrains Mono",
    fallback = { "Noto Sans CJK JP", "Noto Color Emoji" },
}
```

The honest cost: a codepoint with no glyph in any declared family renders as tofu. That is why the
`fallback` list exists and why the default config ships a chain covering CJK and emoji, which is
where arbitrary text actually arrives from (MPRIS track titles, notification bodies, window titles).
Roughly ten to twenty faces instead of the full system set.

Upgrade path if tofu turns out to matter in practice: build the system database lazily on a shaping
miss rather than at startup, keeping the fast path unchanged. Do not build that until a real config
hits the limit; the declared chain is a complete answer for every script someone thought to declare.

`fontdb`'s persistent cache (`new_cached()`) is the fallback if a full system database is ever
genuinely wanted again. It addresses the one-second startup, not the memory.

## Decision 3: the glyph atlas needs eviction, because femtovg has none

femtovg's atlas pages are 512x512 RGBA8, one mebibyte each. It allocates a new page whenever no
existing page has room, holds them in a `Vec` that grows without bound, and frees them only when
something explicitly calls `clear()`. There is no LRU and no automatic eviction. A shell running for
weeks, rendering many distinct glyph, size, and style combinations (a clock at one size, notification
bodies at another, a media title at a third), accretes pages permanently.

Note this also corrects build-steps.md Phase 4's "2048x2048" atlas language, which ADR-0012 already
found had no public API behind it. The real geometry is 512x512 pages, and the real gap is eviction,
not page size.

**Decision: clear the atlas when it exceeds a page-count threshold and the shell is idle**, rebuilding
on demand. Mark it with a `ponytail:` comment naming the ceiling: this is a whole-cache drop, not an
LRU, chosen because femtovg exposes no per-glyph eviction and because a full rebuild on an idle
frame is invisible. If the rebuild is ever observed as a hitch, an LRU needs femtovg-side support
that does not exist today and would mean forking it.

This is GPU memory, so it will not show up in the PSS number under a real driver. It still counts
against the same physical RAM on integrated graphics, which is most target hardware.

## Decision 4: per-surface buffers are why the dynamic surface model helps here

One RGBA8 buffer at 2560x1440 is about 14 MiB; double or triple buffered, 28 to 42 MiB per surface.
That is the dominant graphics cost and it scales with surface area, not with surface count.

The model ADR-0038 and ADR-0040 replaced was worse for this budget, not better. A permanently mapped
fullscreen `overlay_canvas` holds a fullscreen buffer for the entire session so that popups have
somewhere to draw. Per-popup `xdg_popup` surfaces allocate their own size and only while open, and
tightly sized panels allocate a bar-sized buffer rather than a screen-sized one. The generality
decision and the memory target point the same way, which is worth recording because they look
opposed.

## Non-goal: optimizing before measuring

No allocator swap, no arena, no `jemalloc`, no `Box`-shrinking pass until the measurement in decision
1 exists and says where the memory is. Decisions 2 and 3 are in this ADR because each has a
documented mechanism and a known unbounded growth path, not because a profile pointed at them. Every
other suspicion waits for a number.

The measurement harness itself is small: read `smaps_rollup` for the supervisor and each live
renderer, sum PSS, and record USS per renderer. It has no dependency on the paint pass and can be
built at any point.

## Amendment: what decision 1 got wrong about DRM fdinfo, and what the harness cannot divide by

Phase 24 built decision 1's harness and checked it against the machine it runs on rather than
against the kernel documentation alone. Three corrections, none of which change the decision.

**The field name is wrong.** Decision 1 says to read `drm-*-memory` fields. No such field exists on
this hardware. i915 exposes `drm-total-<region>`, `drm-shared-<region>`, `drm-resident-<region>`,
`drm-purgeable-<region>` and `drm-active-<region>`, with `<region>` being `system0` and
`stolen-system0` here. `drm-memory-<region>` is the older amdgpu-era naming that the current
`drm-usage-stats` documentation supersedes. The harness reads `drm-resident-<region>`, summed across
regions because regions are disjoint memory, and falls back to `drm-memory-<region>` only when no
resident field is present at all. `drm-total-<region>` is deliberately not the number reported: it
counts buffers that are not resident, which is not physical RAM and not what a 50 MB budget is
about.

**One client is many fds, and summing them triple-counts.** A DRM client holding several fds repeats
the same byte counts in every one of that process's fdinfo files, all carrying the same
`drm-client-id`. Reading niri on this machine, three fds each report an identical 279968 KiB
resident. A per-fd sum would have reported roughly 820 MiB of GPU memory for a compositor using
about 273 MiB. The harness keys on `(drm-pdev, drm-client-id)` and counts each client once, and
reports the surviving client count alongside the bytes so the deduplication is visible rather than
assumed.

**The budget is per monitor and the Supervisor cannot count monitors.** ADR-0041 makes `screens`
Renderer-sourced, and no output count crosses the socket in the other direction. So the harness
reports absolute totals and leaves the division to whoever reads the line. Adding a wire field just
to divide by it would put a number in the protocol that only a log line consumes.

## Amendment: the handoff sample is always on, the steady-state sample is opt-in

Decision 1 asks for three numbers but not for when to take them, and the two kinds are not equally
available from outside the process.

Steady state is samplable by anyone at any time: the shell is running, the pids are in `/proc`, and
an outside tool reads the same `smaps_rollup` this harness does. Building that into the Supervisor
buys a log line, not a capability, so it stays behind `OBLISK_MEMORY_SAMPLE_SECS` and prints
nothing when unset.

The handoff peak is the opposite. It exists only while two Renderers are alive, which happens only
during a swap, lasts about as long as PBA's evidence round trip, and starts at a moment no outside
sampler can predict. Nothing but the code holding both `Child` handles can catch it. So that sample
is unconditional, taken once at the widest point of the window: after `run_pba` returns `Ok` and the
Candidate has presented evidence, before the superseded generation is reaped. A swap is rare enough
that one line per config edit is not noise.

This is also the honest ceiling on the handoff number. It is one sample at one instant, not a peak
tracked across the window, so it undercounts if the true peak lands during evidence collection
instead. `ponytail:` upgrade path if that gap ever matters: sample on a short timer for the duration
of the window and keep the maximum. Not built, because the number's purpose is to show that two
Renderers cost well under 2x, and a single sample at the widest point already answers that.

## Amendment: the first reading, and what it cost to check decision 2

The non-goal section of this ADR says every suspicion waits for a number. Phase 24 produced the
first ones, from a release build on this machine (niri, one 1920x1200 output, i915, Mesa 26.2.1):

```
[oblisk-memory] steady state: total pss 149.7 MiB; supervisor pss 12.6 MiB; generation 0 pss 137.0 MiB uss 130.4 MiB gpu 14.3 MiB (0.6 MiB shared, 1 drm client(s))
[oblisk-memory] pba handoff:  total pss 196.9 MiB; supervisor pss 12.6 MiB; generation 0 pss 92.2 MiB uss 42.6 MiB gpu 14.3 MiB (0.6 MiB shared, 1 drm client(s)); generation 1 pss 92.1 MiB uss 42.4 MiB gpu 14.5 MiB (0.4 MiB shared, 1 drm client(s))
```

**149.7 MiB against a 50 MB budget, on one monitor.** Roughly three times over, and not for any
reason this ADR suspected. Attributing the Renderer's PSS by mapping:

| PSS | Mapping |
| --- | --- |
| 82.3 MiB | `/usr/lib/libLLVM.so.22.1` |
| 25.0 MiB | anonymous |
| 13.3 MiB | `/usr/lib/libgallium-26.2.1-arch1.1.so` |
| 12.4 MiB | heap |
| 4.2 MiB | the `renderer` binary itself |
| 0.1 MiB | every mapped font, together |

libLLVM is 59% of the Renderer's PSS and 55% of the whole shell's. It arrives as a dependency of
Mesa's gallium megadriver, which contains llvmpipe whether or not llvmpipe is in use, and it is not
in use here: the Renderer's DRM fdinfo reports `drm-driver: i915`, so this is hardware rendering
paying to load a software rasterizer's compiler. Roughly 85 MiB of that library becomes resident.
The mechanism is not confirmed and this ADR does not guess at one.

**Decision 2 was right, and this ADR understated it by four orders of magnitude.** The 0.1 MiB of
fonts above is not evidence against decision 2; it is decision 2 already working. Phase 19 item 10
landed before this measurement, so `renderer/src/text/fonts.rs` resolves the declared chain through
`fc-match` and loads two files. Reading that number as a verdict on `load_system_fonts()` would have
been measuring the fix and calling it the bug.

So the harness was pointed at the path that still calls it. `system_fallback` is the last resort
taken when fontconfig is unreachable, and forcing it (running with `fc-match` off `$PATH`) is a real
before-and-after on this machine's 2648 faces:

| Renderer PSS | Font loading |
| --- | --- |
| 2207.9 MiB | `load_system_fonts()`, the whole system set |
| 137.0 MiB | the declared chain, two files |

**Two point one gibibytes.** Decision 2 argued from a one-second startup cost and called system font
loading the largest risk to the budget; it is a 16x multiplier on the entire shell. The cost also
exceeds the 1.1 GB of font files on disk, which rules out the memory-mapping this ADR assumed when
it wrote that "the file bytes are memory-mapped rather than read (`fontdb` uses `Source::File`)".
`fontdb` 0.23 is loading file contents, not mapping them, and the parsed metadata is on top.

That leaves a live hazard this measurement found rather than created. `system_fallback` is shipped
code on a path any machine without fontconfig takes, and it is now known to cost 2.2 GB rather than
the "slow" its own comment claims. Its `ponytail:` comment says it is correct only because it is the
last resort. That is a weaker defence against 2.2 GB than against a slow scan, and bounding it is
Phase 19's to weigh, not this ADR's to decide.

Decision 3's atlas eviction is untouched by these readings. femtovg's unbounded page growth is a leak
over days and invisible to a sample taken seconds after boot. It needs a shell that has been running
for a long time, which is the measurement to take next.

**The handoff number vindicates PSS.** 196.9 MiB for two Renderers against 149.7 for one is 1.32x,
not 2x, and the per-Renderer numbers show why: each generation's USS fell from 130.4 MiB to 42.6 MiB
the moment the second one existed. That is roughly 88 MiB per Renderer moving from private to shared
in one step, because two processes running the same binary page in the same clean file pages of the
same Mesa libraries. A naive RSS sum would have reported about 274 MiB. Decision 1's first paragraph
predicted this effect; the measurement gives its size.

**The open question this hands back.** If 55% of the number is a driver library any GL client on the
system maps, the 50 MB target is measuring the graphics stack as much as the shell. Oblisk's own
pages (its binary, heap, anonymous memory and fonts) come to roughly 41.7 MiB in the Renderer plus
12.6 MiB in the Supervisor. Whether the budget should exclude the driver, and whether Noctalia's
claimed figure ever included it, is a question this amendment raises and deliberately does not
settle: the target in this ADR stands until someone changes it with a reason.
