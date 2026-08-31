# The icon path lookup is the paint loop, not the GPU

ADR-0063 stopped surfaces repainting when nothing changed. This is about what a repaint that
*does* happen costs, and the answer was not where the GL calls are.

## The measurement

`paint_surface` instrumented in phases, release build, idle eleven-surface session, 30 second
windows. The numbers below are wall time summed over the window, so a share of one core.

| phase | time | share |
|---|---|---|
| `Scene::surface` (deep-clones the tree) | 1.6 ms | 0.00% |
| `paint::build` (flattens to a display list) | 1.4 ms | 0.00% |
| `eglMakeCurrent` | 4.1 ms | 0.01% |
| `glClear` | 19.9 ms | 0.06% |
| recording draws into femtovg | 165.1 ms | 0.55% |
| `Canvas::flush` (all the actual GL submission) | 8.3 ms | 0.03% |
| `eglSwapBuffers` | 11.9 ms | 0.04% |

Three things that looked like the obvious suspects are not:

- The deep clone per surface per repaint costs 1.6ms across a whole 30 second window. ADR-0063
  listed it as the next thing to measure. It has now been measured and it is nothing.
- `eglSwapBuffers` does not block. No `eglSwapInterval` call is made anywhere, so the default of
  1 is in force, and the worry was that eleven surfaces would serialize behind vsync. They do not:
  swap is 0.04% of a core.
- The GL submission is 8.3ms against 165.1ms spent recording the commands. Nothing here is
  GPU-bound or driver-bound. It is CPU work in the paint loop.

Splitting the recording by draw kind found it:

| kind | count | total | each |
|---|---|---|---|
| box | 1150 | 1.9 ms | 1.7 µs |
| text | 713 | 8.7 ms | 12.2 µs |
| **icon** | **62** | **102.0 ms** | **1645 µs** |
| image | 3 | 43.9 ms | 14638 µs |

## Decision: memoize the name-to-path resolution

Of the icon's 1645µs, 1638µs was `icons::resolve` and 7µs was drawing. The texture was already
cached: `ImageCache` measured 32 hits against 2 misses over the same window. What ran every frame
for every icon was the *lookup that produces the cache key*, a `freedesktop-icons` search across
the active theme and its whole inheritance chain. Mean 1.6ms, worst single call 5.5ms.

`resolve` now memoizes `(size, name) -> Option<PathBuf>` for the life of the process. A `None` is
memoized as carefully as a hit, because "not found" is the expensive answer: it means the entire
inheritance chain was walked and every candidate stat'd.

Process lifetime is the correct scope and it is borrowed, not invented. `theme()` is already a
`OnceLock` read once per process, with its own doc comment saying an icon theme change appears at
the next reload rather than immediately. What `resolve` maps cannot change without the theme
changing, so the memo is exactly as stale as the theme it is keyed against, and the generation swap
(docs/adr/0054) is what clears both.

`with_cache` stays on the `freedesktop-icons` call. It caches parsed theme *indexes*, which is what
keeps a first lookup from reading every `index.theme` under `/usr/share/icons`. It does not cache
the per-name search that uses them, which is the part measured above.

Measured after, same harness and config:

| | before | after |
|---|---|---|
| per icon command | 1645 µs | 79 µs |
| recording draws | 165.1 ms | 39.1 ms |
| bar, per repaint | 3.9 ms | 0.57 ms |
| whole GL phase | 0.64% of a core | 0.19% |

## What was deliberately not changed

The image at 14.6ms a command is three calls, and the `ImageCache` counters say two misses in the
window. The dev config's wallpaper is an SVG, so a miss is resvg rasterizing it at 1920px. That is
a startup cost paid once per generation, already named in `ImageCache::image`'s own ponytail note,
and the mean over three calls is what makes it look like a per-frame one.

The `stat` per image per frame stays. `ImageCache`'s key carries the file's revision so an edited
icon appears without a reload, and its doc comment already argues the case: the alternative to
asking whether the bytes changed is re-reading them to find out. It does not show up against a 79µs
icon.

`eglSwapInterval(0)` is not set. There is nothing to win: swap measured 0.04% of a core, and
turning vsync off on a shell that repaints roughly once a second would trade nothing for tearing.

The per-surface deep clone stays too, for the plainest possible reason. It is 0.00%.
