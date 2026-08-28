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
