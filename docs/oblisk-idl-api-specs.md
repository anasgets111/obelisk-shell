# Oblisk IDL and API Specification
## Strict Rust-Lua Boundary and Interface Contract

This document defines the strict binary and type boundaries between the Rust platform layers (Supervisor and Renderer) and the Lua configuration environment. This specification serves as the absolute, compiler-validated contract for type marshalling, reactive state signals, command schemas, and static window surface registrations.

---

## 1. Rust-Lua Marshalling and Type Mapping

All data passing across the Rust-Lua boundary (driven by `mlua` hosting PUC Lua 5.4) is mapped according to the following strict, non-coercive rules. Any mismatch fails immediately at construction/execution time rather than degrading silently or raising unhandled panic errors.

**What the config VM contains.** "Lua 5.4" no longer describes it on its own. `coroutine`, `table`, `string`, `utf8`, `math`, and `package` are present in full. `debug` and `ffi` are absent, and `package.loadlib` raises, because mlua's safe mode says so. `io` is absent and `os` is cut to `time`, `date`, `clock`, and `getenv`: every other call in either library blocks the thread that dispatches Wayland events, and `process.run` is the non-blocking way to run a command (ADR-0048). `package.path` resolves inside the config directory only (ADR-0047).

### 1.1 Fundamental Type Mapping Table

| Rust Type | Lua Type | Boundary Mapping Rules & Constraints |
| :--- | :--- | :--- |
| `f64` | `number` | Double-precision float. NaN and Inf are rejected. |
| `i64` / `u64` | `integer` / `number` | Mapped to Lua integer if within `[-2^53 + 1, 2^53 - 1]`. |
| `String` | `string` | UTF-8 encoded, byte-length limited string (max 64KB). |
| `bool` | `boolean` | Clean mapping. No type coercion. |
| `Option<T>` | `T` or `nil` | Maps to its inner type `T` on `Some(T)`, or to Lua `nil` on `None`. |
| `Vec<T>` | `table` (array) | 1-indexed dense Lua table. Sparse or mixed-type tables are rejected. |
| `HashMap<String, T>`| `table` (dictionary) | Key-value associative table. Numeric keys in dictionaries are rejected. |
| `Box<Signal<T>>` | `userdata` (`Signal`) | Opaque C-userdata reference containing a stable pointer to the Rust-owned signal. |
| `OpaqueHandle` | `userdata` (`ProcessHandle`) | Non-blocking process control handle returning buffered streams and terminating safely. |

### 1.2 The Reactive Signal Sentinel (`Signal`)

Signals are exposed to Lua as read-only or read-write userdata primitives.

*   **Read-Only Signal Methods**:
    *   `signal:get()`: Returns the current unwrapped primitive value.
    *   `signal:map(fn)`: Returns a new `Computed` signal computed by applying the Lua function `fn` to the parent value.
*   **Computed Signal Rules**:
    *   `computed(dependencies, fn)`: Exposes a multi-dependency computed signal. The `dependencies` argument must be an array of `Signal` or `Computed` handles.
    *   The evaluation function `fn` must be entirely side-effect-free. CPU runtime is capped at 5ms per evaluation.

**Handles are live; `:get()` results are not.** Wherever a node property below accepts `T / Signal`, the two spellings mean different things and both are valid Lua:

```lua
text { content = oblisk.mpris.title }         -- live: re-reads whenever the value changes
text { content = oblisk.mpris.title:get() }   -- frozen: the value at evaluation time, forever
```

A handle left in a property resolves at layout time on every pass, so the node follows the signal. A `:get()` result is a plain string the engine cannot distinguish from a literal, and nothing updates it until the next config edit. Signals are read-only to Lua: a config cannot construct one or write to one, and the only writable state is what the Supervisor pushes (ADR-0044).

---

## 2. Core Engine Signals (Read-Only State Schema)

The active Renderer process populates the global `oblisk` state tree with the following schema. No other properties exist in the global space. All fields below return a `Signal` wrapping the indicated inner type.

### 2.1 Keyboard Modifier & Layout State (`oblisk.keyboard`)
*   `keyboard.caps_lock`: `boolean` (Active = `true`, Inactive = `false`)
*   `keyboard.active_layout`: `string` (The user-friendly active layout name, e.g., `"English (US)"`)

### 2.2 Battery Status (`oblisk.battery`)
*   `battery.present`: `boolean` (True if physical battery detected)
*   `battery.percent`: `integer` (`0` to `100`)
*   `battery.charging`: `boolean` (True if status is "Charging" or "Full")

### 2.3 Brightness State (`oblisk.brightness`)
*   `brightness.percent`: `integer` (`0` to `100` percent)

### 2.4 Audio State (`oblisk.audio`) (PipeWire-Only)
*   `audio.volume`: `number` (Float representing master output volume, range `[0.0, 1.0]`)
*   `audio.muted`: `boolean` (Muted = `true`, Unmuted = `false`)
*   `audio.sinks`: `table` (Array of output playback audio devices):
    *   Sink object:
        *   `id`: `integer` (WirePlumber node ID)
        *   `name`: `string` (User-friendly description, e.g. `"Built-in Audio Analog Stereo"`)
        *   `active`: `boolean` (True if this is the active default output route)
*   `audio.sources`: `table` (Array of input recording audio devices):
    *   Source object:
        *   `id`: `integer` (WirePlumber node ID)
        *   `name`: `string` (User-friendly description, e.g. `"Built-in Microphone"`)
        *   `active`: `boolean` (True if this is the active default input route)
*   `audio.apps`: `table` (Array of per-application volume mixer playback streams):
    *   App stream object:
        *   `id`: `integer` (WirePlumber client playback node ID)
        *   `name`: `string` (Process/Application name, e.g. `"spotify"` or `"chromium"`)
        *   `volume`: `number` (Volume level, range `[0.0, 1.0]`)
        *   `muted`: `boolean` (True if application stream is muted)

### 2.5 Network State (`oblisk.network`) (NetworkManager-Only)
*   `network.connected`: `boolean` (True if default gateway interface is active and online)
*   `network.ssid`: `string` (Connected Wi-Fi SSID, or `"Ethernet"` if wired, or `nil` if offline)
*   `network.strength`: `integer` (Connected Wi-Fi signal strength percent `[0, 100]`, or `0` if offline)
*   `network.wifi_enabled`: `boolean` (True if physical Wi-Fi radio is powered on)
*   `network.networking_enabled`: `boolean` (True if global NetworkManager execution is active)
*   `network.ethernet_enabled`: `boolean` (True if Ethernet link-carrier is active)
*   `network.available_networks`: `table` (Array of scanned Wi-Fi access point structures):
    *   Access Point object:
        *   `ssid`: `string` (AP name)
        *   `strength`: `integer` (Signal percentage `[0, 100]`)
        *   `secure`: `boolean` (True if security key is required)
        *   `band`: `string` (Frequency group identifier: `"2.4 GHz"`, `"5 GHz"`, or `"6 GHz"`)
        *   `active`: `boolean` (True if currently associated/active)
*   `network.connection_details`: `table` (Active IP configurations or `nil`):
    *   `ip_address`: `string` (e.g. `"192.168.1.15"`)
    *   `interface`: `string` (e.g. `"wlan0"`)
    *   `gateway`: `string` (e.g. `"192.168.1.1"`)
    *   `dns`: `table` (Array of DNS server IP strings)

### 2.6 Bluetooth State (`oblisk.bluetooth`) (BlueZ-Only)
*   `bluetooth.enabled`: `boolean` (True if Bluetooth adapter is powered on)
*   `bluetooth.discovering`: `boolean` (True if background discovery scan is active)
*   `bluetooth.connected_devices`: `table` (Array of active paired & connected accessories):
    *   Connected Device object:
        *   `mac`: `string` (Canonical MAC address, e.g. `"00:1A:7D:DA:71:11"`)
        *   `name`: `string` (Device name)
        *   `battery`: `integer` (Device battery percent `[0, 100]`, or `-1` if unsupported/unknown)
        *   `codec`: `string` (Active negotiated PipeWire audio codec: `"LDAC"`, `"AAC"`, `"SBC"`, or `nil`)
        *   `category`: `string` (Visual category identifier: `"keyboard"`, `"mouse"`, `"headphones"`, `"headset"`, `"phone"`, `"computer"`, `"generic"`)
*   `bluetooth.discovered_devices`: `table` (Array of un-paired discovered accessories):
    *   Discovered Device object:
        *   `mac`: `string`
        *   `name`: `string`
        *   `paired`: `boolean` (Always false for items in this scan pool)

### 2.7 Notifications State (`oblisk.notifications`)
*   `notifications.feed`: `table` (Array of active notifications received over the D-Bus, capped at 20 entries)
    *   Notification object structure:
        *   `id`: `integer` (Unique notification identifier)
        *   `app_name`: `string` (Calling application name, sanitized/stripped)
        *   `summary`: `string` (Title, sanitized)
        *   `body`: `string` (Main message text, sanitized plain text)
        *   `icon_path`: `string` (Asset path to cached image, or empty)

### 2.8 Media Players (`oblisk.mpris`)
*   `mpris.players`: `table` (Array of active MPRIS playback targets):
    *   Player object structure:
        *   `id`: `string` (Unique bus name suffix identifier)
        *   `identity`: `string` (e.g. `"Spotify"`)
        *   `play_state`: `string` (`"Playing"`, `"Paused"`, `"Stopped"`)
        *   `title`: `string` (Track title)
        *   `artist`: `string` (Track artist)
        *   `album_art_path`: `string` (Filepath URI pointing to cached image in `/dev/shm`)
        *   `position`: `integer` (Playback offset in microseconds *at the timestamp of last update*)
        *   `position_updated_at`: `integer` (Monotonic clock timestamp in microseconds matching the exact instant the position was recorded)
        *   `length`: `integer` (Total track duration in microseconds)

### 2.9 Workspace State (`oblisk.workspaces`)
Workspace state only. Output geometry lives in `oblisk.screens` (§ 2.15), which reads it from `wl_output` rather than from a compositor adaptor; this section refers to screens by `name` instead of restating their dimensions (ADR-0041).
*   `workspaces.outputs`: `table` (Array of per-output workspace structures)
    *   Output structure:
        *   `name`: `string` (Connector name, e.g., `"eDP-1"`, matching an `oblisk.screens` entry)
        *   `active_workspace`: `integer` (ID of the workspace currently visible)
        *   `focused_workspace`: `integer` (ID of the workspace that currently has keyboard focus)
*   `workspaces.active_client`: `table` (Focused top-level Wayland client window parameters, or `nil` if none focused):
    *   `title`: `string` (Active window title text, e.g. `"src/main.rs - Neovim"`)
    *   `class`: `string` (Active window application class name, e.g. `"Alacritty"` or `"firefox"`)
    *   `is_floating`: `boolean` (True if marked floating/pinned by compositor)
    *   `is_fullscreen`: `boolean` (True if window occupies entire display boundary)

### 2.10 Rescue Mode & Recovery State (`oblisk.rescue`)
*   `rescue.is_rescue`: `boolean` (True if the user configuration is broken and Rescue Mode is active)
*   `rescue.error_log`: `string` (The compiled Lua syntax error or backtrace message)

This signal covers **reload** failures only, where the scene from before the edit is still on screen and the still-running config can render its own error banner. It cannot cover a **startup** failure, because there is no tree to render it through: a config that fails its first evaluation has no surfaces, and `is_rescue` has nobody to read it. That case is handled out of band by a separate Supervisor-spawned process (ADR-0046), not by this signal, and no config code runs for it.

### 2.11 Persistent User State and Storage Paths (`oblisk.system`)
*   `system.state`: `table` (A reactive, read-only dictionary of persistent states loaded from `$XDG_STATE_HOME/oblisk/state.json`)
*   `system.time`: `integer` (Reactive system time epoch, updated at 1-second intervals)

### 2.12 System Hardware Diagnostics (`oblisk.sysinfo`)
*   `sysinfo.cpu_percent`: `integer` (`0` to `100` total CPU core utilization, updated per configurable interval)
*   `sysinfo.ram_percent`: `integer` (`0` to `100` physical memory footprint)
*   `sysinfo.swap_percent`: `integer` (`0` to `100` swap partition usage)
*   `sysinfo.temp_cores`: `table` (Array of core temperatures in Celsius, parsed from `/sys/class/hwmon/`)
*   `sysinfo.temp_gpu`: `integer` (Active GPU temperature in Celsius, `-1` if undetected)

### 2.13 Power Profiles and Thermals (`oblisk.power`)
*   `power.active_profile`: `string` (Active scaling profile: `"performance"`, `"balanced"`, `"power-saver"`)
*   `power.profiles`: `table` (Array of strings representing all hardware profiles supported on host)
*   `power.on_battery`: `boolean` (True if running on battery power)
*   `power.energy_rate`: `number` (Active battery discharge or charge rate in Watts, floating-point)

### 2.14 Tray State (`oblisk.tray`) (docs/adr/0031)
*   `tray.items`: `table` (Array of registered `StatusNotifierItem` tray icons):
    *   Tray Item object:
        *   `id`: `string` (Stable id -- the sanitized D-Bus unique name of the registering process, e.g. `"1.234"`)
        *   `name`: `string` (Display name -- `Title`, falling back to `Id` when empty)
        *   `icon_name`: `string` (Theme icon name, or `nil` if an `icon_path` is set instead -- exactly one of the two is ever populated)
        *   `icon_path`: `string` (Asset path to a decoded, bounds-checked PNG spooled to `/dev/shm`, or `nil` if `icon_name` is set instead)
        *   `tooltip`: `string` (Flattened tooltip title+text, or `nil` if the item has none)
        *   `status`: `string` (SNI status string, e.g. `"Active"`, `"Passive"`, `"NeedsAttention"`)
        *   `item_is_menu`: `boolean` (True if this item must show its menu instead of activating on click)
        *   `menu`: `table` (Array of top-level Menu Item objects, or `nil` if this item has no `com.canonical.dbusmenu` menu):
            *   Menu Item object:
                *   `id`: `integer` (DBusMenu-assigned item id, needed for `tray:activate_menu_item`/`tray:menu_will_show`)
                *   `menu_type`: `string` (`"standard"` or `"separator"`)
                *   `label`: `string` (Menu entry text, or `nil`)
                *   `enabled`: `boolean`
                *   `icon_name`: `string` (Theme icon name, or `nil`)
                *   `toggle_type`: `string` (`"checkmark"`, `"radio"`, or `nil` if not a toggle entry)
                *   `toggle_state`: `integer` (DBusMenu's own `-1`/`0`/`1`, or `nil` if not a toggle entry)
                *   `children`: `table` (Array of nested Menu Item objects, recursive, empty if none)

### 2.15 Screens (`oblisk.screens`) (docs/adr/0041)
The connected outputs, read from `wl_output` in the Renderer rather than pushed by the Supervisor. This is the one signal in § 2 that is not a Supervisor-owned capability and does not appear in `shared::CAPABILITIES`; it needs no compositor adaptor and is available from a generation's first evaluation. Iterating it is how a config declares one panel per monitor (ADR-0041), and it updates on monitor hotplug.
*   `screens`: `table` (Array of connected output structures)
    *   Screen structure:
        *   `name`: `string` (Connector name, e.g. `"eDP-1"`. The value a `panel`'s `monitor` property takes, and the key `oblisk.workspaces` entries refer to)
        *   `width`: `integer` (Physical pixel width)
        *   `height`: `integer` (Physical pixel height)
        *   `scale`: `number` (Fractional scaling factor, e.g. `1.25`)
        *   `refresh`: `number` (Refresh rate in Hz, or `nil` if the compositor does not report one)

---

## 3. Command Execution Protocol (Write Path)

All state mutations and system actions must traverse the private IPC command channel back to the Supervisor. Lua configurations invoke these through method calls on imported modules.

### 3.1 Serialization Format
All write actions are serialized as JSON-RPC 2.0 payloads over the private Unix socket.

### 3.2 Target Command Protocols & Validations

| Module Method | IPC Command JSON Payload Details |
| :--- | :--- |
| `system:write_state(key, val)` | `capability: "system", action: "write_state", arguments: [key, val]`<br>**Validation**: `key` must be alphanumeric. `val` must be string, number, or boolean. |
| `system:find_icon(app_id, name, fallback_name)` | *Synchronous internal Rust lookup* returning `string` path.<br>**Validation**: `app_id` and `name` are strings. Falls back to desktop entry values. |
| `audio:set_volume(vol)` | `capability: "audio", action: "set_volume", arguments: [vol]`<br>**Validation**: `vol` must be a float in range `[0.0, 1.0]`. |
| `audio:set_muted(bool)` | `capability: "audio", action: "set_muted", arguments: [bool]`<br>**Validation**: `bool` is boolean. |
| `audio:toggle_mute()` | `capability: "audio", action: "toggle_mute", arguments: []` |
| `audio:set_default_sink(id)` | `capability: "audio", action: "set_default_sink", arguments: [id]`<br>**Validation**: `id` must be an active Sink Node ID. |
| `audio:set_default_source(id)` | `capability: "audio", action: "set_default_source", arguments: [id]`<br>**Validation**: `id` must be an active Source Node ID. |
| `audio:set_app_volume(id, vol)` | `capability: "audio", action: "set_app_volume", arguments: [id, vol]`<br>**Validation**: `id` is application node ID, `vol` float `[0.0, 1.0]`. |
| `audio:set_app_muted(id, bool)` | `capability: "audio", action: "set_app_muted", arguments: [id, bool]`<br>**Validation**: `id` is application node ID, `bool` is boolean. |
| `audio:play_sound(sound)` | `capability: "audio", action: "play_sound", arguments: [sound]`<br>**Validation**: `sound` must be string path or system theme icon name. |
| `audio:set_event_sounds_enabled(en)` | `capability: "audio", action: "set_event_sounds_enabled", arguments: [en]`<br>**Validation**: `en` must be boolean. |
| `brightness:set(pct)` | `capability: "brightness", action: "set", arguments: [pct]`<br>**Validation**: `pct` must be an integer in range `[0, 100]`. |
| `network:set_networking_enabled(en)` | `capability: "network", action: "set_networking_enabled", arguments: [en]`<br>**Validation**: `en` is boolean. |
| `network:set_wifi_enabled(en)` | `capability: "network", action: "set_wifi_enabled", arguments: [en]`<br>**Validation**: `en` is boolean. |
| `network:set_ethernet_enabled(en)` | `capability: "network", action: "set_ethernet_enabled", arguments: [en]`<br>**Validation**: `en` is boolean. |
| `network:scan()` | `capability: "network", action: "scan", arguments: []`<br>**Validation**: Triggers asynchronous AP scanning. |
| `network:connect(ssid, hidden)` | `capability: "network", action: "connect", arguments: [ssid, hidden]`<br>**Validation**: `ssid` is string. `hidden` is boolean (true for hidden). The password never travels as a Lua argument -- it follows as a `secure_submit(network, connect)` (ADR-0005/ADR-0029); an empty secret means an open network. |
| `network:forget(ssid)` | `capability: "network", action: "forget", arguments: [ssid]`<br>**Validation**: Deletes NM profile. |
| `bluetooth:set_enabled(en)` | `capability: "bluetooth", action: "set_enabled", arguments: [en]`<br>**Validation**: `en` is boolean. |
| `bluetooth:start_discovery()` | `capability: "bluetooth", action: "start_discovery", arguments: []` |
| `bluetooth:stop_discovery()` | `capability: "bluetooth", action: "stop_discovery", arguments: []` |
| `bluetooth:pair(mac)` | `capability: "bluetooth", action: "pair", arguments: [mac]`<br>**Validation**: `mac` is string. |
| `bluetooth:connect(mac)` | `capability: "bluetooth", action: "connect", arguments: [mac]`<br>**Validation**: `mac` is string. |
| `bluetooth:disconnect(mac)` | `capability: "bluetooth", action: "disconnect", arguments: [mac]`<br>**Validation**: `mac` is string. |
| `bluetooth:forget(mac)` | `capability: "bluetooth", action: "forget", arguments: [mac]`<br>**Validation**: Removes pairing profile in BlueZ. |
| `bluetooth:set_audio_codec(mac, c)` | `capability: "bluetooth", action: "set_audio_codec", arguments: [mac, c]`<br>**Validation**: `c` is `"LDAC"`, `"AAC"`, or `"SBC"`. |
| `notifications:dismiss(id)` | `capability: "notifications", action: "dismiss", arguments: [id]`<br>**Validation**: `id` is integer. |
| `mpris:send_command(id, cmd)`| `capability: "mpris", action: "control", arguments: [id, cmd]`<br>**Validation**: `cmd` is `"play"`, `"pause"`, `"play_pause"`, `"next"`, `"previous"`. |
| `mpris:seek(id, pos_us)` | `capability: "mpris", action: "seek", arguments: [id, pos_us]`<br>**Validation**: Sets track position to absolute microseconds. |
| `mpris:seek_relative(id, off)`| `capability: "mpris", action: "seek_relative", arguments: [id, off]`<br>**Validation**: Shifts current playback by relative microseconds `off`. |
| `idle:register_threshold(sec, on_idle, on_resume)` | `capability: "idle", action: "register", arguments: [sec]`<br>**Validation**: `sec` is integer. Creates dynamic listener callback reference inside Renderer. |
| `wallpaper:set(mon, path, fit, anim, dur)` | `capability: "wallpaper", action: "set", arguments: [mon, path, fit, anim, dur]`<br>**Validation**: Sets background config parameters. |
| `workspaces:focus(id)` | `capability: "workspaces", action: "focus", arguments: [id]`<br>**Validation**: `id` must be an integer. Focuses target workspace. |
| `rescue:reload_config()` | `capability: "rescue", action: "reload_config", arguments: []`<br>**Validation**: Runs compiler pass on `shell.lua` and reloads Renderer if valid. |
| `sysinfo:configure(cfg)` | `capability: "sysinfo", action: "configure", arguments: [cfg]`<br>**Validation**: `cfg` is dictionary containing integers `cpu_interval`, `ram_interval`, `temp_interval` in seconds. An interval of `0` suspends the matching monitor thread. |
| `power:set_profile(p)` | `capability: "power", action: "set_profile", arguments: [p]`<br>**Validation**: `p` is string matching active host profiles. |
| `process.run(cmd, args, out_cb, exit_cb)`| *Internal non-blocking shell fork* returning `ProcessHandle`. <br>**Validation**: `cmd` is string, `args` array table of strings, callbacks are Lua functions. |
| `tray:activate(id, x, y)` | `capability: "tray", action: "activate", arguments: [id, x, y]`<br>**Validation**: `id` is string, `x`/`y` are integers. No-ops (does not call the real `Activate`) when the item's `item_is_menu` is `true` (docs/adr/0031). |
| `tray:activate_menu_item(id, menu_item_id)` | `capability: "tray", action: "activate_menu_item", arguments: [id, menu_item_id]`<br>**Validation**: `id` is string, `menu_item_id` is integer matching a `menu[].id` from `tray.items`. |
| `tray:menu_will_show(id, submenu_id)` | `capability: "tray", action: "menu_will_show", arguments: [id, submenu_id]`<br>**Validation**: `id` is string, `submenu_id` is integer. Fires DBusMenu's `AboutToShow` and refreshes `tray.items[].menu` before Lua renders it -- required for correctness with apps that populate submenus lazily (docs/adr/0031). |

### 3.3 The Non-Blocking Process Control Handle (`ProcessHandle`)
The `process.run` function yields an opaque `ProcessHandle` object to Lua:
*   `process_handle:kill()`: Terminate the child process and its entire Unix process group cleanly in Rust (sending `SIGTERM`, then escalating to `SIGKILL` if it persists). Prevents orphaned processes.

---

## 4. The Window Surface Lifecycle & Input Grab Handshake

`shell.lua` decides what surfaces exist. Every `surface` node it returns (§ 6.1) maps to one
`zwlr_layer_surface_v1` per output it targets, with its own layer, anchors, namespace, exclusive
zone, and keyboard interactivity. Surfaces are created, unmapped, and destroyed at runtime as the
evaluated topology changes; the Renderer owns no surface the config did not ask for (ADR-0038).

A config may still put every popup, OSD, and modal inside one fullscreen transparent surface, and
the default config does. That is a configuration idiom, not an engine rule. A launcher needing
`keyboard_interactivity = "Exclusive"` while a volume OSD stays click-through needs two surfaces,
because both fields are per surface in the layer-shell protocol.

```text
Renderer (Lua VM)                          Renderer (Rust Engine)                Wayland Compositor
       │                                            │                                     │
       │                                            │─── eglCreateWindowSurface() ───────▶│
       │                                            │─── zwlr_layer_surface::set_size() ─▶│ (One per declared surface)
       │                                            │                                     │
       │─── Toggle Modal (visible=true) ───────────▶│                                     │
       │                                            │─── Set Bounding Box Input Region ──▶│ (Updates input region)
       │                                            │                                     │ (Captures click focus)
       │─── Toggle Modal (visible=false) ──────────▶│                                     │
       │                                            │─── Clear Input Region (Empty) ─────▶│ (Input passes through)
```

1.  **Surface Setup**: The Renderer evaluates `shell.lua`, reads the surface topology from the returned nodes, and registers one layer surface per `(surface, output)` pair with the compositor. Evaluation happens before binding, matching the Candidate's own ordering in `oblisk-supervisor-services-dbus.md` § 15.2.
2.  **Input Region Manipulation**: This applies to any surface whose visible content is smaller than the surface itself. It is load-bearing for a fullscreen transparent surface and a no-op for a tightly-sized bar.
    *   A surface with no visible content has an empty input region. All pointer clicks bypass it entirely and reach background application windows.
    *   When the Lua configuration toggles a child's visibility (a volume OSD, a dropdown notification card), the Renderer recomputes the union of its visible children's absolute bounding boxes and calls `wl_surface::set_input_region` on that surface with only those bounds.
    *   Clicks inside the bounds are routed to Lua callbacks (`on_click`), while clicks outside the bounds pass through.

---

## 5. Declarative UI Node Primitives (The AST Contract)

The Rust scene-graph engine parses layout trees built from sugar constructors. To keep the engine completely product-neutral, **no visual compound components** are written in Rust. Only the following geometric nodes are defined.

### 5.1 The Abstract Node Base Class Table

Every node schema contains the following base layout properties.

Any property in this table or in § 5.2 accepts a `Signal` handle in place of a literal, whether or not its row spells the union out. The engine resolves the handle at layout time and then applies that property's normal rules to the result, so a `Signal` returning `"Fill"` is a valid `width` and one returning a table is the same error a literal table would be (ADR-0044). The rows that name `Signal` explicitly are the ones a config reaches for most, not the only ones allowed.

| Property Name | Type | Valid Range / Options | Layout Engine Interpretation |
| :--- | :--- | :--- | :--- |
| `width` | `integer` / `string` | `[0, 8192]` or `"Fill"` | Explicit width or fill maximum available space. |
| `height` | `integer` / `string` | `[0, 8192]` or `"Fill"` | Explicit height or fill maximum available space. |
| `margin` | `table` | `{ top, right, bottom, left }` | Outer spacing boundaries. |
| `padding` | `table` | `{ top, right, bottom, left }` | Inner spacing boundaries. |
| `align_h` | `string` | `"Start"`, `"Center"`, `"End"`, `"Stretch"` | Horizontal alignment distribution. |
| `align_v` | `string` | `"Start"`, `"Center"`, `"End"`, `"Stretch"` | Vertical alignment distribution. |
| `visible` | `boolean` / `Signal` | `true`, `false`, or binary signal | Determines if the node enters constraint and paint passes. |
| `id` | `string` | Unique among siblings | Optional reconciliation hint. Matches this node to its previous self across a re-resolve, so leases and named state follow the right node when siblings are inserted or removed. Scoped to the parent, so a reusable module may carry the same ids in every instantiation. Not addressable from Lua and has no effect on layout or paint (ADR-0045). |

### 5.2 Specific Geometric Node Schemas

#### 1. `rect`
A flexible rectangular element representing either a containment box or a solid drawing shape, depending on whether it has children.
*   `background`: `string` (Hex-color code `#RRGGBB` or `#RRGGBBAA`)
*   `radius`: `integer` (Corner rounding radius)
*   `border_color`: `string` / `table` (Hex-color code or a dictionary table `{ top, right, bottom, left }`)
*   `border_width`: `integer` / `table` (Border thickness in logical pixels or `{ top, right, bottom, left }`)
*   `children`: `table` (Optional dense array of child node structures. If specified, the layout engine instantiates this node as a layout parent container; if omitted, it resolves as a static childless leaf shape, e.g. a progress bar or background spacer.)

#### 2. `row`
Arranges children horizontally.
*   `spacing`: `integer` (Pixels of space between siblings)
*   `children`: `table`

#### 3. `column`
Arranges children vertically.
*   `spacing`: `integer`
*   `children`: `table`

#### 4. `text`
Draws shaped unicode glyph text via `cosmic-text`.
*   `content`: `string` / `Signal` (The string text to display)
*   `font_size`: `integer` (Defaults to `12`)
*   `foreground`: `string` (Hex-color code)

#### 5. `icon`
Draws a system SVG/PNG icon.
*   `name`: `string` / `Signal` (The theme name, e.g., `"audio-volume-high"`)
*   `size`: `integer` (Bounding box diameter)

#### 6. `button`
Receives input focus and pointer events.
*   `children`: `table` (Content elements nested inside the button boundary)
*   `on_click`: `function` (Lua callback executed on mouse click or pointer tap)

#### 7. `list`
A fast-reconciling virtual repeater element.
*   `source`: `Signal` (Must wrap a flat array table)
*   `itemfn`: `function` (A Lua builder function that is executed for every index, returning child nodes)
*   `key`: `function` (Maps a `source` element to a stable string, called on the element rather than on the node `itemfn` builds. Items reconcile by key, so inserting one element rebuilds one item instead of every item below it. Duplicate keys are an error. Without `key`, items match by index and an insertion rebuilds everything after it, which is fine for a short static list and wrong for anything driven by a capability. ADR-0045)

#### 8. `textfield` (The Engine Security Exception)
An IME-aware native input field mapped directly to Rust-owned `wp-text-input-v3`.
*   `placeholder`: `string`
*   `mask_character`: `string` (Capped at 1 byte; if specified, hides typed input)
*   `secure_submit`: `table` (`{ capability, action }`; see § 5's `textfield` glossary entry in `CONTEXT.md`. Only meaningful alongside `mask_character` -- without it, a masked field's value is unreadable from Lua entirely)
*   `on_change`: `function` (Lua callback executed on each committed edit batch from `wp-text-input-v3`, not per keystroke; IME composition is not character-by-character. Key events are swallowed inside Rust's memory blocks during sensitive lock states)
*   `on_submit`: `function` (Fires on `zwp_text_input_v3`'s protocol-native `submit` action, e.g. Enter -- IME-correct, not a raw keystroke check. Takes the committed text as its one argument, *except* when both `mask_character` and `secure_submit` are set: fires with no argument, since the Renderer's IPC layer attaches the native input buffer directly to the named capability/action envelope instead. docs/adr/0005, docs/adr/0027)

## 6. Top-Level Surface Nodes

These nodes are returned at the root of `shell.lua` and define physical Wayland surface mappings. In Wayland a `wl_surface` is inert until a protocol assigns it a **role**; Oblisk exposes four, one constructor each (ADR-0040). The set of surfaces is whatever `shell.lua` returns, evaluated fresh on each reload; there is no fixed or engine-owned set (ADR-0038).

| Constructor | Role | Protocol | Typical use |
| :--- | :--- | :--- | :--- |
| `panel` | Layer surface | `zwlr_layer_surface_v1` | Bar, dock, wallpaper, OSD, launcher |
| `window` | Toplevel | `xdg_toplevel` | Settings window, standalone dialog |
| `popup` | Popup | `xdg_popup` | Dropdown, context menu, tooltip |
| `lock` | Lock surface | `ext_session_lock_surface_v1` | Lock screen |

All four share the base node properties (§ 5.1) and take a `child` node tree. Adding or removing any of them is a topology change (`CONTEXT.md`); see ADR-0038 for what changes in place instead.

### 6.1 `panel`
A layer-shell surface container (`zwlr_layer_surface_v1`). Formerly named `surface`; renamed in ADR-0040 when "surface" became the umbrella term for all four roles.
*   `id`: `string` (Unique window identifier. A surface targeting several outputs produces one Wayland surface per output, addressed as `"{id}@{output}"`)
*   `layer`: `string` (`"Background"`, `"Bottom"`, `"Top"`, `"Overlay"`)
*   `anchor`: `table` (`{ top, bottom, left, right }` edge booleans)
*   `exclusive`: `boolean` (Reserves physical screen area for bar if true)
*   `height`: `integer` / `string` (Explicit height or `"Fill"`)
*   `width`: `integer` / `string` (Explicit width or `"Fill"`)
*   `margin`: `table` (`{ top, right, bottom, left }` offsets from the anchored edges. Distinct from a node's `padding`, which is inside the surface: `margin` moves the surface itself, so a floating panel inset from a screen edge needs it)
*   `monitor`: `string` (A specific output EDID, or `"All"` to spawn on all monitors)
*   `namespace`: `string` (The layer-shell namespace the compositor sees. Compositor rules match on it, for instance Hyprland's `layerrule` for blur and animations. Defaults to `"oblisk-{id}"`)
*   `keyboard_interactivity`: `string` (`"None"` (default), `"OnDemand"`, or `"Exclusive"`, mapping to layer-shell's own field. A launcher or any surface accepting typed input needs `"OnDemand"` or `"Exclusive"`; leaving it `"None"` means the surface never receives key events)
*   `visible`: `boolean` / `Signal` (Unmaps the surface when false, without destroying it. Toggling this is how a config shows and hides a panel; it does not churn Wayland objects)
*   `child`: `node` (The root visual primitive node inside this window)

### 6.2 `window`
A standard toplevel window (`xdg_toplevel`), the kind the compositor tiles, stacks, and lists in a task switcher. For a settings window or a standalone dialog, where a `panel` would be wrong.
*   `id`: `string` (Unique identifier)
*   `title`: `string` / `Signal` (Window title the compositor displays)
*   `app_id`: `string` (Application identifier the compositor matches rules against, e.g. `"oblisk.settings"`)
*   `min_size`: `table` (`{ width, height }`. Advisory: the spec states a client "should not rely on the compositor to obey" it)
*   `max_size`: `table` (`{ width, height }`. Advisory, same as `min_size`)
*   `on_close`: `function` (Fires when the compositor asks the window to close. This is a request, not a command: the callback may decline by doing nothing, and the window stays open until the config sets `visible = false`)
*   `visible`: `boolean` / `Signal`
*   `child`: `node`

Decorations are not requested per window. Oblisk asks the compositor for server-side decorations once and accepts whatever mode it grants; it draws no titlebar of its own (ADR-0040).

### 6.3 `popup`
A real popup (`xdg_popup`), positioned by the compositor relative to its parent and dismissed by the compositor on click-outside. Parents to either a `panel` or a `window`, so a bar can own a genuine dropdown rather than a hand-positioned second panel.
*   `id`: `string` (Unique identifier)
*   `parent`: `string` (The `id` of the `panel` or `window` this popup anchors to)
*   `anchor_rect`: `table` (`{ x, y, width, height }` in the parent surface's logical coordinates. Required and must be non-zero. Normally passed straight from the rect `button`'s `on_click` hands back, so a dropdown lands on the button that opened it)
*   `width` / `height`: `integer` (Required and must be non-zero; a popup has no `"Fill"`)
*   `anchor`: `string` (Which edge or corner of `anchor_rect` the popup hangs from: `"Top"`, `"Bottom"`, `"Left"`, `"Right"`, `"TopLeft"`, and so on, or `"Center"`)
*   `gravity`: `string` (Which direction the popup extends from that point, same value set as `anchor`)
*   `constraint_adjustment`: `table` (Array of `"SlideX"`, `"SlideY"`, `"FlipX"`, `"FlipY"`, `"ResizeX"`, `"ResizeY"` naming how the compositor may move the popup to keep it on screen. Defaults to `{ "FlipY", "SlideX" }`, which is dropdown behavior; the protocol's own default is no adjustment at all. Applied in the fixed precedence flip, then slide, then resize)
*   `offset`: `table` (`{ x, y }` pixel nudge applied after anchor and gravity)
*   `grab`: `boolean` (Default `true`. Takes an explicit grab, giving the popup keyboard focus and letting the compositor dismiss it on click-outside. A compositor may deny the grab, in which case the popup is dismissed immediately and `on_dismiss` fires; treat that as a normal outcome, not an error)
*   `on_dismiss`: `function` (Fires when the compositor dismisses the popup)
*   `child`: `node`

A popup may only be opened in response to real user input, so `grab = true` outside an input callback is rejected. Nested popups close in reverse order of opening.

### 6.4 `lock`
A session-lock surface (`ext_session_lock_surface_v1`). One per output, created when the session locks and destroyed on unlock. While the session is locked the compositor shows only these, so a `panel` cannot be part of a lock screen (ADR-0042).
*   `id`: `string` (Unique identifier)
*   `child`: `node` (The lock screen's node tree, authored like any other)

There is no `visible`, `monitor`, `anchor`, or size: a lock surface covers its output, exists on every output, and its lifetime is the lock's, not the config's. Authentication runs through a `textfield` with `secure_submit` (§ 5.2 item 8), so the password reaches the Supervisor's PAM worker without entering the Lua VM (ADR-0005, ADR-0028, ADR-0042). Locking is triggered by the Supervisor, not by returning this node; declaring it says what the lock screen looks like, not when it appears.

---

## 7. The Generation-Guarded Command Envelope Contract

To ensure absolute system state integrity during concurrent, overlapping hot-reloads, Oblisk implements a strict cryptographic and generational guard on the IPC command write channel.

### 7.1 Seamless Lua Integration (The mlua Boundary)
The Lua user configuration remains completely clean and boilerplate-free. The Lua script never has to manage or append generation tracking parameters manually. Instead, the native Rust `mlua` hosting container intercepts all method invocations on exported singletons and transparently wraps them inside a **Generation-Guarded Envelope** before serializing to the control socket.

### 7.2 Guarded JSON-RPC 2.0 Envelope Schema
Every write transaction written over the IPC socket carries the sender's active **Generation ID** and the target capability **Revision Sequence Number**:

```json
{
  "jsonrpc": "2.0",
  "method": "ExecuteCommand",
  "params": {
    "generation_id": 4,
    "capability": "audio",
    "action": "set_volume",
    "arguments": [0.75],
    "expected_revision": 42
  },
  "id": 105
}
```

### 7.3 Generational Gating Fields
*   `generation_id`: `integer` (The unique chronological process epoch index assigned to the Renderer instance by the Supervisor on boot).
*   `expected_revision`: `integer` (The logical revision index of the target capability's state snapshot that this command is reacting to).
*   **The Guard Rule**: The Supervisor maintains a chronological ledger of active Renderer generations. If the Supervisor intercepts a command whose `generation_id` is less than the current active generation, or if the `expected_revision` is stale, the Supervisor **instantly drops the packet**, preventing dying processes from generating race conditions or duplicate commands during transition swaps.
