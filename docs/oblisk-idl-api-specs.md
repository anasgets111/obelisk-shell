# Oblisk IDL and API Specification
## Strict Rust-Lua Boundary and Interface Contract

This document defines the strict binary and type boundaries between the Rust platform layers (Supervisor and Renderer) and the Lua configuration environment. This specification serves as the absolute, compiler-validated contract for type marshalling, reactive state signals, command schemas, and static window surface registrations.

---

## 1. Rust-Lua Marshalling and Type Mapping

All data passing across the Rust-Lua boundary (driven by `mlua` hosting PUC Lua 5.4) is mapped according to the following strict, non-coercive rules. Any mismatch fails immediately at construction/execution time rather than degrading silently or raising unhandled panic errors.

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

### 2.9 Workspaces & Output State (`oblisk.workspaces`)
*   `workspaces.outputs`: `table` (Array of connected display output structures)
    *   Output structure:
        *   `name`: `string` (Connector name, e.g., `"eDP-1"`)
        *   `width`: `integer` (Physical pixel width)
        *   `height`: `integer` (Physical pixel height)
        *   `scale`: `number` (Fractional scaling factor, e.g., `1.25`)
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
| `network:connect(ssid, pwd, hid)` | `capability: "network", action: "connect", arguments: [ssid, pwd, hid]`<br>**Validation**: `ssid` is string. `pwd` is string (nil for open). `hid` is boolean (true for hidden). |
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

### 3.3 The Non-Blocking Process Control Handle (`ProcessHandle`)
The `process.run` function yields an opaque `ProcessHandle` object to Lua:
*   `process_handle:kill()`: Terminate the child process and its entire Unix process group cleanly in Rust (sending `SIGTERM`, then escalating to `SIGKILL` if it persists). Prevents orphaned processes.

---

## 4. The Lazy Window Surface & Input Grab Handshake

To maintain a zero-overhead footprint and support completely dynamic rendering without resource thrashing, Oblisk maps exactly **two static layer surfaces** on startup. All popups, OSDs, and modals are visual nodes drawn inside the permanent **Overlay Surface** (`overlay_canvas`).

```text
Renderer (Lua VM)                          Renderer (Rust Engine)                Wayland Compositor
       │                                            │                                     │
       │                                            │─── eglCreateWindowSurface() ───────▶│
       │                                            │─── zwlr_layer_surface::set_size() ─▶│ (Mapped at startup)
       │                                            │                                     │
       │─── Toggle Modal (visible=true) ───────────▶│                                     │
       │                                            │─── Set Bounding Box Input Region ──▶│ (Updates input region)
       │                                            │                                     │ (Captures click focus)
       │─── Toggle Modal (visible=false) ──────────▶│                                     │
       │                                            │─── Clear Input Region (Empty) ─────▶│ (Input passes through)
```

1.  **Static Surface Setup**: At boot, the Renderer allocates the two top-level window structures returned by `shell.lua`. It registers them with the compositor.
2.  **Input Region Manipulation**:
    *   By default, the **Overlay Surface** has an empty input region. All pointer clicks bypass the canvas entirely and trigger background application windows.
    *   When the Lua configuration toggles visibility of an overlay card (such as a volume OSD or a dropdown notification overlay), the Renderer intercepts the layout change, calculates the absolute coordinate bounding box of that visual child, and calls `wl_surface::set_input_region` on the Overlay surface to include only the active bounds.
    *   Clicks inside the bounds are routed to Lua callbacks (`on_click`), while clicks outside the bounds pass through seamlessly.

---

## 5. Declarative UI Node Primitives (The AST Contract)

The Rust scene-graph engine parses layout trees built from sugar constructors. To keep the engine completely product-neutral, **no visual compound components** are written in Rust. Only the following geometric nodes are defined.

### 5.1 The Abstract Node Base Class Table

Every node schema contains the following base layout properties:

| Property Name | Type | Valid Range / Options | Layout Engine Interpretation |
| :--- | :--- | :--- | :--- |
| `width` | `integer` / `string` | `[0, 8192]` or `"Fill"` | Explicit width or fill maximum available space. |
| `height` | `integer` / `string` | `[0, 8192]` or `"Fill"` | Explicit height or fill maximum available space. |
| `margin` | `table` | `{ top, right, bottom, left }` | Outer spacing boundaries. |
| `padding` | `table` | `{ top, right, bottom, left }` | Inner spacing boundaries. |
| `align_h` | `string` | `"Start"`, `"Center"`, `"End"`, `"Stretch"` | Horizontal alignment distribution. |
| `align_v` | `string` | `"Start"`, `"Center"`, `"End"`, `"Stretch"` | Vertical alignment distribution. |
| `visible` | `boolean` / `Signal` | `true`, `false`, or binary signal | Determines if the node enters constraint and paint passes. |

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

#### 8. `textfield` (The Engine Security Exception)
An IME-aware native input field mapped directly to Rust-owned `wp-text-input-v3`.
*   `placeholder`: `string`
*   `mask_character`: `string` (Capped at 1 byte; if specified, hides typed input)
*   `secure_submit`: `table` (`{ capability, action }`; see § 5's `textfield` glossary entry in `CONTEXT.md`. Only meaningful alongside `mask_character` -- without it, a masked field's value is unreadable from Lua entirely)
*   `on_change`: `function` (Lua callback executed on each committed edit batch from `wp-text-input-v3`, not per keystroke; IME composition is not character-by-character. Key events are swallowed inside Rust's memory blocks during sensitive lock states)
*   `on_submit`: `function` (Fires on `zwp_text_input_v3`'s protocol-native `submit` action, e.g. Enter -- IME-correct, not a raw keystroke check. Takes the committed text as its one argument, *except* when both `mask_character` and `secure_submit` are set: fires with no argument, since the Renderer's IPC layer attaches the native input buffer directly to the named capability/action envelope instead. docs/adr/0005, docs/adr/0027)

## 6. Top-Level Window Surface Nodes

These nodes are returned at the root of `shell.lua` and define physical Wayland window mappings.

### 6.1 `surface`
A layer-shell surface container (`zwlr_layer_surface_v1`).
*   `id`: `string` (Unique window identifier)
*   `layer`: `string` (`"Background"`, `"Bottom"`, `"Top"`, `"Overlay"`)
*   `anchor`: `table` (`{ top, bottom, left, right }` edge booleans)
*   `exclusive`: `boolean` (Reserves physical screen area for bar if true)
*   `height`: `integer` / `string` (Explicit height or `"Fill"`)
*   `width`: `integer` / `string` (Explicit width or `"Fill"`)
*   `monitor`: `string` (A specific output EDID, or `"All"` to spawn on all monitors)
*   `visible`: `boolean` / `Signal` (Hides/unmaps surface completely if false)
*   `child`: `node` (The root visual primitive node inside this window)

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
