# Wallpaper is an `image` on a Background panel, not a capability

Wallpaper has two ADRs and a § 3.2 row and no code. ADR-0007 gave it its own `Background`-layer
surface. ADR-0002 specified its reload and transition behaviour. § 3.2 lists `wallpaper:set(mon,
path, fit, anim, dur)`. `shared::CAPABILITIES` has no `wallpaper` entry, there is no controller, no
dispatch arm, and the hardcoded `wallpaper_layer@{output}` surface ADR-0007 asked for was deleted
by ADR-0038 when surface declaration moved into `shell.lua`.

So the question is not "build the wallpaper capability" but "what is left of that design once
ADR-0038 and ADR-0054 have both landed". The answer is: almost none of it needs to be a capability.

## Decision 1: there is no `wallpaper` capability

A config declares the surface itself, and puts an `image` in it:

```lua
panel { id = "wallpaper", layer = "Background", anchor = { "top", "bottom", "left", "right" },
        children = { image { source = wallpaper_path, fit = "cover", width = "Fill", height = "Fill" } } }
```

Every part of `wallpaper:set(mon, path, fit, anim, dur)` already has a home. `mon` is `panel.monitor`,
which ADR-0038 built. `path` is `image.source`. `fit` is `image.fit` (decision 3). `anim` and `dur`
are the two that do not, and they do not because nothing in this engine animates anything (decision
4).

Adding a capability would mean a Supervisor controller that owns no hardware, watches nothing, and
exists only to relay a string the config already has, to a surface the config already declares. It
would be the Middle Man smell with a D-Bus connection attached. § 3.2's row is superseded by this
ADR.

ADR-0007's conclusion is untouched: wallpaper belongs on the `Background` layer, and its reasoning
about `Overlay` being above every application window by protocol definition is exactly as true as
it was. What changed is only who declares that surface, which ADR-0038 already changed.

## Decision 2: a runtime change is a `state()` signal, not a command

Phase 21 built `state(name, initial)`: a writable signal that marks the scene dirty and survives an
in-place reload by name. Bind it to `image.source` and the config changes its own wallpaper by
calling the setter, with no IPC in the path at all.

This is strictly better than the command it replaces. `wallpaper:set()` would have been a one-way
message whose effect a config could not read back; a `state` signal is the value, so a config can
show the current wallpaper's name in a menu without a capability having to report it.

What it does not do is persist. `oblisk.system.state` is read-only and nothing writes
`state.json`, which ADR-0053 already recorded as an open hole; until something does, a wallpaper
chosen at runtime is gone at the next reload. That hole is now load-bearing for a user-visible
feature rather than only for hypothetical ones, which is the argument for closing it, and it is
still not this ADR's to close.

## Decision 3: `fit` is `image`'s property, with three modes

`cover` (default) scales to fill the box and crops the overflow. `contain` scales to fit inside and
leaves the remainder unpainted. `stretch` ignores the aspect ratio. No `tile`: nothing has asked for
it and it is one `ImageFlags` bit away on the day something does.

`cover` is the default because it is the only one of the three that cannot leave a wallpaper with
bars down the side of a screen, and a wallpaper is the reason `fit` exists.

## Decision 4: ADR-0002 stays unbuilt, and stays right

ADR-0002's queue semantics (a set during an in-flight transition queues rather than racing, the
second buffer lives only as long as the transition) describe transition behaviour. `build-steps.md`
records that there is no animation model at all: no easing, no transition, no clock faster than
`system.time`'s 1 Hz, and `CONTEXT.md`'s `Lease`, which exists to hold a removed node's GPU
resource alive for "a wallpaper crossfade", has had no caller since Phase 12.

Nothing here changes that, and nothing here makes it worse. Painting a new texture directly with no
transition is precisely what ADR-0002 already specifies for first frame and for reload, so what
ships is one of that ADR's two branches rather than a contradiction of it. Its other branch is
correct and waiting.

## What this does not decide

**How a wallpaper gets picked.** A config can hardcode a path or read one it was given. There is no
file picker, no directory scan and no `process.run` recipe blessed here.

**The memory cost of a full-screen texture.** A 3840x2160 wallpaper is 32 MB of RGBA in the atlas,
which is the single largest thing ADR-0054's cache will ever hold and roughly the whole of some of
ADR-0043's budget. It is one entry, it is bounded by the number of distinct wallpapers a session
uses, and it wants measuring under Phase 24's harness rather than guessing here.
