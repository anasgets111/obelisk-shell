# The tray backend exposes what the spec defines

An audit of the tray against `org.kde.StatusNotifierItem` found six gaps. The first pass scored them
by whether `dev-config` used them, and four came out as YAGNI on that basis. That test is wrong:
Oblisk is a framework, the Supervisor is its API, and "this bar does not call it" says nothing about
whether a config could. The right test is whether the spec defines it and applications implement it.

Rescored on that, five of the six are in.

## What the live session says

Three items registered while this was written. Telegram introspects fully and exports `Activate`,
`SecondaryActivate`, `Scroll` and `ContextMenu`, plus every `{Attention,Overlay}Icon{Name,Pixmap}`
property and `IconThemePath`. Vesktop and Slack are Chromium and answer the same properties.

So the methods and properties are there on real applications. Only the values are empty today, and
an empty value is not an absent feature.

## Decision 1: delete a spooled icon when its item goes

`write_icon_png` wrote `/dev/shm/oblisk-$UID/tray/{unique_name}.png` and nothing ever deleted one.
`/dev/shm` is RAM and outlives the process, the filename is built from a connection's unique name,
and a reconnecting application never gets the same one back. So every application restart left one
more file resident until reboot. Files from the previous day were still there when this was written.

`NameOwnerChanged` removal deletes the item's PNGs, and `TrayController::new` sweeps the directory
before anything can spool into it.

`ponytail:` two Supervisors at once and the second sweeps the first's live files, which shows as
blank tray icons until something makes each item re-spool. Two shells is a debugging accident rather
than a mode, against a leak measured in kilobytes.

## Decision 2: `Passive` is the config's call, not the backend's

The spec says a `Passive` item "is likely that visualizations will chose to hide it" -- a
recommendation about presentation, not a rule about data. `TrayItem.status` already reaches Lua, so
`sys_tray.lua` filters and the backend keeps carrying every item. A bar that wants to show Passive
items dimmed can.

This is the opposite call from ADR-0031's `should_call_activate`, and deliberately: that one is
centrally enforced because sending a click to an item that wanted a menu has a side effect on
another process. Hiding an icon has none.

## Decision 3: all three icon variants are carried, none are composited

`TrayItem` gains `attention_icon_{name,path}` and `overlay_icon_{name,path}`, resolved through the
same pipeline as the base pair. `NewAttentionIcon` and `NewOverlayIcon` were already subscribed and
already woke a full refetch that read neither property, so the signals now mean something.

Carried rather than applied. The Supervisor does not decide that `NeedsAttention` swaps the icon,
because a bar might badge it or tint it instead, and it does not composite the overlay, because a
`stack` node is what puts one image on another and the Supervisor has no canvas.

Each variant spools to its own filename (`{name}.png`, `{name}-attention.png`,
`{name}-overlay.png`), since all three would otherwise land on the same path and the last write
would win.

## Decision 4: `SecondaryActivate` and `Scroll` join `TrayAction`

Middle-click and scroll-over-icon. Telegram exports both; so does Chromium; so does Qt's own tray.
Without them no config can express either, which makes it a hole in the API rather than a feature
nobody asked for.

`SecondaryActivate` gets no `should_call_activate` gate. `ItemIsMenu` says a *primary* click must
open the menu instead of activating and says nothing about the secondary one; an application that
wants nothing to happen exports a method that does nothing.

`Scroll` gets its own argument parser. Its `[id, delta, orientation]` looks like `Activate`'s
`[id, x, y]`, and reusing that parser would read the orientation as a coordinate and silently drop
every correctly spelled scroll. The orientation string reaches the application verbatim, because it
is the application's to interpret and policing it here would turn a shrug into a dropped command.

## Decision 5: `IconThemePath` resolves in the Supervisor

An application that bundles artwork the session theme has never heard of says so with
`IconThemePath`. Oblisk ignored it, so those items resolved a name the renderer would look up and
miss.

The Supervisor resolves it: with a theme path set, `{path}/{icon_name}` plus `.png` and `.svg` are
tried, and a hit comes back as `icon_path`, which already means an absolute path. So this needs
nothing new in the renderer, nothing new in the IDL, and no change to a config that already reads
`icon_name or icon_path`.

It outranks `IconName` when it hits, on the grounds that a name which resolved to a concrete file
has no business being looked up again in a theme that does not have it.

Both `IconThemePath` and `IconName` come from the application, so a name containing a path separator
is refused rather than sanitized. A themed icon name never contains one, and the alternative is
canonicalizing paths to decide what is inside a directory the shell does not own.

## What stays out

`AttentionMovieName`, an animation name from the KDE3 era that nothing sets. `WindowId`, which is
X11. `Category`, which groups items in a panel that sorts them, and no bar here sorts.

## Correction recorded

The first pass of this audit rejected decisions 3, 4 and 5 as speculative, citing that `sys_tray.lua`
calls no tray action at all. That is true and irrelevant: it measures one config, and the thing being
designed is the surface every config gets. Recorded because the same mistake is easy to make again
in any capability whose only in-repo consumer is `dev-config`.
