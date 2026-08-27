# ADR-0031: Tray controller hand-writes SNI/DBusMenu host, prefers IconName over pixmap decode, and skips cache-busting

## Status

Accepted

## Context

`docs/oblisk-supervisor-services-dbus.md` §2 specs a `StatusNotifierWatcher`/
`StatusNotifierItem` host: ARGB pixmap bounds-checking, PNG spooling to
`/dev/shm`, `tray.items` pushed to Lua, and a bare `tray:activate(id, x, y)`
command. It says nothing about menus. `docs/oblisk-idl-api-specs.md` has no
`oblisk.tray` section at all — no read schema, no write-table entries. Grilled
against ADR-0013 (proxy-crate reuse ladder), ADR-0029/0030 (NetworkManager/
BlueZ, the closest prior art: capability-tagged `StateSnapshot`, hand-written
`#[zbus::proxy]` traits, per-object dynamic registry with forwarder tasks, no
debounce), and the user's own framing that item name/icon/tooltip plus menu
plus click-vs-menu-click needed real design, not just spec transcription.

## Decisions

**No crate reuse — hand-write the Watcher, Item, and DBusMenu proxies.**
`system-tray` (JakeStanger, MIT, zbus 5 + tokio) is the only real candidate:
it self-hosts `org.kde.StatusNotifierWatcher` and cooperatively defers via
`NameTaken` tolerance when one already exists, the correct dual-role shape.
But its `Client::new()` always couples Watcher+Host registration with no way
to run one without the other, its `IconPixmap::from_array` has a
source-verified, currently-unreported bug (reads width and height from the
same field index twice via two `.first()` calls, silently squaring every
non-square pixmap's height), and both `IconPixmap.pixels` and
`MenuItem.icon_data` come back as raw undecoded bytes anyway — the crate
would not save the security-critical bounds-checking/PNG-encoding work this
controller has to do regardless. Same rung of ADR-0013's ladder as
`dbus::polkit`/`dbus::bluetooth`: hand-written `#[zbus::proxy]`/
`#[zbus::interface]` types against `org.kde.StatusNotifierWatcher`,
`org.kde.StatusNotifierItem`, and `com.canonical.dbusmenu` directly. This
covers DBusMenu layout parsing too — it's four D-Bus calls and one recursive
struct, smaller than the BlueZ proxies already hand-written, and avoids
depending on a crate for 5% of its surface.

**Watcher/Host registration mirrors the proven dual-role dance.**
`RequestName("org.kde.StatusNotifierWatcher")` with no `ReplaceExisting`/
`DoNotQueue` flags; `NameTaken` is treated as success (defer to a real DE
session already running one), not an error. Either way, once connected to
whichever process owns the name, call
`RegisterStatusNotifierHost(our_unique_name)` against it. Works standalone
(niri/sway, no DE) and coexisting with Plasma/GNOME on the same session bus.

**Registry keys on the resolved D-Bus unique name, never the caller-supplied
`service` string.** `RegisterStatusNotifierItem`'s `service` argument is
either an object path (resolve the bus name from the calling message's
sender) or a bus name (unique `:N.M` used directly; well-known resolved once
via `GetNameOwner`). That resolved unique name (`:N.M` — guaranteed by the
D-Bus spec to contain only digits/colons/dots) is both the registry key
(paired with object path) and, with the leading `:` stripped, the PNG spool
filename component. Using the raw `service` string for either would be a
path-traversal/filename-injection risk from any process on the session bus.
Fixes a real spec inconsistency in the same motion: §14's XDG table gives the
canonical RAM-disk root as `/dev/shm/oblisk-$UID/`, but §1/§2's literal
example paths omit `$UID` entirely, which would collide across users on a
shared machine. Real path:
`/dev/shm/oblisk-$UID/tray/{sanitized_unique_name}.png`.

**Icon source: prefer `IconName`, only decode `IconPixmap` as fallback.**
IDL §5.2's `icon` node primitive already takes a raw theme `name` string and
resolves it renderer-side — that's a solved problem one layer up, and most
well-behaved tray apps set `IconName` to a standard theme icon. Pushing
`icon_name` straight through skips the whole bounds-check/decode/PNG-spool
pipeline entirely, which the spec's own security section (§2.1) only ever
describes for the pixmap case. When `IconPixmap` is the only source, take the
**largest available pixmap capped at the spec's 128px limit** — no assumed
display-size target (an earlier draft guessed a 48px floor; rejected because
Lua's `icon` node `size` field owns display size, not the D-Bus layer, and
downscaling a large source always beats upscaling a small one).

**No PNG cache-busting on `NewIcon`.** Same spool path gets overwritten in
place on every icon update, no revision suffix, no cleanup logic. Building
protection against a stale path-keyed texture cache would be defending
against a caching bug in renderer code that does not exist yet (the renderer
has no scene graph or icon-loading path at all as of this ADR) — Speculative
Generality per this project's own review checklist. Revisit once the
renderer's actual texture-loading mechanism is built and only if it turns out
to matter.

**Menu tree: eager top-level fetch, `AboutToShow`-driven per-submenu
refresh.** `GetLayout` is fetched in full on item registration and re-fetched
on `LayoutUpdated`, so `tray.items[].menu` is populated with no first-open
latency (the user's explicit UX requirement — menus must not visibly load).
But DBusMenu's real-world contract is that some apps (NetworkManager's
applet menu is the canonical example) leave a submenu's children empty until
`AboutToShow(id)` fires — that is the spec's actual signal to populate
lazily. Pure eager-fetch-once would leave those submenus permanently empty.
New write command `tray:menu_will_show(id, submenu_id)` fires `AboutToShow`
and re-fetches that submenu's layout immediately before Lua renders it —
required for correctness, not cosmetic.

**Click semantics enforced in Rust, not left to Lua discipline.**
`tray:activate(id, x, y)` only calls the real `Activate(x, y)` when
`item_is_menu` is false; when true, the supervisor no-ops it rather than
trusting every `shell.lua` author to gate on `item_is_menu` before calling
activate — that's SNI's own documented semantics (an item with
`ItemIsMenu == true` should show its menu instead of activating), enforced
once centrally. `tray:activate_menu_item(id, menu_item_id)` is new, calling
DBusMenu's `Event(menu_item_id, "clicked", ...)`.

**`SecondaryActivate`/`ContextMenu`/`Scroll` deferred, not built.** The spec
doc only ever names `Activate`. Modern tray items overwhelmingly expose a
`Menu` for right-click rather than relying on `ContextMenu(x, y)`, and no
known real consumer needs `Scroll`. A one-line proxy method addition later if
a real device needs one, not worth speculative scope now.

**PNG encoding: the `png` crate, not `image`.** Pure Rust, encode-only usage,
minimal dependency tree. `image` is already a transitive dependency but only
carries decode/format-conversion machinery this controller never touches —
matches this project's demonstrated preference for the minimal option over
the already-present-transitively one (see the `bluer` disqualification in
ADR-0030).

## Consequences

- `oblisk.tray`'s write table ships with `activate`, `activate_menu_item`,
  and `menu_will_show` — none of which existed in either spec doc before this
  round; both docs need updating to match (tracked as a doc-sync task, not
  blocking implementation).
- Menu trees assume human-scale app menus (a handful of entries, shallow
  nesting). No cap enforced this round — revisit only if a real tray item
  proves pathological.
- `StateSnapshot{capability: "tray"}` needs zero new plumbing in
  `shared`/`main.rs`/`socket.rs` — ADR-0029 already generalized the
  capability-tagging path.

## Upgrade path

- `SecondaryActivate`/`ContextMenu`/`Scroll` proxy methods, if a real device
  ever needs them.
- Renderer-side texture cache-busting, only once the renderer's actual
  icon-loading mechanism exists and is shown to need it.
- `docs/oblisk-idl-api-specs.md` gains a real `oblisk.tray` §2.x read schema
  and §3.2 write-table entries matching this ADR's design.
