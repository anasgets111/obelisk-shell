# Oblisk Supervisor Services Specification
## Durable system services, zero-polling monitors, and feature-complete backends

This spec defines the D-Bus interfaces and system services the long-lived **Oblisk Supervisor** process owns. Each runs off-thread in native Rust, giving the Lua VM event-driven, zero-polling state and command execution.

**Nothing is built until a config asks for it** (ADR-0070). The Renderer sends a `StartCapability` frame the first time an evaluation reads `oblisk.<name>`; that frame constructs the controller each section below describes. A config that reads nothing leaves the process idle after startup: connected to the system bus, control socket bound, Renderer spawned, no bus name claimed, no D-Bus subscription, no poll task, no authentication agent.

Registering the polkit authentication agent is gated on a `textfield` declaring `secure_submit = { capability = "polkit", ... }`, since polkit has no capability member to read. Every registration failure logs rather than stopping the process; "An authentication agent already exists for the given subject" is the normal answer beside any other desktop.

---

## 1. Durable D-Bus notifications server (`org.freedesktop.Notifications`)

The Supervisor claims and holds `org.freedesktop.Notifications` on the session bus, so notifications survive Renderer reloads.

### 1.1 Memory-bounded queue and sanitation

* **Queue cap**: 100 active notifications, FIFO.
* **Field truncation**: `app_name` to 64 bytes, `summary` to 128 bytes, `body` to 512 bytes, each on a character boundary.
* **Plain-text sanitation**: strips scripts, style tags, and image elements with a non-backtracking regex parser before the body reaches Lua's notifications signal feed.
* **Icon spooling**: `image-data`/`icon_data` hints are bounds-checked, PNG-encoded, and written off-thread to `/dev/shm/oblisk-$UID/notifications/notif-{id}.png`. Lua gets the path, never raw pixel bytes; a hint that fails the bounds check is dropped.
* **Picture and app icon are separate fields** (ADR-0091). `image_path` is what the sender attached — `image-data`/`image_data` > `image-path`/`image_path` > `icon_data`, the three spellings the spec accumulated for one thing — always an absolute path to a file that exists. `app_icon` is the positional `app_icon` argument: a theme name carried as a name, or an absolute path (or `file://` URI) run through the trusted-root check. A value holding a path separator that is not absolute is refused rather than passed off as a theme name.
* **Arrival time** (ADR-0093). Every entry carries `timestamp`, Unix epoch seconds on the same clock and in the same unit as `oblisk.system`'s `time`, so a card's relative age is one subtraction. A `replaces_id` replacement is new content and gets a new timestamp.

### 1.2 Interactive inline replies and actions

A client requesting an inline reply (`x-kde-reply` hint or actions carrying an `"inline-reply"` key) gets `has_reply = true` in the signal snapshot. `notifications:reply(id, text)` from the Renderer emits the two-argument `ActionInvoked(id, action_key)` signal with `action_key = "inline-reply::<text>"` (ADR-0033), then removes the notification.

`Notify`'s `actions` array is otherwise split into `actions[]` and `has_default_action` (ADR-0090). Each entry carries the opaque `key` the sender will receive back, a `label` to draw (the key itself when the sender sent an empty one), and, when `hints["action-icons"]` is set, an `icon_name` -- a *theme name*, refused if it holds a path separator, since `icon` also accepts absolute paths. The two keys with meanings of their own never appear as entries: `"default"` becomes `has_default_action`, `"inline-reply"` becomes `has_reply`. At most 8 actions are kept and a label is truncated to 64 bytes, on the same reasoning §1.1 caps the text properties.

`notifications:hold_expiry(seconds)` stops every pending expiry countdown for that long (ADR-0094), which is what keeps a card from vanishing part-way through a reply. Time already served is banked, `0` releases, and the value is clamped to 300 seconds — it is a deadline rather than a paused/resumed flag precisely so a config that misses the release edge cannot pin the feed for the rest of the session.

`notifications:invoke_action(id, key)` emits `ActionInvoked(id, key)` and then removes the notification, which is the base spec's default; `hints["resident"]` is the spec's own exception and keeps it in the queue instead. A removal here also emits `NotificationClosed(id, reason=3)`, because an action-invoked close is a close and a sender tracking its own ids needs to hear about it.

---

## 2. System tray host (`StatusNotifierWatcher` & `StatusNotifierItem`)

The Supervisor hosts `org.kde.StatusNotifierWatcher` at `/StatusNotifierWatcher` on the session bus, so Lua never marshals raw D-Bus for tray icons.

### 2.1 Watcher mechanics and ARGB decoding security

* **Buffer bounds checks**: for each client item's (e.g. `nm-applet`, `discord`) raw ARGB pixmap, verifies `width == height` and `width * height * 4` matches the payload length.
* **Size cap**: 128x128px; larger streams are rejected (ADR-0031).
* **SHM spooling**: valid buffers are written as PNGs to `/dev/shm/oblisk-$UID/tray/{service_name}.png`.
* **Lua surface**: `tray.items` carries only pre-decoded PNG paths.
* **Activation**: `tray:activate(id, x, y)` from the Renderer invokes `Activate(x, y)` on the item's own D-Bus object path.

---

## 3. Persistent media controls (Supervisor-owned MPRIS)

MPRIS ownership lives in the long-lived Supervisor, not the Renderer, so media widgets don't flicker or drop track titles on hot-reload.

### 3.1 D-Bus player discovery and properties caching

* Listens to `org.freedesktop.DBus` name changes to discover sessions matching `org.mpris.MediaPlayer2.*` (Spotify, Audacious, MPV, Firefox, etc.).
* Subscribes to `org.mpris.MediaPlayer2.Player` property changes: playback status, track metadata, volume.
* **Zero-polling progress sync**: instead of polling for seek position, the Supervisor caches `position` (µs), `position_updated_at` (monotonic clock timestamp, µs), and `play_state` (`"Playing"`, `"Paused"`, `"Stopped"`). Lua computes the live position as `position + (now - position_updated_at)`, only while `play_state == "Playing"`, giving fluid progress updates with zero socket traffic.
* **External seek tracking**: a `Seeked` D-Bus signal updates the cached position and timestamp immediately.

---

## 4. Feature-complete NetworkManager D-Bus controller (`oblisk.network`)

Event-driven subscriptions to `org.freedesktop.NetworkManager`; zero polling.

### 4.0 What is subscribed

Every §2.5 field is re-derived from scratch on each event (ADR-0029), so the subscription set is what decides how stale a panel can get, and it has to cover the association and not only the scan (ADR-0082):

* **`Device.Wireless`**: `AccessPointAdded`, `AccessPointRemoved`, `ActiveAccessPoint`, `LastScan`. The first two move the AP list, the third is the only one on this interface that moves when the radio joins or leaves a network, and the fourth ends a scan.
* **`AccessPoint`**: `Strength`, on the associated access point only, re-targeted whenever `ActiveAccessPoint` moves. Every rebuild re-reads all strengths, so one subscription keeps the whole list fresh; subscribing to all of them costs three times the traffic for numbers that refresh anyway (ADR-0082).
* **`Device`**: `State`, on the Wi-Fi device and every wired one. What `network.ethernet_enabled` reads, and how a Wi-Fi disconnect announces itself first.
* **`NetworkManager`**: `WirelessEnabled`, `NetworkingEnabled`, `PrimaryConnection`. The two radio switches, plus whatever holds the default route — which can move between two devices that both stay activated, so no device subscription covers it.
* **`Connection.Active`**: `StateChanged`, on one activation at a time and only while a `network:connect` is in flight. Where `network.connect_error` comes from: it is the only place NetworkManager says *why* a connection went down, and the activation call returns before the radio has tried anything (ADR-0084).

### 4.1 Master switches and radio controls

* **Global networking**: `network:set_networking_enabled(bool)` calls NetworkManager's `Enable(bool)` method. `NetworkingEnabled` itself is a read-only property; only `WirelessEnabled`/`WwanEnabled`/`WimaxEnabled` have setters, so `Enable` is the actual toggle.
* **Wi-Fi radio**: `network:set_wifi_enabled(bool)` sets the read-write `WirelessEnabled` property.
* **Ethernet**: `network:set_ethernet_enabled(bool)` targets Ethernet devices (type `1`); `false` disconnects every wired device, `true` activates each device's existing autoconnect profile if one exists (ADR-0029). There is no NM method that fabricates a carrier connection without a profile already present.

### 4.2 Wi-Fi scanning and frequency band resolution

* **Scan**: `network:scan()` dispatches `RequestScan({})` off-thread. `network.scanning` flips to `true` on initiation, `false` once `PropertiesChanged` on the wireless device reports completion.
* **Band resolution**: an access point's `Frequency` property (MHz) maps to a `band` string: `[2400, 2500]` → `"2.4 GHz"`, `[4900, 5900]` → `"5 GHz"`, `[5925, 7125]` → `"6 GHz"`.
* **Results**: duplicate SSIDs merge to the highest signal strength; the connected access point plus the strongest 19 serialize into `network.available_networks`, connected first, ties on strength broken by SSID so the order does not move between rebuilds (ADR-0083). Sorting `active` ahead of strength is what keeps the connected network inside the cut — `network.ssid`/`strength` are read off this list, so an association weaker than 20 neighbours would otherwise be truncated away and reported as no association at all (ADR-0082). `active` merges across the duplicates rather than riding on the strongest one: NetworkManager keeps more than one AP object per BSSID and `ActiveAccessPoint` routinely names the weaker, so carrying the flag with the winning object drops it (ADR-0082).
* **Link state**: `network.connected` comes from `PrimaryConnection`, not from the AP list, and `network.ssid`/`strength`/`wifi_enabled`/`networking_enabled`/`ethernet_enabled` fill out §2.5 alongside it. The AP list cannot answer any of them: it has no wired entry, and a powered-down radio looks exactly like a powered one joined to nothing.

### 4.3 Hidden, secure, and open network associations

* **Saved**: a profile already stored for the SSID is activated with `ActivateConnection`, not duplicated, and it completes without waiting for a password — a saved network has one already, so `network:connect` finishes on its own rather than stashing an intent for a `secure_submit` that will never arrive (ADR-0084). Only an SSID this machine has never joined reaches `AddAndActivateConnection2` — NetworkManager stores a new profile per call and accepts duplicates of both `id` and SSID, so creating unconditionally left one behind per re-join (ADR-0083).
* **Open**: no password, builds a minimal connection dict and calls `AddAndActivateConnection2`.
* **Secure**: populates `802-11-wireless-security` with `key-mgmt = "wpa-psk"` and the credential. A password supplied for an SSID that is already saved is written back to that profile with `SettingsConnection.Update` before activating, so a stored key can be corrected from the panel; enterprise (`802-1x`) profiles are activated as-is instead, since `GetSettings` omits secrets and a rewrite would drop the stored 802.1X password (ADR-0083).
* **Hidden**: `hidden = true` sets `hidden`/`scan-ssid` in the `802-11-wireless` dict to force active probe broadcasts.
* **Forget**: `network:forget(ssid)` resolves every matching connection profile and calls `Delete()` on each one's object path.
* **Outcome**: `AddAndActivateConnection2` and `ActivateConnection` both return an activation, not a verdict. `network.connecting_ssid` is set on the attempt and cleared when that activation reaches `ACTIVATED` or `DEACTIVATED`; the latter fills `network.connect_error` from the reason code, where `NO_SECRETS` is a wrong password (ADR-0084).

---

## 5. Feature-complete BlueZ Bluetooth controller (`oblisk.bluetooth`)

Binds to `org.bluez` on the system bus.

### 5.1 ObjectManager monitoring and pairing

* **Zero-polling status**: registers on `org.freedesktop.DBus.ObjectManager`, capturing `InterfacesAdded`/`InterfacesRemoved` for `org.bluez.Device1` to update discovered and connected pools instantly.
* **Forget**: `bluetooth:forget(mac)` resolves the device's object path and calls `RemoveDevice(path)` on the active `org.bluez.Adapter1`.

### 5.2 Device battery telemetry and categorization

* **Battery**: monitors `org.bluez.Battery1`; on change, extracts `Percentage` into the device's signal entry.
* **Categorization**: parses each device's `Class` property (32-bit integer) rather than trusting BlueZ's own `Icon` property, which comes back empty whenever `Class == 0` (common for BLE peripherals, ADR-0030). Maps to `category`: `"keyboard"`, `"mouse"`, `"headphones"`, `"headset"`, `"phone"`, `"computer"`, or `"generic"`.

### 5.3 PipeWire audio codec control

`bluetooth:set_audio_codec(mac, codec)` finds the matching BlueZ SPA audio node and issues a `SetParam` on `SPA_PARAM_Route` with the target codec (`"LDAC"`, `"AAC"`, or `"SBC"`). PipeWire tears down and renegotiates the link.

---

## 6. Direct PipeWire audio & stream mixer controller (`oblisk.audio`)

Oblisk is PipeWire-only, no ALSA/PulseAudio wrapper. A background thread binds natively to the PipeWire API.

### 6.1 Event-driven stream and node monitoring

Registers callbacks on the PipeWire registry (`pw_registry`).

* **Mute & volume**: PipeWire's volume-property broadcasts update `audio.volume`/`audio.muted`.
* **Default routing**: output sinks and input sources track reactively. `audio:set_default_sink(id)`/`set_default_source(id)` write to the default `Metadata` node.

### 6.2 Application-specific audio mixer (app mixer)

Tracks every playback node of class `Stream/Output/Audio`, exposed as `audio.apps`. `audio:set_app_volume(id, volume)`/`set_app_muted(id, bool)` target one node's PipeWire id without touching global volume.

---

## 7. Durable idle capability (`ext-idle-notifier-v1`)

Oblisk has no hardcoded inactivity timeouts; the config sets its own.

### 7.1 Dynamic, multiple threshold registration

* The Supervisor binds once to the compositor's `ext_idle_notifier_v1`.
* `idle:register_threshold(seconds, on_idle, on_resume)` from Lua dispatches a registration packet over the IPC; the Supervisor allocates a distinct `ext_idle_notification_v1` listener for that duration.
* **Handoff loop**: on `ext_idle_notification_v1::idled`, the Supervisor pushes the matched threshold duration to the Renderer, which runs `on_idle()`; `resumed` runs `on_resume()`.
* Lua can register unlimited custom thresholds (dim at 30s, lock at 5m, DPMS sleep at 10m) with zero active timers.
* Registrations do not outlive an evaluation: re-running `shell.lua` drops every callback the old tree registered, since the Supervisor's listener persists and re-registering the same duration is a no-op there.
* Two registrations for the same duration are one Wayland listener and two callbacks; the Supervisor allocates per distinct duration and fans out, since the event names the threshold, not the registration.

### 7.2 Idle inhibit

* `idle:inhibit(reason)`/`idle:release_inhibit()` hold off auto-suspend through `org.freedesktop.login1.Manager.Inhibit(what="idle", mode="block")` on the system bus the Supervisor already has (ADR-0032). Not the Wayland `idle-inhibit-unstable-v1` protocol, which inhibits per surface and would need the Renderer to own it.
* The hold is a counted, per-generation reference on one logind fd: it opens on the 0-to-1 transition and closes on 1-to-0, so a media player and a presentation mode can both hold it without either release killing the other.
* logind closes the fd if the holding process dies, so a Supervisor crash cannot leak a stuck inhibit.
* Notify degrading to inert (no `ext_idle_notifier_v1`, a failed dedicated connection, a setup timeout) does not disable inhibit. The two halves share a controller, not a transport.

---

## 8. High-performance wallpaper transition engine

Wallpapers render on the GPU inside the Renderer, double-buffered for seamless animated transitions.

### 8.1 GPU scaling, fit, and transition algorithms

* **Fit**: `"Cover"`, `"Contain"`, `"Stretch"`, `"Tile"`, `"Center"`, `"ScaleDown"` are solved inside GLES3 fragment shaders, no CPU pixel resizing.
* **Command**: `wallpaper:set(monitor, filepath, [fit], [transition], [duration])`.
* **Transitions**: `"Crossfade"`, `"Slide"`, `"Sweep"`, `"Zoom"`, interpolated between old and new textures on the GPU.

---

## 9. Active window tracking (niri only)

The Renderer/Supervisor track the focused toplevel's class and title over niri's IPC event stream (ADR-0056), not a compositor-independent Wayland protocol. `oblisk.workspaces` stays `nil` on any other compositor; nothing here reaches for `ext-foreign-toplevel-list-v1` or `zwlr_foreign_toplevel_manager_v1`.

### 9.1 `workspaces.active_client`

`title`, `class` (niri's `app_id`; Wayland has no `WM_CLASS`), and `is_floating` are exposed to Lua. `is_fullscreen` is **not** exposed: niri-ipc 26.4.0 carries no fullscreen state to source it from (ADR-0056 decision 5).

### 9.2 Icon resolution: superseded, not built

§ 3.2's `system:find_icon(app_id, name, fallback)` was never built (ADR-0054 decision 5, amended by ADR-0061). The two halves it would have joined now live elsewhere. Theme-name-to-file resolution moved to the Renderer, reached through `icon.name` (ADR-0054), because the control socket carries one-way commands and snapshots only, with no request/response shape for a synchronous lookup to return a path over. The `app_id`-to-`.desktop`-to-`Icon=` half is served by the enumerated `oblisk.applications` capability instead of a per-`app_id` call (ADR-0061): a launcher wants the whole entry list, not one lookup at a time.

---

## 10. Workspace state (niri only, no adaptor trait)

Compositor identity is detected once, by env var (`$HYPRLAND_INSTANCE_SIGNATURE`, `$NIRI_SOCKET`, in that probe order). `oblisk.workspaces` has exactly one implementor, niri's IPC socket, and no `WorkspaceAdaptor` trait: ADR-0056 decision 1 treats a trait with one implementor as speculative generality, so the compositor-specific code is a module boundary (`workspaces::niri`) rather than a dynamically loaded driver. A session detected as anything but niri leaves `oblisk.workspaces` `nil` rather than guessing.

---

## 11. Configurable telemetry scheduler (`sysinfo`)

* **Intervals**: `sysinfo:configure({ cpu_interval, ram_interval, temp_interval })`.
* **Suspension**: an interval of `0` halts and suspends that background task entirely.
* **Sources**: CPU from `/proc/stat`'s aggregate line, RAM from `/proc/meminfo`, temperatures from `/sys/class/hwmon/` chip resolution by name preference. No shell-outs.

---

## 12. Asynchronous non-blocking subprocess stream pipelines (`process`)

Non-blocking subprocess spawning with line-buffered streams.

* **No Lua blockage**: stdout/stderr lines feed Lua callbacks via a non-blocking select loop as they're emitted.
* **Process group lifecycle**: every `process.run` child gets its own Unix process group (`process_group(0)`, not hand-rolled `setpgid`). On config reload the Supervisor sends `SIGTERM` to the group (`killpg`); if it hasn't exited within a 100ms grace window, it escalates to `SIGKILL`. No zombie or orphaned background processes survive a reload.

---

## 13. Unified power & thermals (`oblisk.power`)

Two unrelated sources under one capability: **UPower** (`org.freedesktop.UPower`) for `power.energy_rate` and battery state, **power-profiles-daemon** for `power.active_profile` and `power:set_profile(name)`. The daemon renamed its bus name from `net.hadess.PowerProfiles` to `org.freedesktop.UPower.PowerProfiles` in 0.20; the Supervisor tries the new name first and falls back to the old one.

---

## 14. XDG directory layout & state manager

| Path | XDG baseline | Write state | Content |
| :--- | :--- | :--- | :--- |
| `~/.config/oblisk/` | `$XDG_CONFIG_HOME` | Read-only to engine | `shell.lua` and every `.lua` file it `require`s (ADR-0047). |
| `~/.local/state/oblisk/` | `$XDG_STATE_HOME` | Read-only today | Flat state file (`state.json`). |
| `/dev/shm/oblisk-$UID/` | RAM memory-disk | Read-write (RAM) | Decoded notification images and icons. |

### 14.1 `state.json`: read path only

`state.json` loads once at construction, at `$XDG_STATE_HOME/oblisk/state.json` (falling back to `~/.local/state/oblisk/state.json`); a missing file is the ordinary first-run case, not a fault. `system:write_state(key, val)` is a separate, **unbuilt** IDL row (§ 3.2). There is no write path from Lua yet, so no atomic-rename contract exists to describe.

---

## 15. Glitch-free Renderer hot-reload and overlapping handoff lifecycle

An invisible hot-reload, no stutters, black frames, or desktop flashes, via the **Presentation Before Authority (PBA)** protocol. The compositor never sees a gap in frame commits; the active session stays interactive throughout.

```
                  [ Config Save (inotify) ]
                             │
            ┌────────────────┴────────────────┐
            ▼                                 ▼
[ Active Generation N ]            [ Spawn Candidate N+1 ]
- Fully authoritative              - Evaluates AST (5ms)
- Accept seat inputs               - Binds Wayland Protocols
- Commit active buffers            - Staged in Null-Buffer State
            │                                 │
            │                      (Activate) │ (IPC Socket)
            │                       ◄─────────┤
            │                                 │
            │                                 ▼
            │                      [ GLES3 Rendering Frame ]
            │                      - Commit physical textures
            │                      - Request Presentation Feedback
            │                                 │
            │                     (Presented) │ (wp_presentation_feedback)
            │                       ◄─────────┤
            │                                 │
            ▼                                 ▼
[ De-authorize Input ] ────────────▶ [ Promote Generation N+1 ]
- Clear input region                 - Activate Input Grab
- Reaped via SIGTERM                 - Commits Authoritative buffers
```

### 15.1 Concurrent overlapping lifetimes

On a config edit (via `inotify` watch on `~/.config/oblisk/`), the Supervisor does not terminate the active Renderer (Generation N). N stays fully authoritative: active Lua VM, layout, all seat input, while the Supervisor concurrently spawns the edited config as an isolated Candidate (Generation N+1).

### 15.2 Null-buffer staging & nonce-bound handshake

1. **Fast AST evaluation**: the Candidate compiles and evaluates `shell.lua`. High-overhead system queries are skipped; it hydrates from a state snapshot the Supervisor pushes over the IPC instead.
2. **Null-buffer registration**: the Candidate binds `zwlr_layer_surface_v1` [oblisk-idl-api-specs § 6.1], acknowledges the compositor's `configure`, but commits null buffers. It stays invisible, occupying no on-screen coordinates.
3. **Nonce handshake**: once ready, the Candidate signals the Supervisor, which verifies process integrity and writes a nonce-bound `ActivateDraw` command over the private control socket.

### 15.3 Wayland presentation-feedback verification

On the activation nonce, the Candidate draws its initial layout on the GPU, commits the buffer with a `wp_presentation_feedback` request attached, and waits for the `presented` event, confirmation the pixels physically hit the screen on every target monitor, before reporting evidence back to the Supervisor.

### 15.4 The swapping seam & instant reaping

Only once presentation evidence is verified across every connected display does the Supervisor swap:

1. **Input deselection**: Generation N calls `wl_surface::set_input_region` with empty bounds and drops focus.
2. **Candidate promotion**: Generation N+1 claims pointer focus and begins receiving seat input.
3. **Reaping**: `SIGTERM` to N's process group; `SIGKILL` after a 100ms grace window if it hasn't exited.

Delaying N's destruction until N+1 has verified physical screen mapping eliminates black screens, flash boundaries, and coordinate-mapping delays during hot-reloads.
