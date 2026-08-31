# A font file is mapped once and shared, not copied per reader

The Renderer showed 192MB of RSS on an idle eleven-surface session, and the question was what of
that is real and what can be given back.

## The measurement first

RSS is the wrong number and it is the one every system monitor shows. Broken down on this machine:

| | RSS | PSS | private dirty |
|---|---|---|---|
| Renderer, idle, 11 surfaces | 192.5 MB | 74.3 MB | 49.7 MB |
| `libLLVM.so.22.1` alone | 83.5 MB | 15.6 MB | 2.1 MB |
| `libgallium` alone | 47.0 MB | 7.2 MB | 1.6 MB |

Mesa's OpenGL driver is 130MB of the 192MB, and it is `Shared_Clean`: twelve other processes on
this machine map the same pages, niri and kitty among them. Unmapping it from the Renderer would
free no physical memory at all while the compositor is running. Private dirty, 49.7MB, is the only
number that names what this process actually costs, and it is what the rest of this ADR moves.

## Decision 1: the font chain is mapped, not copied

An 11MB `NotoColorEmoji.ttf` was costing 27MB of private dirty, because the bytes were held three
times over:

- the shaping worker's own `chain_bytes: Vec<Vec<u8>>`, built by `data.to_vec()` and kept for the
  life of the process to answer one request at startup,
- femtovg's copy, because `Canvas::add_font_mem` is `data.to_owned()` inside femtovg,
- cosmic-text's, which maps the file itself on first use.

`fontdb::Database::make_shared_face_data` maps the file, hands back an
`Arc<dyn AsRef<[u8]> + Send + Sync>`, and rewrites every face that came from that path to
`Source::SharedFile`. Calling it before the `Database` is handed to `FontSystem` is what collapses
all three: the worker holds the `Arc`, femtovg is given the same `Arc` through
`TextContext::add_shared_font_with_index`, and cosmic-text finds the mapping already in the
database rather than making its own.

Measured, same binary, same idle session, 11 surfaces:

| | RSS | PSS | private dirty |
|---|---|---|---|
| before | 192.5 MB | 74.3 MB | 49.7 MB |
| after | 166.3 MB | 46.1 MB | 26.4 MB |
| for reference: emoji font deleted from the chain entirely | 165.5 MB | 47.3 MB | 22.7 MB |

Private dirty roughly halves, and the third row is the point: the fix lands within 4MB of deleting
the feature, so there was never a reason to delete it. The fonts are now `r--s` mappings with zero
private dirty, and only the pages actually touched are resident: 2.1MB of the 11MB emoji file.

`add_shared_font_with_index` lives on `TextContext`, not on `Canvas`, so `TextPainter` now builds
the context first and passes it to `Canvas::new_with_text_context`. `Canvas::add_font_mem` is the
only font route the canvas itself exposes, and it is the copying one.

The `unsafe` is `make_shared_face_data`'s, and its documented hazard is a font file rewritten on
disk changing under the mapping. cosmic-text already takes that bargain internally for every font
it renders. The alternative is a private copy per font per process to defend against someone
editing a system font in place.

## Decision 2: not wgpu, and not a CPU rasterizer either

The obvious follow-up is that 130MB of Mesa is still mapped, and the driver stack is a choice. It
is worth writing down why it stays.

The Renderer is femtovg on OpenGL ES through `khronos-egl` and `glow`, not wgpu. Moving to wgpu
would mean Vulkan, and Vulkan genuinely is smaller here: `libvulkan_intel.so` is 25MB and links no
LLVM at all, because Intel's ANV compiles through NIR rather than through gallium. `vkcube` drawing
on this compositor measures 24.8MB RSS, 9.4MB PSS, 2.7MB private dirty, whole process.

It still is not worth it:

- Those 130MB are shared clean and resident for niri's sake regardless. Switching would move the
  number a system monitor prints without freeing a page.
- femtovg has no wgpu backend, so this is not a backend swap. It is replacing the 2D renderer
  (vello, or hand-rolled), and with it `layout::paint`'s execute half, the image upload path, the
  EGL surface binding in `wayland::surface`, and every pixel test that reads the framebuffer back.
- wgpu and naga add several MB of their own text.

Going the other way is worse, and this is the part that is not obvious. Dropping GL for `wl_shm`
plus `tiny-skia` (already in the tree under resvg) would unmap Mesa entirely, and ADR-0063 made the
wallpaper repaint zero times a second so the rasterization cost would barely show. But a
1920x1200 shm buffer is 9.2MB of *our* private dirty, 18.4MB double-buffered, for the wallpaper
alone. Today those pixels live in GPU buffer objects: the twenty `anon_inode:i915.gem` mappings in
the Renderer's `smaps` are all `Rss=0`, costing this process nothing. Trading 130MB of shared clean
for 18MB of private dirty makes RSS look better and makes the machine worse.

So the stack stays. The remaining private dirty is 13.7MB of heap (the Lua VM, eleven resolved
trees, the display lists, femtovg's and cosmic-text's caches), 2.9MB of anon, and 3.8MB of
relocations in Mesa's own libraries. For scale, niri's own private dirty on the same session is
37.8MB.

## What this does not fix

`VmSize` is 768MB and looks alarming. It is six glibc per-thread arenas, each a 64MB `PROT_NONE`
reservation with about 132KB actually mprotected, plus Mesa's reservations. Address space is free
on 64-bit. `MALLOC_ARENA_MAX` would shrink the number and buy no memory, so it is not set.

`"Noto Sans CJK JP"` in `fonts::DEFAULT_CHAIN` resolves to nothing on this machine and is skipped,
which the startup log says plainly. That is a missing package, not a bug here, but it does mean the
chain currently has no CJK coverage and the emoji font is doing the whole fallback job.
