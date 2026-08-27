# ADR-0036: `mpris` capability — `playerctld` excluded by name, track-identity caching over per-push flattening, strict signal-driven seek state

## Status

Accepted

## Context

`docs/oblisk-supervisor-services-dbus.md` §3 and `docs/oblisk-hardware-event-pipeline.md` §3
describe the zero-polling progress-sync math (cache `position`/`position_updated_at`,
Lua computes live progress, `Seeked` updates the cache instantly) but not discovery,
duplication, or degrade shape. `docs/oblisk-idl-api-specs.md` §2.8 and its command table
give the wire shape (`mpris.players` array of `{id, identity, play_state, title, artist,
album_art_path, position, position_updated_at, length}`; `mpris:send_command`/`seek`/
`seek_relative`). `docs/oblisk-reference-fixtures.md` §6's `mini_ticker` confirms Lua wants
the raw array with no Supervisor-side "active player" selection — it iterates
`mpris.players` directly and filters per-item in Lua (`is_playing`). None of the four docs
address the real problems below; this ADR is greenfield, grounded in live testing on this
dev machine and two independent prior-art sources: Quickshell's own C++ `Mpris` service
(`/usr/src/debug/quickshell-git/quickshell/src/services/mpris/{watcher,player}.cpp`) and
the user's own daily-driver Quickshell config (`~/.config/quickshell/Services/Core/
MediaService.qml`), which encodes real, hard-won quirks from running MPRIS against real
players for months.

`supervisor/src/dbus/mod.rs`'s own doc comment already flagged MPRIS as "a later phase";
no prior ADR or design exists.

## Decisions

**`playerctld` is excluded from discovery by exact bus-name suffix match, confirmed via
live introspection to be the only reliable signal.** This machine currently runs both
`org.mpris.MediaPlayer2.firefox.instance_1_59239` (a real player) and
`org.mpris.MediaPlayer2.playerctld` (the `playerctl` project's own aggregator, which
mirrors whichever underlying player is "active" so generic tools don't need per-player
knowledge). Both match the `org.mpris.MediaPlayer2.` discovery prefix. Live `busctl`
queries against both show `Identity`/`DesktopEntry` byte-for-byte identical
(`"Mozilla zen"`/`"zen"` on both) — `playerctld` transparently proxies every property read,
including its own identity, so no property distinguishes it from the player it's mirroring.
Bus-name-suffix equality (`== "playerctld"`) is the only signal that works. Without this
exclusion, the reference `mini_ticker` widget (§6, iterates the raw array with no
dedup) would render two visually identical cards for the same track. This does not affect
external media-key control: this machine's `XF86Audio*` keybinds
(`~/.config/niri/config.kdl`) call the `playerctl` CLI directly, which talks to the
`playerctld` daemon over D-Bus independently of anything Oblisk discovers or displays —
that control path exists entirely outside `oblisk.mpris`.

**A source reporting `CanControl == false` is excluded from tracking entirely, matching
Quickshell's own filter.** `MediaService.qml:32`, `players.filter(player =>
!!player?.canControl)` — Quickshell's own daily-driver config already excludes
non-controllable sources from its player list, not just from a "which one is active"
selection. A player that can't be controlled at all (play/pause/seek/next/previous all
unavailable) isn't meaningfully useful in a status-bar widget built around control buttons
(the reference `mini_ticker`'s `on_click` calls `mpris:send_command` unconditionally), so
`register_player` checks `CanControl` once at registration and skips tracking on `false`
(logged). This is a real, observable exclusion — a `CanControl=false` source is silently
absent from `mpris.players`, the same category of decision as the `playerctld` exclusion
above, called out explicitly here rather than left as an inline code comment only (Spec
review: this decision existed in the implementation before this ADR paragraph did).

**No Supervisor-side "active player" selection.** Quickshell's own `MediaService.qml`
computes a priority-ordered single `active` player (`Playing` > not-`Stopped` > `canPlay` >
first) for its single-card "now playing" UI. Oblisk's reference fixture does the opposite —
`mini_ticker` renders one card per array entry, filtering visibility in Lua per-item. The
IDL's `mpris.players` is a plain array with no `active`/`primary` field. The Supervisor
exposes every non-`playerctld` player unconditionally; Lua owns any "which one to show"
policy, matching every other array-shaped capability payload in this codebase (e.g.
`notifications.feed`, `tray`'s item list).

**Album art: trust-checked local path passed straight through, no SHM copy, no HTTP
fetch.** Live testing against two real players confirms two real shapes: Zen's browser
MPRIS bridge gives `mpris:artUrl = "file:///home/.../177088_0.png"` (a stable local file);
a freshly spawned `mpv --script=mpris.so` playing a local file with no embedded artwork
omits the `mpris:artUrl` key from `Metadata` entirely (a real, common case, not
hypothetical). `album_art_path` becomes: the local path (stripped of its `file://` prefix,
`canonicalize()`d, and confirmed to be a real existing regular file — mirroring
`dbus::notifications::icon::strip_file_uri`/`validate_trusted_path`'s mechanism, but with
no directory-allowlist restriction, since real players cache art in widely varying
locations — Zen's own cache lives under `~/.config/zen/...`, not any XDG-standard cache
dir, and a curated allowlist would false-negative real, legitimate album art) if the key is
present, `file://`-scheme, and resolves; empty string otherwise (key absent, or any
non-`file://` scheme, e.g. a remote `http(s)://` `artUrl` some other MPRIS clients report
but neither observed here nor supportable without a new HTTP-client dependency — none
exists anywhere in this workspace today, and Oblisk's FemtoVG/GLES3 renderer has no native
network-image loader the way Qt/QML's `Image` element does, so unlike Quickshell — which
hands `artUrl` to `Image` unvalidated and lets Qt's own loader handle both schemes — Oblisk
cannot treat the two schemes identically). No SHM copy: the IDL's "cached image in
/dev/shm" wording is aspirational, not load-bearing (the same category of imprecision
ADR-0034 already corrected twice elsewhere) — `dbus::shm_icons`'s PNG-spooling machinery
exists for raw-pixel-hint sources (notification/tray icons) that need encoding before they
have a path at all; `artUrl`'s source is already a stable file on disk, so copying it
serves no purpose the trust-check doesn't already provide.

**Player `id` is the bus-name suffix; the full bus name is reconstructed on every write,
never cached separately.** IDL: "Unique bus name suffix identifier." `id =
address.strip_prefix("org.mpris.MediaPlayer2.")`; `send_command`/`seek`/`seek_relative`
reconstruct the full bus name by prepending the prefix back. Reversible, one string
operation each direction, no `id -> bus_name` lookup table to keep in sync with discovery.

**`length` defaults to `-1` when `mpris:length` is absent or the wrong D-Bus type,
confirmed by Quickshell's own implementation.** `player.cpp:96-101`: Quickshell's
`bInternalLength` binding defaults to literal `-1` on a missing/malformed `mpris:length`
key, with `bLengthSupported = (length != -1)` as a derived flag — independent
confirmation of the same "genuine unavailable, not a fabricated zero" convention this
codebase already established for `backlight_pct`/`temp_gpu` (ADR-0034/0035). A live stream
or radio source with no fixed duration is a real "doesn't exist" case, distinct from an
impossible true zero-length track.

**Seek: cache `mpris:trackid` per player, `SetPosition(trackid, target)` when known,
`Seek(target - position)` fallback when not; clamp `target` to `[0, cached length]`
Supervisor-side before either call; do not rely on the real MPRIS `SetPosition` spec's
trackid-staleness guard.** Quickshell's fallback shape (`player.cpp:196-217`): call
`SetPosition` with the cached `mpris:trackid` when non-empty, otherwise compute a relative
`Seek` offset from the last known position — adopted directly, since some real players
don't report `mpris:trackid` at all. The freedesktop MPRIS spec documents `SetPosition` as
a no-op when the given `TrackId` doesn't match the player's current track (a built-in
staleness guard). Live-tested against a real `mpv` instance via a direct `SetPosition` call
with a deliberately wrong, nonexistent `TrackId`: `mpv`'s own MPRIS implementation
(`mpv-mpris`) honored it anyway and seeked regardless — the guard is not reliably enforced
across real players, so Oblisk keeps its cached `trackid` reasonably fresh (refreshed on
every property resync) but does not treat the spec's guard as a substitute for its own
correctness. `mpris:seek`/`mpris:seek_relative`'s `pos_us`/`off` are clamped to `[0,
cached length]` before either D-Bus call — the user's own `MediaService.qml:112-127` clamps
client-side the same way; matches the equivalent architectural boundary in Oblisk (the one
place, Supervisor-side, that actually issues the write).

**Seek state updates strictly through the real `Seeked`/`PropertiesChanged` signal, no
optimistic local update.** Quickshell updates its local position cache immediately after
issuing `SetPosition`/`Seek` (`player.cpp:216`), before any real signal confirms it, trading
a small round-trip lag for lower-latency seekbar feedback. This would be the first
exception to this codebase's established "state changes flow through the signal, not the
write call" convention (`keyboard::set_backlight`, `bluetooth::set_enabled`, and every
other write action already follow it with zero exceptions). Confirmed keeping the existing
convention rather than opening a first exception: `mpris:seek`/`seek_relative` write and
return, `position`/`position_updated_at` update only once the real `Seeked` signal (or a
`PropertiesChanged` `Metadata`/`Position` update) arrives.

**Track-identity caching for `album_art_path`/`length`, keyed by a composite signal, not
bare `mpris:trackid` alone.** Motivated by the user's own `MediaService.qml:71-91` (real,
empirically-motivated code, not speculative): Zen's MPRIS bridge is observed to omit
`mpris:artUrl`/`mpris:length` on some `PropertiesChanged` updates for the *same* still-
playing track, which would otherwise flicker `album_art_path` to empty and `length` to `-1`
and back on every such gap. The exact key fields come from Quickshell's own C++
(`player.cpp:266-298`): trackid, `xesam:url`, *and* `xesam:title` are each checked
independently, any one changing marks a track change — the comment there explains why one
field alone isn't enough ("Some players (Jellyfin) specify xesam:url or mpris:trackid and
DON'T ACTUALLY CHANGE THEM WHEN THE TRACK CHANGES"). The QML dotfile's own `_trackKey`
(`MediaService.qml:87-91`) independently corroborates the general "don't trust one metadata
field alone" principle, though its literal fields differ (`[dbusName, url, title]` — a
static per-player value plus two of the same three signals, not trackid). Oblisk's
`PlayerState` keeps a composite track key (`trackid` + `xesam:url` + `xesam:title`,
matching `player.cpp`'s three-signal shape) alongside the last-known-good
`album_art_path`/`length`. On every resync: if the composite key is unchanged from the
previous resync, a missing/malformed `artUrl`/`length` in the new read keeps the previous
value instead of clearing it; if the key changed, both reset (empty/`-1`) before applying
whatever the new read provides. This is strictly better than a flat per-push default and
costs one extra cached struct field per player.

**Degrade shape: keep the player entry, degrade only the affected field, matching
bluetooth/tray precedent exactly.** A transient property-read failure mid-resync (e.g.
`Metadata` reads fine but `PlaybackStatus` momentarily doesn't) does not drop the whole
player from `mpris.players` — a player worth listing at all shouldn't vanish over one
glitch. Independently corroborated by Quickshell's own reactive-property model
(`onGetAllFinished`, `DBusPropertyGroup`): a property that fails to update via signal simply
keeps its last bound value; there is no whole-entry-drop path in Quickshell's model either.

**Discovery: `ListNames` scan at startup, `NameOwnerChanged` filtered by the
`org.mpris.MediaPlayer2.` prefix thereafter — the same shape Quickshell's own
`MprisWatcher` uses (`watcher.cpp`: `registerExisting()` + a `QDBusServiceWatcher` on
`"org.mpris.MediaPlayer2*"`).** This codebase's only existing `NameOwnerChanged` precedent
(`dbus::tray::registry::spawn_name_owner_changed_forwarder`) only removes already-known,
self-registered names; MPRIS needs active prefix-based discovery of names it never
registered itself, a genuinely new mechanism validated directly against a mature real-world
implementation rather than invented from scratch.

**One capability, N producers, one shared `Arc<Mutex<Vec<PlayerState>>>`** — reuses
ADR-0035's multi-producer-capability mechanism exactly (independent per-player tasks
writing into one shared state behind one shared signal channel, full array re-derived and
pushed on any player's event, no debounce). `oblisk.mpris` is the second capability with
this shape; nothing about N *players* instead of N *poll intervals* changes the mechanism.

**Module layout: `supervisor/src/dbus/mpris/{watcher,player,controller}.rs`**, mirroring
`dbus::{tray,bluetooth,notifications}`'s split-by-concern precedent (`watcher.rs`:
discovery/`NameOwnerChanged`; `player.rs`: per-player proxy binding, property resync,
`PlayerState`, track-identity caching; `controller.rs`: the shared state/signal owner and
write-action dispatch). Session bus (`zbus::Connection::session()`), reusing the connection
already established for `tray`/`notifications` (`supervisor/src/main.rs:174,201`) — MPRIS
is a session-bus protocol, unlike NetworkManager/BlueZ/polkit/idle-inhibit's system bus.

## Consequences

- `docs/oblisk-idl-api-specs.md`'s "cached image in /dev/shm" wording for `album_art_path`
  no longer matches the implementation (a trust-checked pass-through of the player's own
  local path, not a Supervisor-managed SHM copy) — a doc-sync task, not a blocker, the same
  posture ADR-0035 took for §11/§7's undersold field count.
- A player supplying a genuinely remote (`http(s)://`) `artUrl` gets `album_art_path = ""`
  today. Not observed on this machine in either tested player; revisit only against a real
  report from an actual player exhibiting it, not speculatively.
- `playerctld` itself is unaffected and keeps working for anything that talks to it
  directly (media keys, scripts, `playerctl` CLI) — this ADR only excludes it from
  `oblisk.mpris`'s own discovered-player list.
- A real `CanControl=false` source (not observed on this machine) is silently absent from
  `mpris.players` rather than shown read-only. If a real report ever surfaces a legitimate
  read-only "now playing" source a user wants visible without control buttons, that needs
  its own IDL change (an exposed `can_control` field, or a separate read-only list) — not a
  silent widening of this filter.

## Upgrade path

- If a real player is found that never changes `mpris:trackid`/`xesam:url` *and* never
  changes `xesam:title` on an actual track change (defeating the composite key), a fourth
  signal (e.g. `mpris:length` itself, or a content hash) would need to join the key — no
  such player has been observed yet.
- An HTTP-fetch path for non-`file://` `artUrl` schemes, if a real report ever shows this
  losing art on an actual setup — would need a new dependency (no HTTP client exists in
  this workspace today) and its own trust/caching design, not a small addition to this
  ADR's mechanism.
