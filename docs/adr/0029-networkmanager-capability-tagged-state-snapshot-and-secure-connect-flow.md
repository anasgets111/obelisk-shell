# NetworkManager: capability-tagged `StateSnapshot`, `rusty_network_manager`, empty-secret-means-open connect

Grilled against `docs/oblisk-supervisor-services-dbus.md` §4, `build-steps.md` Phase 16, and four existing ADRs this work directly extends or closes: ADR-0022 (explicitly deferred per-capability namespacing as out of scope for the minimal slice), ADR-0004 (revision tracked per capability -- `CONTEXT.md`'s own glossary already defined Revision as *"a capability's state-version counter"*, singular per capability, even though the code only ever had one global `audio_revision: u32`), ADR-0005 (secure submit targets a capability, citing Wi-Fi password entry as its own motivating case), and ADR-0013 (reuse a maintained `#[zbus::proxy]` crate before hand-writing one).

## Capability tagging: `StateSnapshot` gains a `capability` field

`shared::StateSnapshot` had no capability tag; `renderer/src/socket.rs`'s `apply_state_snapshot` hardcoded routing every push to one global `audio` Lua signal; `supervisor/src/main.rs` tracked a single `audio_revision: u32`. That was fine with exactly one capability pushing state and stops being fine with a second.

Decision: `StateSnapshot` gains `capability: String`. `main.rs`'s `audio_revision: u32` becomes `revisions: HashMap<String, u32>`. `apply_state_snapshot` looks up (or lazily creates) the matching live Lua signal by `capability` instead of hardcoding `audio`. `payload: serde_json::Value` stays untyped -- no per-capability payload struct; that's speculative generality until a second capability's shape actually needs distinguishing beyond its name. This closes ADR-0022 item 1 and makes the code match the glossary's own already-written definition of Revision.

## D-Bus access: `rusty_network_manager`, not a hand-written proxy

No NetworkManager crate exists in the dependency tree. Checked crates.io rather than assuming one doesn't exist: `rusty_network_manager` (MIT, `zbus = "5.11.0"`, compatible with this workspace's `zbus 5.19.0`) exports `NetworkManagerProxy`, `DeviceProxy`, `WiredProxy`, `WirelessProxy`, `AccessPointProxy`, `SettingsProxy`, `SettingsConnectionProxy`, `ConnectionProxy` -- exactly the interface set §4.1-4.3 needs (`NetworkingEnabled`/`WirelessEnabled`, `RequestScan`, AP `Frequency`/`Strength`/`Ssid`, connection profile creation and `Delete`). Per ADR-0013's own rule, adopted as-is instead of hand-writing introspection-based proxies. No separate ADR for the pick alone -- swapping proxy crates later only touches trait signatures, cheap to reverse.

## Listener architecture: async in the existing `select!`, not a dedicated thread

`audio::mixer` needed its own `std::thread::spawn` because PipeWire's client API is callback-driven, not async. NetworkManager's API is D-Bus-native, so it has no such constraint. Decision: `PropertiesChanged`/AP-added/AP-removed signal streams merge into `main.rs`'s existing top-level `tokio::select!`, following `dbus::polkit`'s precedent instead of `audio::mixer`'s.

Write actions (`scan`, `connect`, `forget`) get `tokio::spawn`ed rather than awaited inline. ADR-0028's own review found that an inline-awaited call inside this same `select!`, with no ceiling, can wedge the entire Supervisor if the far end hangs -- true there for a PAM worker subprocess, equally true here for a D-Bus call to a wedged NetworkManager. Unlike PAM's `polkit.authenticate`, none of these three write actions need to hand a synchronous result back through the calling envelope, so there's no reason to await them inline at all.

## Password: `secure_submit`, not the IDL's literal `connect(ssid, pwd, hid)`

The IDL spec's `network:connect(ssid, pwd, hid)` puts the password in a plain Lua-visible argument. ADR-0005 names Wi-Fi password entry as `secure_submit`'s own motivating case for existing -- the IDL text and the ADR's stated rationale contradict each other. Decision: follow the ADR.

`network:connect(ssid, hidden)` (a normal `CommandEnvelope`, no password) stashes a single-slot pending connect intent in `main.rs`, mirroring `pending_challenge` from the PAM work (ADR-0028). A `textfield`'s `secure_submit = "network.connect"` (capability `"network"`, action `"connect"`, matching `polkit.authenticate`'s existing capability/action shape) always follows with the password bytes. An **empty** secret means open network -- skip `802-11-wireless-security` entirely and call `AddAndActivateConnection2` with the minimal dict; a **non-empty** secret populates `wpa-psk` with those bytes.

This is one code path for both scanned and hidden networks. A scanned network's AP security flags are visible in `available_networks`, but a hidden network's aren't (nothing broadcast the SSID to scan), so the Supervisor can't reliably tell open from secured for every case on its own -- rather than inspect AP flags for one case and add a separate plain `secured` boolean for the other, the Lua widget always renders a password field and leaves it empty for an open network. `docs/oblisk-idl-api-specs.md` §2.5's `connect()` signature is corrected to `connect(ssid, hidden)` to match.

## Ethernet toggle: no NM method forces a carrier connection

The spec text ("disconnects/connects link carriers") doesn't map onto a real NM method for the "on" direction -- link carrier is hardware-detected (cable plugged in), and NetworkManager has no call that fabricates one. Decision: `set_ethernet_enabled(false)` calls `Device.Disconnect()` on every type-1 device. `set_ethernet_enabled(true)` looks for the device's existing auto-connect connection profile (`connection.autoconnect != false`, via `SettingsProxy`/`ConnectionProxy`) and calls `ActivateConnection` on it if one exists; no-op, not an error, if none does -- there's nothing D-Bus can do without a profile already present.

## Push cadence: no debounce

A completed scan can fire a burst of AP-discovery signals in quick succession. Decision: rebuild the `NetworkState` accumulator (mirrors `audio::mixer`'s `Rc<RefCell<MixerState>>`) and push a fresh `StateSnapshot` on every relevant event, no debounce. `revision` already makes intermediate pushes harmless -- the renderer just re-hydrates the same signal repeatedly. Add debounce later only if a real scan burst proves chatty enough to matter; not designed preemptively.

## What's still open

The exact `NetworkState` struct shape, `forget()`'s loop over every matching connection profile (spec text already says plural "profiles" -- delete all matches, not just the first), and `RequestScan`'s options dict content (empty `{}` by default, no options exposed yet) are implementation-pass details, not designed here.

## Upgrade path

Not yet built: any of it. This ADR records the design; implementation is a separate, later pass per this project's established phase-loop workflow.
