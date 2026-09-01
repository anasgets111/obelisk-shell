# The GL context is built by the first surface that needs it

`run` called `egl::init` as its ninth statement, seventy lines before `run_startup_evaluation`
reads `shell.lua`. So the Renderer built an EGL display, config and GLES3 context before it knew
whether the config declared any surface to draw into.

`eglInitialize` is the call that makes Mesa load its driver. Measured on an Iris Xe laptop, warm
page cache, debug build:

| | |
| --- | --- |
| `egl::init` | 12.9 - 34.7 ms, median 26 |
| `libgallium` + the LLVM it links | 125 MB resident |
| first `glow` context in `ensure_bound` | 1.7 - 3.2 ms |
| a Candidate's time to `ReadySignal` | 104 ms, against a 2000 ms `ready_timeout` |

A config that declares no surfaces is legal (docs/adr/0070 decision 7). Before this change it
still paid all of that. The Renderer for `return {}` was 151 MB resident.

## Decision 1: `App::egl` is an `Option`, built on the first bind

`ensure_bound` calls `ensure_egl` after its two cheap bails and before `WlEglSurface::new`.
`ensure_egl` is the only caller of `egl::init`, which is what makes the whole Mesa load conditional
on a surface existing.

`App` holds the `Connection` rather than the raw `wl_display` pointer it passes on. Same pointer,
but the refcount is then what guarantees `egl::init`'s SAFETY precondition instead of a comment
promising the connection outlives the state built from it.

Everything downstream already ran only when a surface was bound, so the `Option` does not spread:
`release_bound` and `paint_surface` reach `egl` through a local, and both are unreachable for a
surface that never bound.

## Decision 2: a Candidate reaches ready without a GL context

This was the surprise. `bind_and_clear` short-circuits on `self.is_pba_candidate ||
!self.ensure_bound(index)`, because § 15.2 point 3 keeps a Candidate invisible until `ActivateDraw`
and `activate_draw_one` is its only bind. A Candidate therefore never called `ensure_bound` before
signalling ready, and every millisecond `egl::init` spent was spent inside the ready window
building a context the Candidate was forbidden from using.

Deferring the call takes that time out of the window rather than putting it in. Verified: a
Candidate now holds ready with `libgallium` unmapped.

The cost lands after `ActivateDraw` instead, where the first bind grows from about 2 ms to about
30 ms. That is under two frames at 60 Hz on the reload path only.

## Decision 3: an EGL failure is fatal later than it used to be

`egl::init` failing was a `?` out of `run`, so the process died before it could signal ready and
the Supervisor rolled back to the generation that still had a working context. Now the failure
surfaces from `ensure_egl`, which sets `self.exit` like every other bind failure in
`ensure_bound` -- after promotion, with no rollback left.

Accepted. Reaching it needs EGL to work for one generation of a session and fail for the next,
which needs a driver replaced or a GPU reset under a live process. The alternative is initialising
eagerly to prove it works, which is the thing this ADR removes.

## Results

| config | Renderer RSS | Renderer PSS |
| --- | --- | --- |
| `return {}`, before | 151 MB | 37 MB |
| `return {}`, after | 16 MB | 13 MB |
| 13 surfaces, after | 183 MB | 59 MB |

RSS and PSS differ by so much because the Mesa pages are shared. Eleven other processes on the
test machine map the same `libLLVM`, so the 125 MB is one copy divided twelve ways and the shell
exiting frees none of it. PSS is the honest number and 13 MB is the honest saving.

## What holds this

Nothing in `cargo test`. `App` needs a live Wayland connection to construct and `egl::init` needs a
live compositor, and `wayland/surface.rs`'s tests are all pure functions over `MapState` and spec
fixtures. The property is checked by running against a real session and reading
`/proc/<pid>/maps` for `libgallium`.

`ponytail:` the ceiling is that a regression here is silent -- deleting the `is_pba_candidate ||`
short-circuit in `bind_and_clear` would put `egl::init` back inside the ready window and every test
would still pass. The upgrade path is a headless EGL harness like the one
`layout::paint`'s tests already build with `PLATFORM_SURFACELESS_MESA`, driving a real `App`.
