# ADR-0030: BlueZ controller uses hand-written proxies, Just-Works-only pairing, and defers audio codec control

## Status

Accepted

## Context

`docs/oblisk-supervisor-services-dbus.md` §5 specs a "feature-complete" BlueZ
controller (`oblisk.bluetooth`): radio enable, discovery, pairing, connect/
disconnect/forget, battery telemetry, device categorization, and PipeWire
audio codec control. Grilled against ADR-0013 (proxy-crate reuse ladder),
ADR-0017 (`audio` capability scope, one-directional PipeWire thread), and
ADR-0029 (the NetworkManager controller, the closest prior art: capability-
tagged `StateSnapshot`, async-in-`select!`, no-debounce push).

## Decisions

**No maintained zbus proxy crate — hand-write against BlueZ's own D-Bus API
docs.** `bluer` (the BlueZ-project crate) depends on the `dbus`/`dbus-tokio`/
`dbus-crossroads` family, not `zbus` — adopting it means two D-Bus client
stacks in one process, not just a version mismatch. The only zbus-based
alternatives (`blues`, `bluebus`) are unmaintained or unreviewed toys. Same
rung of ADR-0013's ladder `dbus::polkit` already sits on: hand-written
`#[zbus::interface]`/proxy types against `org.bluez.Adapter1`, `Device1`,
`Battery1`, `Agent1`, `AgentManager1`, plus the standard
`org.freedesktop.DBus.ObjectManager`.

**Pairing is Just-Works-only, enforced by our own `Agent1`, not deferred to
BlueZ's default agent.** We register `Agent1` with `AgentManager1` using
capability `"NoInputNoOutput"` (forces Just Works for any SSP-capable peer)
and call `RequestDefaultAgent`, at controller construction time — not lazily
on first `pair()` — so any pairing attempt on this machine (including one
triggered outside our own `pair()` action, e.g. `bluetoothctl`) hits our
policy instead of BlueZ's undocumented built-in fallback. Agent method
bodies: `RequestPinCode`/`RequestPasskey`/`DisplayPinCode` return
`org.bluez.Error.Rejected` (legacy PIN-only devices cannot pair — a real,
intentional limitation, not an oversight: the IDL's `pair(mac)` takes no
PIN/passkey argument and defines no secure_submit target for bluetooth).
`RequestConfirmation`/`DisplayPasskey`/`AuthorizeService`/
`RequestAuthorization` auto-accept unconditionally — there is no UI to ask a
human, and refusing would silently break `connect()` for already-trusted or
SSP-Just-Works devices. `Cancel`/`Release` are no-ops.

**`set_audio_codec(mac, codec)` is deferred, not built this round.** Every
other write action is a direct D-Bus call once the device registry (below)
exists. Codec switching needs a live PipeWire `Device` proxy for the BlueZ
SPA node and `SPA_PARAM_Profile` switching (the spec doc's own claim of
`SPA_PARAM_Route` is wrong — verified against PipeWire's `bluez5-device.c`:
codecs are distinct enumerable Profiles like `a2dp-sink-ldac`, switched via
`Device.SetParam`/`SPA_PARAM_Profile`). `audio::mixer`'s PipeWire thread has
only ever pushed `StateSnapshot` out (ADR-0017 deferred `set_app_volume`/
`set_app_muted` for the same reason: no inbound channel exists). Building
`set_audio_codec` here means designing that channel under `bluetooth`'s name
instead of `audio`'s — backwards, since the channel is an `audio` capability
concern. Deferred to a follow-up once `audio`'s own inbound-command design
lands, so the channel is designed once, under the capability that owns it.

**Device tracking is a dynamic per-object registry, not a scan-and-replace
list.** Unlike NetworkManager's fixed device + per-scan AP list, BlueZ's
`Device1` set is unbounded and changes live via `ObjectManager`
`InterfacesAdded`/`Removed`, and `Battery1` can appear/disappear
independently on an already-tracked device. `HashMap<OwnedObjectPath,
DeviceEntry>` keyed by object path (not MAC — path is what `ObjectManager`
events give natively; MAC is derived only for signal output and for
`pair`/`connect`/`disconnect`/`forget`'s path lookup). Hydrated once via
`ObjectManager.GetManagedObjects()` at startup. Each `InterfacesAdded` for a
path carrying `Device1` spawns one forwarder task — the same
`spawn_wifi_signal_forwarder` shape ADR-0029 already proved, instantiated
per-device instead of once — listening to that device's `PropertiesChanged`
(`Connected`, `Paired`, `Name`, plus `Battery1.Percentage` if present) and
forwarding a tagged signal into the controller's channel. Its `JoinHandle` is
stored in the registry entry and aborted on `InterfacesRemoved`.

**Categorization parses `Class` ourselves, not BlueZ's `Icon`.** `Icon` is
BlueZ's own derivation of `Class`/`Appearance` and comes back empty whenever
`Class == 0` (common for BLE peripherals before GAP data is read) — parsing
the same source ourselves is strictly more robust. Bit layout: bits 8-12
Major Device Class, bits 2-7 Minor. Major `0x01`→`"computer"`, `0x02`→
`"phone"`, `0x04` (Audio/Video) minor `0x01`/`0x02`→`"headset"` (has mic),
minor `0x06`→`"headphones"`, other Audio/Video minors→`"generic"` (a wrong
guess, e.g. a car kit shown as headphones, is worse than neutral). Major
`0x05` (Peripheral) minor top-2-bits `01`→`"keyboard"`, `10`→`"mouse"`,
`11`(combo)→`"keyboard"` (deterministic; most combo devices market
themselves primarily as keyboards). Everything else→`"generic"`.

**Push cadence: no debounce**, matching ADR-0029. BlueZ property-change
volume in practice (human-scale pairing/connect events, infrequent battery
ticks) doesn't approach a rate where debounce pays for itself. Revisit only
if a real device proves observably chatty.

**`discovered_devices` clears on `start_discovery()`, not on `stop()`.**
Matches NM's scan-replace semantics for a fresh session; the last snapshot
stays visible after `stop_discovery()` so the UI doesn't blank immediately.
No artificial cap — realistic discovery sessions are short and human-driven,
not a notification-feed-style firehose.

**Single adapter, first one found.** Same single-well-known-device
assumption NM already makes for its one Wi-Fi device. No hardware on this
machine to test a second adapter, and the IDL exposes one flat
`oblisk.bluetooth` signal with no adapter selector.

## Consequences

- Legacy PIN-only Bluetooth devices cannot be paired through this
  controller. Documented here as intentional scope, not a bug to be
  discovered later.
- `oblisk.bluetooth`'s write table ships without `set_audio_codec` this
  round; the IDL doc still lists it; a follow-up ADR will cover the
  `audio`-owned inbound channel it actually needs.
- `StateSnapshot{capability: "bluetooth"}` needs zero new plumbing in
  `shared`/`main.rs`/`socket.rs` — ADR-0029 already generalized the
  capability-tagging path.

## Upgrade path

- Inbound PipeWire command channel (owned by `audio`, not `bluetooth`) →
  unblocks `set_audio_codec` here and `set_app_volume`/`set_app_muted` in
  `audio` at the same time.
- A real PIN/passkey UI flow (its own `secure_submit`-shaped design, or a
  new IDL affordance) → unblocks legacy-device pairing.
- Multi-adapter support, if a real second-adapter use case shows up.
