# Oblisk IDL and API specification
## The Rust-Lua boundary and interface contract

The strict binary and type boundary between the Rust platform layers (Supervisor and Renderer) and the Lua config environment. Compiler-validated contract for type marshalling, reactive signals, command schemas, and window surface registration.

---

## 1. Rust-Lua marshalling and type mapping

All data crossing the boundary (`mlua` hosting PUC Lua 5.4) maps per the strict, non-coercive rules below. A mismatch fails immediately at construction/execution time; it never degrades silently or panics.

**What the config VM contains.** "Lua 5.4" no longer describes it alone. `coroutine`, `table`, `string`, `utf8`, `math`, and `package` are present in full. `debug` and `ffi` are absent, and `package.loadlib` raises, per mlua's safe mode. `io` is absent and `os` is cut to `time`, `date`, `clock`, and `getenv`: every other call in either library blocks the Wayland event thread, so `process.run` is the non-blocking way to run a command (ADR-0048). A `json` global with a single `decode` function sits on top of the standard set, since a subprocess's output is otherwise a string Lua cannot read (ADR-0057). `package.path` resolves inside the config directory only, as `?.lua` and `?/init.lua`, replacing Lua's default rather than prepending to it, so no system module can shadow the config's own (ADR-0047). A config's own modules drop from `package.loaded` before every re-evaluation, so editing a required file changes what the next reload sees.

Lua 5.4 detail: `require` returns two values (module, file path) where 5.3 returned one. A call in a table constructor's last position expands to all its values, so `return { require(a), require(b) }` returns three surfaces, the last a string. Bind each `require` to a local first.

### 1.1 Fundamental type mapping table

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

### 1.2 The reactive signal sentinel (`Signal`)

Signals reach Lua as read-only or read-write userdata primitives.

*   **Read-only methods**:
    *   `signal:get()`: current unwrapped primitive value.
    *   `signal:map(fn)`: new `Computed` signal from applying Lua `fn` to the parent value.
*   **Computed signals**:
    *   `computed(dependencies, fn)`: multi-dependency computed signal. `dependencies` must be an array of `Signal`/`Computed` handles.
    *   `fn` must be side-effect-free. CPU runtime capped at 5ms per evaluation.

**Handles are live; `:get()` results are not.** Wherever a property below accepts `T / Signal`, both spellings are valid Lua and mean different things:

```lua
text { content = oblisk.mpris.title }         -- live: re-reads whenever the value changes
text { content = oblisk.mpris.title:get() }   -- frozen: the value at evaluation time, forever
```

A handle left in a property resolves at layout time on every pass, so the node follows the signal. A `:get()` result is a plain string indistinguishable from a literal; nothing updates it until the next config edit. Every Supervisor-pushed signal is read-only to Lua, and `signal:set()` names the kind it refused. `state(name, initial)` (§ 5.2) is the one writable kind a config constructs (ADR-0044).

**A signal resolving to `nil` means the property is absent**, so the documented default applies rather than the resolution failing. Every capability signal reads `nil` until its first `StateSnapshot`, a state a config sees on every boot, so `content = oblisk.mpris.title` renders the `content` default until the first push. This keeps the two spellings consistent too: a Lua table cannot store `nil`, so `content = nil` is already indistinguishable from omitting `content`.

**Structural properties reject a `Signal` outright**, because they are identities rather than values and an identity does not resolve; `node::resolve_properties` copies them through raw. They are `id`, `hover`, and `scroll` on any kind, and `layer`, `anchor`, `monitor`, `namespace` on a `panel`. The `panel` four are read once per evaluation to decide whether a reload is an in-place update or a generation swap (ADR-0001); a value changing after that decision would move a surface between layers or monitors inside a live generation. `hover`/`scroll` name the signal a handler writes into, so a resolved one would arrive as the value instead of the handle (ADR-0062 decision 3, ADR-0069 decision 4). A `panel`'s other live fields, `keyboard_interactivity`, `exclusive`, `margin`, `width`, `height`, are deliberately not structural: layer-shell permits changing each on a mapped surface.

---

## 2. Core engine signals (read-only state schema)

The active Renderer populates the global `oblisk` state tree with the schema below. No other properties exist in the global space. Every field returns a `Signal` wrapping the indicated inner type.

**Reading a capability is what starts it** (ADR-0070). Nothing runs behind `oblisk.bluetooth` until a config indexes that name: the first read hands back the member and tells the Supervisor to build the controller; every read after is an ordinary table lookup. A config that never mentions a capability never pays for its D-Bus subscription, poll task, or bus-name claim; `return {}` starts nothing.

Two consequences: a capability reads `nil` until its first `StateSnapshot` (this now includes the window between the starting read and the controller's first push, so `:map` must handle `nil`), and a start is one-way, an edit removing the last reader does not stop it until the session ends.

`oblisk.idle` has no member to index; its own methods (`register_threshold`, `inhibit`, `release_inhibit`) send the start. `polkit` has no member either; a `textfield` naming it in `secure_submit` (§ 5.2 item 8) registers the authentication agent.

### 2.1 Keyboard modifier and layout state (`oblisk.keyboard`)
*   `keyboard.caps_lock`: `boolean` (Active = `true`, inactive = `false`)
*   `keyboard.active_layout`: `string` (User-friendly active layout name, e.g. `"English (US)"`)

### 2.2 Battery status (`oblisk.battery`)
Read off UPower's `DisplayDevice`, the composite across every battery on the machine (ADR-0080). No UPower means no report, same as § 2.13's missing power-profiles-daemon.

*   `battery.present`: `boolean` (True if the display device is a battery and reports present. False on a desktop, an answer rather than an absence)
*   `battery.percent`: `integer` (`0` to `100`, rounded)
*   `battery.state`: `string` (One of `"Unknown"`, `"Charging"`, `"Discharging"`, `"Empty"`, `"FullyCharged"`, `"PendingCharge"`, `"PendingDischarge"`, UPower's own seven `Device.State` values by name. Replaces a `charging: boolean`, which could not distinguish a held charge limit (`"PendingCharge"`) from running on battery (`"Discharging"`) or from draining to a limit with the cable in (`"PendingDischarge"`); a laptop with `charge_control_end_threshold` set sits in the first most of the day)
*   `battery.time_to_empty`: `integer` (Seconds until flat, or `nil`. UPower reports `0` while charging and before it has estimated; neither is a duration, so both arrive as `nil`)
*   `battery.time_to_full`: `integer` (Seconds until full, or `nil`, on the same terms)

### 2.3 Brightness state (`oblisk.brightness`)
*   `brightness.percent`: `integer` (`0` to `100` percent)

### 2.4 Audio state (`oblisk.audio`) (PipeWire only)
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

### 2.5 Network state (`oblisk.network`) (NetworkManager only)
*   `network.connected`: `boolean` (True if default gateway interface is active and online)
*   `network.ssid`: `string` (Connected Wi-Fi SSID, or `"Ethernet"` if wired, or `nil` if offline)
*   `network.strength`: `integer` (Connected Wi-Fi signal strength percent `[0, 100]`, or `0` if offline)
*   `network.wifi_enabled`: `boolean` (True if physical Wi-Fi radio is powered on)
*   `network.networking_enabled`: `boolean` (True if global NetworkManager execution is active)
*   `network.ethernet_enabled`: `boolean` (True if Ethernet link-carrier is active)
*   `network.connecting_ssid`: `string` (SSID a `network:connect` is currently attempting, or `nil` when none is in flight)
*   `network.connect_error`: `string` (Why the last `network:connect` failed, in words fit to draw, or `nil` when the last one worked)
*   `network.password_ssid`: `string` (SSID whose `network:connect` is waiting on a password, or `nil` when none is. Set only for a secured network with no saved profile -- a saved or open one connects on the click. What a shell binds a `secure_submit` prompt, and its surface's `keyboard_interactivity`, to)
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

### 2.6 Bluetooth state (`oblisk.bluetooth`) (BlueZ only)
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

### 2.7 Notifications state (`oblisk.notifications`)
*   `notifications.feed`: `table` (Array of active notifications received over the D-Bus, capped at 20 entries)
    *   Notification object structure:
        *   `id`: `integer` (Unique notification identifier)
        *   `app_name`: `string` (Calling application name, sanitized/stripped)
        *   `summary`: `string` (Title, sanitized)
        *   `body`: `string` (Main message text, sanitized plain text)
        *   `icon_path`: `string` (Asset path to cached image, or empty)

### 2.8 Media players (`oblisk.mpris`)
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

### 2.9 Workspace state (`oblisk.workspaces`)
Workspace state only. Output geometry lives in `oblisk.screens` (§ 2.15), which reads it from `wl_output` rather than from a compositor adaptor; this section refers to screens by `name` instead of restating their dimensions (ADR-0041).

> **Amended by ADR-0056**, built against niri. `focused_workspace` is present only on the output holding focus, since focus is one workspace across every output and this structure models it per output (decision 4). `is_fullscreen` is not reported: niri-ipc has no such field, and a fabricated `false` would be wrong for exactly the windows a fullscreen check exists to find (decision 5). Each output carries a `workspaces` array, added because the two ids below are opaque and nothing else names which workspaces exist, their names, or their order (decision 3).

*   `workspaces.outputs`: `table` (Array of per-output workspace structures)
    *   Output structure:
        *   `name`: `string` (Connector name, e.g., `"eDP-1"`, matching an `oblisk.screens` entry)
        *   `active_workspace`: `integer` (ID of the workspace currently visible)
        *   `focused_workspace`: `integer` (ID of the workspace that currently has keyboard focus; absent on every output that does not hold focus, per ADR-0056)
        *   `workspaces`: `table` (Array of the workspaces on this output, ordered by `idx`; added by ADR-0056)
            *   Workspace structure:
                *   `id`: `integer` (Stable, monitor-independent identity; what the two ids above refer to and what `workspaces:focus(id)` takes)
                *   `idx`: `integer` (1-based position on this output; not stable across a reorder)
                *   `name`: `string` (The compositor's own name for the workspace, absent when unnamed)
*   `workspaces.active_client`: `table` (Focused top-level Wayland client window parameters, or `nil` if none focused):
    *   `title`: `string` (Active window title text, e.g. `"src/main.rs - Neovim"`)
    *   `class`: `string` (Active window application class name, e.g. `"Alacritty"` or `"firefox"`. A Wayland toplevel has an `app_id`, not a `WM_CLASS`, and that is what this carries)
    *   `is_floating`: `boolean` (True if marked floating/pinned by compositor)
    *   `is_fullscreen`: `boolean` (True if window occupies entire display boundary. **Not reported**, per ADR-0056 decision 5)

> **Two gaps.** No per-workspace window list: a config can name and focus a workspace but not draw the icon of what runs on it (`niri-ipc`'s `Window.workspace_id` would make this an additive field). Special workspaces are not modelled at all. Both listed in `roadmap.md`.

### 2.10 Rescue mode and recovery state (`oblisk.rescue`)
*   `rescue.is_rescue`: `boolean` (True if the user configuration is broken and Rescue Mode is active)
*   `rescue.error_log`: `string` (The compiled Lua syntax error or backtrace message)

Covers **reload** failures only: the pre-edit scene stays on screen and the still-running config renders its own error banner. It cannot cover a **startup** failure, since a config that fails its first evaluation has no surfaces and `is_rescue` has nobody to read it; that case runs through a separate Supervisor-spawned process instead (ADR-0046), with no config code involved.

### 2.11 Persistent user state and storage paths (`oblisk.system`)
*   `system.state`: `table` (A reactive, read-only dictionary of persistent states loaded from `$XDG_STATE_HOME/oblisk/state.json`)
*   `system.time`: `integer` (Reactive system time epoch, updated at 1-second intervals)

### 2.12 System hardware diagnostics (`oblisk.sysinfo`)
*   `sysinfo.cpu_percent`: `integer` (`0` to `100` total CPU core utilization, updated per configurable interval)
*   `sysinfo.ram_percent`: `integer` (`0` to `100` physical memory footprint)
*   `sysinfo.swap_percent`: `integer` (`0` to `100` swap partition usage)
*   `sysinfo.temp_cores`: `table` (Array of core temperatures in Celsius, parsed from `/sys/class/hwmon/`)
*   `sysinfo.temp_gpu`: `integer` (Active GPU temperature in Celsius, `-1` if undetected)

### 2.13 Power profiles and thermals (`oblisk.power`)
*   `power.active_profile`: `string` (Active scaling profile: `"performance"`, `"balanced"`, `"power-saver"`)
*   `power.profiles`: `table` (Array of strings representing all hardware profiles supported on host)
*   `power.on_battery`: `boolean` (True if running on battery power)
*   `power.energy_rate`: `number` (Active battery discharge or charge rate in Watts, floating-point)

### 2.14 Tray state (`oblisk.tray`) (ADR-0031)
*   `tray.items`: `table` (Array of registered `StatusNotifierItem` tray icons):
    *   Tray Item object:
        *   `id`: `string` (Stable id, the sanitized D-Bus unique name of the registering process, e.g. `"1.234"`)
        *   `name`: `string` (Display name: `Title`, falling back to `Id` when empty)
        *   `icon_name`: `string` (Theme icon name, or `nil` if `icon_path` is set instead; exactly one of the two is ever populated)
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

### 2.15 Screens (`oblisk.screens`) (ADR-0041)
The connected outputs, read from `wl_output` in the Renderer rather than pushed by the Supervisor. The one signal in § 2 that is not a Supervisor-owned capability: it does not appear in `shared::CAPABILITIES`, needs no compositor adaptor, and is available from a generation's first evaluation. Iterating it is how a config declares one panel per monitor (ADR-0041); it updates on monitor hotplug.
*   `screens`: `table` (Array of connected output structures)
    *   Screen structure:
        *   `name`: `string` (Connector name, e.g. `"eDP-1"`. The value a `panel`'s `monitor` property takes, and the key `oblisk.workspaces` entries refer to)
        *   `width`: `integer` (Physical pixel width)
        *   `height`: `integer` (Physical pixel height)
        *   `scale`: `number` (Fractional scaling factor, e.g. `1.25`)
        *   `refresh`: `number` (Refresh rate in Hz, or `nil` if the compositor does not report one)

### 2.16 Installed applications (`oblisk.applications`) (ADR-0061)
The installed `.desktop` entries, enumerated from `$XDG_DATA_HOME/applications` and each `$XDG_DATA_DIRS/applications` in precedence order, first occurrence of a desktop file id wins, so a user's own copy overrides the system one. Entries that are `NoDisplay`, `Hidden`, not `Type=Application`, or missing `Name`/`Exec` are omitted. Scanned once at startup and again on `applications:refresh()`; nothing watches the directories (ADR-0061 decision 4), and a rescan finding no change pushes nothing.

The capability ADR-0054 decision 5 deferred until a caller appeared: enumeration rather than that decision's per-`app_id` lookup, because the launcher that wanted it wants the whole list, and a synchronous lookup has no reply shape on the control socket.
*   `entries`: `table` (Array of application structures, sorted by `name`)
    *   Application structure:
        *   `id`: `string` (Desktop file id, e.g. `"org.gnome.Nautilus"`; a subdirectory becomes a dash, per the desktop entry spec. `launch`'s one argument)
        *   `name`: `string` (Unlocalized `Name=`. `Name[xx]` is deliberately not read; see ADR-0061's costs)
        *   `icon`: `string` (The `Icon=` key as written: a theme name or an absolute path. `icon { name = ... }` takes either, ADR-0054 decision 2. `nil` if no `Icon=`)
*   `by_app_id`: `table` (Map from a toplevel's `app_id` to the same application structure, for a caller holding a window's `app_id` rather than a desktop file id, `workspaces.active_client.class` or a tray item's `Id`. Keyed by exact `StartupWMClass` and exact desktop file id first, then case-folded spellings and the last dot-segment of a reverse-DNS id; an exact key is never displaced by a folded one)

> **No `Exec` field, deliberately.** The parsed command line stays in the Supervisor, reached only through `applications:launch(id)` (ADR-0061 decision 3): a config that could read an argv could assemble a different one before handing it back to run.

---

## 3. Command execution protocol (write path)

All state mutations and system actions traverse the private IPC command channel back to the Supervisor. Lua configs invoke these through method calls on imported modules.

### 3.1 Serialization format
All write actions serialize as JSON-RPC 2.0 payloads over the private Unix socket.

### 3.2 Target command protocols and validations

| Module method | IPC command JSON payload details |
| :--- | :--- |
| `system:write_state(key, val)` | `capability: "system", action: "write_state", arguments: [key, val]`<br>**Validation**: `key` must be alphanumeric. `val` must be string, number, or boolean. |
| `system:find_icon(app_id, name, fallback_name)` | **Not built, not planned as written (ADR-0054 decision 5).** Would return a `string` path via synchronous internal Rust lookup, but the control socket carries only one-way commands and snapshots, with no request/response shape to return a path over. The theme-name half of this lookup lives in the Renderer, reached through `icon.name` (§ 5.2 item 5); the `app_id`-to-`.desktop`-to-`Icon=` half had no caller until an application launcher needed enumeration, not a per-`app_id` lookup. **Built instead as `oblisk.applications`** (§ 2.16, ADR-0061), leaving this signature with no caller and no plan. |
| `audio:set_volume(vol)` | `capability: "audio", action: "set_volume", arguments: [vol]`<br>**Validation**: `vol` must be a float in range `[0.0, 1.0]`. |
| `audio:set_muted(bool)` | `capability: "audio", action: "set_muted", arguments: [bool]`<br>**Validation**: `bool` is boolean. |
| `audio:toggle_mute()` | `capability: "audio", action: "toggle_mute", arguments: []` |
| `audio:set_default_sink(id)` | `capability: "audio", action: "set_default_sink", arguments: [id]`<br>**Validation**: `id` must be an active Sink Node ID. |
| `audio:set_default_source(id)` | `capability: "audio", action: "set_default_source", arguments: [id]`<br>**Validation**: `id` must be an active Source Node ID. |
| `audio:set_source_muted(bool)` | **Not specified: a hole, not a decision.** Would mirror `set_muted` as `capability: "audio", action: "set_source_muted", arguments: [bool]`. A microphone-mute toggle is the click target of every privacy indicator, and the default sink has a mute with no counterpart for the default source. The mixer already writes node props, so this needs a dispatch arm and a row, not a mechanism. Listed in `roadmap.md`. |
| `audio:set_app_volume(id, vol)` | `capability: "audio", action: "set_app_volume", arguments: [id, vol]`<br>**Validation**: `id` is application node ID, `vol` float `[0.0, 1.0]`. |
| `audio:set_app_muted(id, bool)` | `capability: "audio", action: "set_app_muted", arguments: [id, bool]`<br>**Validation**: `id` is application node ID, `bool` is boolean. |
| `audio:play_sound(sound)` | `capability: "audio", action: "play_sound", arguments: [sound]`<br>**Validation**: `sound` must be string path or system theme icon name. |
| `audio:set_event_sounds_enabled(en)` | `capability: "audio", action: "set_event_sounds_enabled", arguments: [en]`<br>**Validation**: `en` must be boolean. |
| `brightness:set(pct)` | `capability: "brightness", action: "set", arguments: [pct]`<br>**Validation**: `pct` must be an integer in range `[0, 100]`. |
| `network:set_networking_enabled(en)` | `capability: "network", action: "set_networking_enabled", arguments: [en]`<br>**Validation**: `en` is boolean. |
| `network:set_wifi_enabled(en)` | `capability: "network", action: "set_wifi_enabled", arguments: [en]`<br>**Validation**: `en` is boolean. |
| `network:set_ethernet_enabled(en)` | `capability: "network", action: "set_ethernet_enabled", arguments: [en]`<br>**Validation**: `en` is boolean. |
| `network:scan()` | `capability: "network", action: "scan", arguments: []`<br>**Validation**: Triggers asynchronous AP scanning. |
| `network:connect(ssid, hidden)` | `capability: "network", action: "connect", arguments: [ssid, hidden]`<br>**Validation**: `ssid` is string. `hidden` is boolean (true for hidden). The password never travels as a Lua argument; it follows as a `secure_submit(network, connect)` (ADR-0005/ADR-0029), and an empty secret means an open network. |
| `network:cancel_connect()` | `capability: "network", action: "cancel_connect", arguments: []`<br>**Validation**: Drops any pending connect intent and clears `password_ssid`/`connect_error`. The way out of a password prompt; does not abort an activation already in flight. |
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
| `oblisk.idle:register_threshold(sec, on_idle, on_resume)` | `capability: "idle", action: "register", arguments: [sec]`<br>**Validation**: `sec` is integer, both callbacks are Lua functions taking no arguments. The Renderer holds the callbacks and matches an inbound `IdleEvent` by `threshold_sec`. Dropped on every re-evaluation of `shell.lua`, so register at the top level, not inside a repeatedly-firing callback. `oblisk.idle` is methods only, no signal: a threshold crossing is an event, not state (ADR-0032). |
| `oblisk.idle:inhibit(reason)` | `capability: "idle", action: "inhibit", arguments: [reason]`<br>**Validation**: `reason` is a string, shown by `loginctl list-inhibitors`. Counted per generation; two holders need two releases. |
| `oblisk.idle:release_inhibit()` | `capability: "idle", action: "release_inhibit", arguments: []`<br>**Validation**: None. Releases one hold, not every hold. A release with no matching `inhibit` is a no-op. |
| `oblisk.lock:lock()` | `capability: "lock", action: "lock", arguments: []`<br>**Validation**: None. Asks the Supervisor to lock the session; it commands the Renderer, which holds `ext_session_lock_v1` (ADR-0042). Refused if the config declares no § 6.4 `lock` surface, reported through `rescue` (ADR-0052). **Deliberately no `unlock` action**: the lock screen's own tree is Lua running while it is the only thing on the glass, so one would be a click-through past PAM; the only unlock is the Supervisor's, on a successful `secure_submit(lock, authenticate)`. Spelled `oblisk.lock:invoke("lock")` for now, the `capability:action(...)` sugar this table uses elsewhere is not yet built. |
| `oblisk.applications:refresh()` | `capability: "applications", action: "refresh", arguments: []`<br>**Validation**: None. Rescans the applications directories off-thread and pushes a new `StateSnapshot` only if the result differs. Cheap to call on every launcher open, which `dev-config` does instead of watching the directories (ADR-0061 decision 4). |
| `oblisk.applications:launch(id)` | `capability: "applications", action: "launch", arguments: [id]`<br>**Validation**: `id` must be an `entries[].id` from the current snapshot; an unknown id is logged and nothing spawned. Runs the entry's own `Exec=`, detached and in its own process group, so a generation swap does not reap it and no pipe is held (unlike `process.run`, ADR-0026). `Terminal=true` wraps in `$TERMINAL -e`, refused with a log line if `$TERMINAL` is unset. |
| `wallpaper:set(mon, path, fit, anim, dur)` | **Superseded by ADR-0055. No `wallpaper` capability, none planned.** A wallpaper is an `image` node on a config-declared `Background` panel: `mon` is `panel.monitor`, `path` is `image.source`, `fit` is `image.fit`, and a runtime change writes the `state()` signal bound to `source`, no IPC involved. `anim` and `dur` have nowhere to go: the engine has no animation model (`roadmap.md`). |
| `workspaces:focus(id)` | `capability: "workspaces", action: "focus", arguments: [id]`<br>**Validation**: `id` must be an integer. Focuses target workspace. |
| `rescue:reload_config()` | `capability: "rescue", action: "reload_config", arguments: []`<br>**Validation**: Runs compiler pass on `shell.lua` and reloads Renderer if valid. |
| `sysinfo:configure(cfg)` | `capability: "sysinfo", action: "configure", arguments: [cfg]`<br>**Validation**: `cfg` is dictionary containing integers `cpu_interval`, `ram_interval`, `temp_interval` in seconds. An interval of `0` suspends the matching monitor thread. |
| `power:set_profile(p)` | `capability: "power", action: "set_profile", arguments: [p]`<br>**Validation**: `p` is string matching active host profiles. |
| `process.run(cmd, args, out_cb, exit_cb)`| *Internal non-blocking shell fork* returning `ProcessHandle`. <br>**Validation**: `cmd` is string, `args` array table of strings, callbacks are Lua functions. <br>**Callbacks**: `out_cb(line, stream)` where `stream` is `"stdout"` or `"stderr"`, so both streams reach one callback and a caller wanting only one branches on it. `exit_cb(code)` where `code` is `nil` if a signal killed the process rather than it exiting. |
| `json.decode(text)` | *Pure function, no IPC.* Returns the decoded value, or `nil` plus a message string on malformed input (ADR-0057). <br>**Validation**: `text` is a string; non-UTF-8 bytes are reported as a decode error rather than raised. |
| `tray:activate(id, x, y)` | `capability: "tray", action: "activate", arguments: [id, x, y]`<br>**Validation**: `id` is string, `x`/`y` are integers. No-ops (does not call the real `Activate`) when the item's `item_is_menu` is `true` (ADR-0031). |
| `tray:activate_menu_item(id, menu_item_id)` | `capability: "tray", action: "activate_menu_item", arguments: [id, menu_item_id]`<br>**Validation**: `id` is string, `menu_item_id` is integer matching a `menu[].id` from `tray.items`. |
| `tray:menu_will_show(id, submenu_id)` | `capability: "tray", action: "menu_will_show", arguments: [id, submenu_id]`<br>**Validation**: `id` is string, `submenu_id` is integer. Fires DBusMenu's `AboutToShow` and refreshes `tray.items[].menu` before Lua renders it, required for correctness with apps that populate submenus lazily (ADR-0031). |

### 3.3 The non-blocking process control handle (`ProcessHandle`)
`process.run` yields an opaque `ProcessHandle` to Lua:
*   `process_handle:kill()`: Terminates the child process and its entire Unix process group cleanly (`SIGTERM`, escalating to `SIGKILL` if it persists). Prevents orphaned processes.

> **`json.decode` is built (ADR-0057).** `out_cb` handed Lua a string Lua could not read, putting `nvtop -s`, `lsblk --json`, `busctl --json=short`, `niri msg -j`, and any `curl` fetch out of reach. A pure-Lua decoder in the config was rejected: the engine has converted JSON to Lua since Phase 19 item 16, since every capability payload arrives as a `serde_json::Value` and both `serde_json` and `mlua`'s `serde` feature were already compiled in, so a second decoder in Lua would just disagree with the first about `null`. `json.decode` is one function on that same mapping.

`json.decode(text)` returns the decoded value, or `nil` plus a message string. See ADR-0057 for the return convention and `json.decode`'s doc comment in `renderer/src/lua/json.rs` for what `null` maps to. Deliberately no `json.encode`.

`out_cb` fires once per line with the newline stripped, so a pretty-printed document arrives in pieces: accumulate in `out_cb`, decode in `exit_cb`. Nothing in the shipped config decodes JSON today.

---

## 4. The window surface lifecycle and input grab handshake

`shell.lua` decides what surfaces exist. Every `surface` node it returns (§ 6.1) maps to one
`zwlr_layer_surface_v1` per output it targets, with its own layer, anchors, namespace, exclusive
zone, and keyboard interactivity. Surfaces are created, unmapped, and destroyed at runtime as the
evaluated topology changes; the Renderer owns no surface the config did not ask for (ADR-0038).

A config may still put every popup, OSD, and modal inside one fullscreen transparent surface (the
default config does); that is a configuration idiom, not an engine rule. A launcher needing
`keyboard_interactivity = "Exclusive"` while a volume OSD stays click-through needs two surfaces,
since both fields are per surface in the layer-shell protocol.

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

1.  **Surface setup**: the Renderer evaluates `shell.lua`, reads the surface topology from the returned nodes, and registers one layer surface per `(surface, output)` pair with the compositor. Evaluation happens before binding, matching the Candidate's own ordering in `oblisk-supervisor-services-dbus.md` § 15.2.
2.  **Input region manipulation**: applies to any surface whose visible content is smaller than the surface itself. Load-bearing for a fullscreen transparent surface, a no-op for a tightly-sized bar.
    *   A surface with no visible content has an empty input region; pointer clicks bypass it entirely and reach background application windows.
    *   When the Lua config toggles a child's visibility (a volume OSD, a dropdown notification card), the Renderer recomputes the union of its visible children's absolute bounding boxes and calls `wl_surface::set_input_region` on that surface with only those bounds.
    *   Clicks inside the bounds route to Lua callbacks (`on_click`); clicks outside pass through.

---

## 5. Declarative UI node primitives (the AST contract)

The Rust scene-graph engine parses layout trees built from sugar constructors. To keep the engine product-neutral, **no visual compound components** are written in Rust; only the geometric nodes below are defined.

### 5.1 The abstract node base class table

Every node schema contains the base layout properties below.

A node takes the properties its kind has a row for, and no others; an unrecognized name is refused when the node is read, naming what that kind does accept (`aling_v = "Center"` used to be copied through and read by nothing, so the node silently failed to centre). The same rule puts § 6.1's `layer` out of reach of a `rect`. The four § 6 surface roles take every § 5.1 base property and paint like a `rect`, so they accept `background`, `radius`, `border_color`, and `border_width` too.

Any property here or in § 5.2 accepts a `Signal` handle in place of a literal, except the structural ones § 1.2 lists and the `on_*`/`itemfn`/`key` callbacks, where a resolved `Signal` is refused for not being a function. The engine resolves the handle once per pass and applies that property's normal rules to the result, so a `Signal` returning `"Fill"` is a valid `width` and one returning a table is the same error a literal table would be (ADR-0044). The rows below name `Signal` on only some properties; `lua-meta` spells it on every union that takes one and is the authority, since that is what the language server checks a config against (ADR-0081).

| Property Name | Type | Valid Range / Options | Layout Engine Interpretation |
| :--- | :--- | :--- | :--- |
| `width` | `integer` / `string` | `[0, 8192]` or `"Fill"` | Explicit width or fill maximum available space. |
| `height` | `integer` / `string` | `[0, 8192]` or `"Fill"` | Explicit height or fill maximum available space. |
| `margin` | `table` | `{ top, right, bottom, left }` | Outer spacing boundaries. |
| `padding` | `table` | `{ top, right, bottom, left }` | Inner spacing boundaries. |
| `align_h` | `string` | `"Start"`, `"Center"`, `"End"`, `"Stretch"` | Horizontal alignment distribution. |
| `align_v` | `string` | `"Start"`, `"Center"`, `"End"`, `"Stretch"` | Vertical alignment distribution. |
| `visible` | `boolean` / `Signal` | `true`, `false`, or binary signal | Determines if the node enters constraint and paint passes. |
| `opacity` | `number` / `Signal` | `[0, 1]`, defaults to `1` | How much of this node and its subtree reaches the screen. Inherited multiplicatively: a child inside a node at `0.5` can be fainter but never more solid, so fading a whole panel is one property. Refused outside the range rather than clamped, so `opacity = 50` meaning percent fails the apply. Distinct from `visible = false`: a node at `0` still lays out, still occupies space in its parent's flow, and still takes pointer events. |
| `id` | `string` | Unique among siblings | Optional reconciliation hint. Matches this node to its previous self across a re-resolve, so leases and named state follow the right node when siblings are inserted or removed. Scoped to the parent, so a reusable module may carry the same ids in every instantiation. Not addressable from Lua and has no effect on layout or paint (ADR-0045). |

### 5.2 Specific geometric node schemas

#### 1. `rect`
A flexible rectangular element: a containment box or a solid drawing shape, depending on whether it has children.
*   `background`: `string` (Hex-color `#RRGGBB` or `#RRGGBBAA`. Strict: `#` required, only 6 or 8 hex digits, no 3-digit shorthand, no named colors. Omitted means no fill, distinct from `#00000000`: the first draws nothing, the second a fully transparent rectangle)
*   `radius`: `integer` (Corner rounding radius. Defaults to `0`)
*   `border_color`: `string` / `table` (Hex-color or `{ top, right, bottom, left }`. A bare string applies to all four edges. No default color: an edge paints only where both a color and a non-zero width say so, so `border_width` alone paints nothing, and so does a table omitting that edge; this is normal, not an error, for a shared style table that sets width and conditions color)
*   `border_width`: `integer` / `table` (Thickness in logical pixels or `{ top, right, bottom, left }`. A bare number applies to all four edges. Defaults to `0`, so `border_color` alone paints nothing)
*   `clip`: `string` (`"Box"` or `"Rounded"`, what this node cuts its children to. `"Box"` (default): its own rectangle, square corners, whatever `radius` says. `"Rounded"` uses `radius` instead, so an overflowing child is cut by the same arc the fill draws. Opt-in rather than implied by `radius`, since it costs an offscreen render pass where a square clip is a free GPU scissor rectangle; QML draws the same line, with `Item.clip` and a separate `ClippingRectangle` for the rounded case)
*   `children`: `table` (Optional dense array of child nodes. Present: a layout parent container. Omitted: a static childless leaf shape, e.g. a progress bar or background spacer)

> **No gradient, no shadow.** `background` takes one flat colour. Both are cheap to add: femtovg 0.26, this workspace's only drawing dependency, already ships `Paint::linear_gradient`/`radial_gradient`/`box_gradient` and a Canvas-2D shadow model, so the work is parsers and rows, not rendering. Backdrop blur is separate and harder. Gradient came off the list, the reference config uses none across 129 files; shadow stands on 16 uses across 7. Blur is in `roadmap.md`.

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
*   `elide`: `string` / `Signal` (`"None"` or `"End"`. `"End"` drops trailing characters until the run plus a single-character ellipsis fits the box; a no-op when it already fits or the box is `Content`-sized. Under `wrap = "Word"` it applies to the last line kept rather than to the whole run. Defaults to `"None"`, leaving the clip to cut the run mid-glyph. Only `"End"` exists: QML also elides head and middle, the reference config uses neither, and a middle elide splits a character budget across two runs)
*   `wrap`: `string` / `Signal` (`"None"` or `"Word"`. `"Word"` breaks an over-wide run onto further lines at a word boundary, falling back to a glyph boundary for a single word wider than the box. Defaults to `"None"`: one line, however long. A `Content`-sized box has no width to break against, so wrapping needs an explicit `width`, a `"Fill"`, or a stretched cross axis. ADR-0089)
*   `max_lines`: `integer` / `Signal` (How many lines `wrap = "Word"` may use, counted before `elide` rewrites the last one. `0` and absent both mean no limit, so an expander is `max_lines = expanded:map(function(e) return e and 0 or 2 end)`; a negative is an error. Ignored without `wrap`, since an unwrapped run has one line to begin with)
*   `text_align`: `string` (`"Start"`, `"Center"`, or `"End"`: where the glyph run sits inside the box, distinct from `align_h`, where the node sits inside its parent. Defaults to `"Start"`. Only visible when the box is wider than the text)
*   `content`: `string` / `Signal` (Text to display. Defaults to `""`, so a `text` bound to a capability signal renders empty until that signal's first push rather than rejecting the tree at boot, ADR-0044)
*   `font_size`: `integer` (Defaults to `12`)
*   `foreground`: `string` (Hex-color, same strict form as `rect.background`. Defaults to opaque white)

#### 5. `icon`
Draws a system SVG/PNG icon.
*   `name`: `string` / `Signal` (Theme name, e.g. `"audio-volume-high"`. An absolute path is used as that path instead, the same rule a `.desktop` file's `Icon=` follows, letting § 2.14's tray pass whichever of `icon_name`/`icon_path` it populated without branching. Resolved in the Renderer, ADR-0054)
*   `size`: `integer` (Bounding box diameter. Defaults to `12`, matching `text`'s `font_size`, for the same boot reason as `content`)

#### 5a. `image`
Draws a file. Added by ADR-0054 decision 3: album art has an aspect ratio and a wallpaper is not an icon by any reading.
*   `source`: `string` / `Signal` (Absolute path. Never a theme name; that is `icon`'s job)
*   `fit`: `string` (`"cover"` scales to fill and crops, `"contain"` fits inside, `"stretch"` ignores aspect ratio. Defaults to `"cover"`)

An `image` has no intrinsic size and takes the box § 5.1's `width`/`height` give it, unlike `icon`: knowing a file's own dimensions requires decoding it, and the layout pass has no canvas to decode against.

#### 6. `button`
Receives input focus and pointer events.
*   `children`: `table` (Content elements nested inside the button boundary)
*   `on_click`: `function(rect, button)` (Lua callback on mouse click or pointer tap. Fires for left, right, and middle buttons, on release, only when the release lands on the same node and button the press armed)
    *   `rect`: `table` (Button's absolute rect, `{ x, y, width, height }`, surface logical coordinates, ADR-0050 decision 3)
    *   `button`: `string` (`"left"`, `"right"`, or `"middle"`; any other evdev code arms and fires nothing. Added by ADR-0050's second amendment, which also covers why back/forward are excluded and why this is a name rather than a code)

> A handler declaring one parameter still works, since Lua drops undeclared arguments; it now also runs on a right or middle click, where those events previously did nothing. `if button ~= "left" then return end` restores the old behavior.

> **The pointer model is complete.** The frame handler in `renderer/src/wayland/input.rs` matches `Press`/`Release`/`Leave` for clicks, `Enter`/`Motion`/`Leave` for hover (`hover` below), and `Axis` for the wheel (`scroll` below); the match over `PointerEventKind` is exhaustive, with no swallowing `_ => {}` arm left. What a scrollable container *is* was ADR-0069's decision.

#### Fonts (`fonts`)
The font chain this shell measures and paints with, in fallback order (ADR-0043 decision 2).

*   `fonts(chain)` (Global, called at the top level of `shell.lua`. Takes an array of family-name strings; refused if any entry is not a string, naming which one, since Lua would otherwise coerce a number into a family nobody can find)
    *   `chain`: `table` (Dense array of family names as fontconfig resolves them, e.g. `"CaskaydiaCove Nerd Font Propo"`. Refused if it has a hole or a named key, since `sequence_values` stops at the first `nil` and Lua's `#` is undefined on a sparse table. An entry no font matches is skipped with a diagnostic, so a typo costs that entry, not the chain)

> **One chain; the codepoint picks the face.** Both readers fall back per glyph across the whole chain in order, so a Nerd Font first and a sans face second gives chrome and body text from one declaration. There is no per-node `font_family`. Declaring nothing keeps the default, `sans-serif`, `Noto Sans CJK JP`, `Noto Color Emoji`, none of which carry Nerd Font private-use glyphs, so chrome needs a declared chain or draws tofu. Read once, at startup: a chain change invalidates every measurement, closer to a topology change than an in-place restyle.

#### Named state (`state(name, initial)`)
The one signal a config writes. Reactive state the config owns, keyed by a name that outlives any single evaluation, so an in-place reload hands back the signal the last one built (ADR-0044 decision 5).

*   `state(name, initial)` -> `Signal` (Global. Writable: `signal:set(value)` stores a new value and marks the scene dirty, so the next pass re-resolves every node reading it)
    *   `name`: `string` (The identity. Two calls with one name are one signal, so the writer and the reader need not be the same file)
    *   `initial`: `any` (The value on the first evaluation naming it. Marshal-checked at § 1.1's boundary, the same check `:set()` applies)

> **An edit to `initial` wins; a reload alone does not.** A re-declaration whose `initial` differs from the seed re-seeds the signal, since editing the file is a later write than the `:set()` it lands on; one whose `initial` is unchanged keeps the live value, which is what leaves a dropdown open across an unrelated save (ADR-0044 decision 5's amendment). A table `initial` is never treated as an edit: tables compare by identity and every evaluation builds a fresh one. Numbers compare across integer/float the way Lua's `==` does; two scalars of different types are an edit. `state("t", os.time())` re-seeds on every reload, since the rule reads intent off the value and cannot detect a non-constant. Dies on a generation swap, since the map lives in the process being reaped.

#### Hover (`hover`, `hover(name)`, `hover_rect(name)`)
A **hover slot** is engine-written reactive state naming one region of one surface: whether the pointer is inside it, and where. Declared on any node, read from anywhere (ADR-0062).

*   `hover`: `Signal` (A node property. Takes the signal `hover(name)` returns and marks that node's box as the slot's region. Structural: the handle is what is stored, so this property does not resolve to a value the way every other one does)
*   `hover(name)` -> `Signal` (Global. Boolean, `false` until the pointer is inside the region. Read-only to Lua: `signal:set()` refuses it, since the engine is the writer)
    *   `name`: `string` (The slot's identity, like `state(name, initial)`'s. Two calls with one name are one slot, so declarer and reactor need not be the same file, and an in-place reload keeps an open tooltip open)
*   `hover_rect(name)` -> `Signal` (Global. The region's absolute rect, `{ x, y, width, height }`, surface logical coordinates, the same shape and space `on_click` hands a handler. Bind to a `popup`'s `anchor_rect` to put a tooltip over the node. Keeps the last rect given when the pointer leaves, so `anchor_rect` stays non-zero while the popup closes)

> **A node and every ancestor are hovered.** Hover uses `on_click`'s hit path (ADR-0050 decision 1), so a `pill` that is a `row` wrapping a `button` wrapping a `text` reports all three; a config binds the outermost. Overlapping siblings resolve like paint: the one drawn last is hovered.

#### Scroll (`scroll`, `scroll(name)`)
A **scroll offset** is engine-written reactive state naming how far one container has scrolled along its main axis, in logical pixels. Declared on any flowing container, read from anywhere (ADR-0069).

*   `scroll`: `Signal` (A node property on `row`, `column`, and `list`. Takes the signal `scroll(name)` returns and makes that node a viewport its children move inside. Structural, like `hover`: the layout pass both reads the offset and writes back the one it used)
*   `scroll(name)` -> `Signal` (Global. A number, `0` at top/left. Read-only to Lua: `signal:set()` refuses it, since the engine is the writer)
    *   `name`: `string` (The slot's identity, like `hover(name)`'s and `state(name, initial)`'s. An in-place reload finds the offset the user left)

> A wheel event adds an unbounded distance to the offset; the layout pass then clamps against the content extent it just measured and writes back what it used, so the signal always reads where the container actually is. A `Content`-sized container's viewport equals its content, so the offset clamps to `0`, a no-op, not an error, same as `"Fill"` in a `Content` parent. No `on_scroll` and no content extent yet: nothing wants the wheel as an event or draws a scrollbar. A tooltip is a `popup` with `grab = false`; there is no tooltip role, and `grab = false` is what lets a hover open one at all, with no click to carry § 6.3's input serial. No callback on the edge and no keyboard equivalent: a hover is a condition bound to `visible`, not an event (ADR-0062 decision 1); the keyboard-equivalent signal, `focused`, does not exist.

#### 7. `list`
A fast-reconciling virtual repeater element.
*   `source`: `Signal` (Must wrap a flat array table)
*   `itemfn`: `function` (Lua builder executed for every index, returning child nodes)
*   `key`: `function` (Maps a `source` element to a stable string, called on the element rather than the node `itemfn` builds. Items reconcile by key, so inserting one element rebuilds one item, not every item below it; duplicate keys are an error. Without `key`, items match by index and an insertion rebuilds everything after it, fine for a short static list, wrong for anything capability-driven. ADR-0045)
*   `direction`: `string` (`"Vertical"` (default) or `"Horizontal"`, which way generated items stack. A `list` is a repeater, not a third layout: it reconciles by key then lays out through the `row`/`column` arm this names, so spacing, margins, `align_h`/`align_v`, and `Stretch` behave identically to a hand-built one)

> A `list` scrolls: clipping cuts a subtree to its parent's box (`layout::paint::paint_tree` pushes `intersect_scissor` per node), and ADR-0069 added the offset the layout pass clamps and the wheel input driving it. Bind `scroll` on the `list` to make it a viewport. The rejected alternative to `direction` was splicing generated children into the *parent's* child list; it conflicts with ADR-0045's identity rule, a node is identified by its position in one parent's child list, and a spliced list has no single such list to hold a position in.

#### 8. `textfield` (the engine security exception)
An IME-aware native input field mapped directly to Rust-owned `wp-text-input-v3`.
*   `placeholder`: `string`
*   `mask_character`: `string` (Capped at 1 byte; if specified, hides typed input)
*   `secure_submit`: `table` (`{ capability, action }`; see § 5's `textfield` glossary entry in `CONTEXT.md`. Only meaningful alongside `mask_character`, without it a masked field's value is unreadable from Lua entirely)
*   `on_change`: `function` (Lua callback on each committed edit batch from `wp-text-input-v3`, not per keystroke; IME composition is not character-by-character. Key events are swallowed inside Rust's memory blocks during sensitive lock states)
*   `on_submit`: `function` (Fires on `zwp_text_input_v3`'s protocol-native `submit` action, e.g. Enter, IME-correct rather than a raw keystroke check. Takes the committed text as its one argument, *except* when both `mask_character` and `secure_submit` are set: fires with no argument, since the Renderer's IPC layer attaches the native input buffer directly to the named capability/action envelope instead. ADR-0005, ADR-0027)

## 6. Top-level surface nodes

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
*   `exclusive`: `boolean` / `string` (`true` reserves physical screen area along the anchored edge, sized from the surface. `false` (default) reserves none, but the compositor still keeps the surface inside the area other surfaces reserved. `"Ignore"` reserves none and ignores what others reserved, so the surface covers the whole output; a full-screen wallpaper needs it, since a surface anchored to all four edges has no single edge to reserve against and so gets nothing from `true`. Layer-shell's three exclusive-zone cases: a positive zone, `0`, and `-1`)
*   `height`: `integer` / `string` (Explicit height or `"Fill"`)
*   `width`: `integer` / `string` (Explicit width or `"Fill"`)
*   `margin`: `table` (`{ top, right, bottom, left }` offsets from the anchored edges. Distinct from a node's `padding`, which is inside the surface: `margin` moves the surface itself, so a floating panel inset from a screen edge needs it)
*   `monitor`: `string` (A specific output EDID, or `"All"` to spawn on all monitors)
*   `namespace`: `string` (The layer-shell namespace the compositor sees; compositor rules match on it, e.g. Hyprland's `layerrule` for blur and animations. Defaults to `"oblisk-{id}"`)
*   `keyboard_interactivity`: `string` (`"None"` (default), `"OnDemand"`, or `"Exclusive"`, mapping to layer-shell's own field. A launcher or any surface accepting typed input needs `"OnDemand"` or `"Exclusive"`; `"None"` never receives key events)
*   `visible`: `boolean` / `Signal` (Unmaps the surface when false, without destroying it. How a config shows and hides a panel without churning Wayland objects)
*   `child`: `node` (The root visual primitive node inside this window)

### 6.2 `window`
A standard toplevel window (`xdg_toplevel`), the kind the compositor tiles, stacks, and lists in a task switcher. For a settings window or standalone dialog, where a `panel` would be wrong.
*   `id`: `string` (Unique identifier)
*   `title`: `string` / `Signal` (Window title the compositor displays)
*   `app_id`: `string` (Application identifier the compositor matches rules against, e.g. `"oblisk.settings"`)
*   `min_size`: `table` (`{ width, height }`. Advisory: the spec states a client "should not rely on the compositor to obey" it)
*   `max_size`: `table` (`{ width, height }`. Advisory, same as `min_size`)
*   `on_close`: `function` (Fires when the compositor asks the window to close. A request, not a command: the callback may decline by doing nothing, and the window stays open until the config sets `visible = false`)
*   `visible`: `boolean` / `Signal`
*   `child`: `node`

Decorations are not requested per window: Oblisk asks the compositor for server-side decorations once and accepts whatever mode it grants, drawing no titlebar of its own (ADR-0040).

### 6.3 `popup`
A real popup (`xdg_popup`), positioned by the compositor relative to its parent and dismissed by the compositor on click-outside. Parents to either a `panel` or a `window`, so a bar can own a genuine dropdown rather than a hand-positioned second panel.
*   `id`: `string` (Unique identifier)
*   `parent`: `string` (The `id` of the `panel` or `window` this popup anchors to)
*   `anchor_rect`: `table` (`{ x, y, width, height }` in the parent surface's logical coordinates. Required, non-zero. Normally passed straight from the rect `button`'s `on_click` hands back, so a dropdown lands on the button that opened it)
*   `width` / `height`: `integer` (Required and non-zero; a popup has no `"Fill"`)
*   `anchor`: `string` (Which edge or corner of `anchor_rect` the popup hangs from: `"Top"`, `"Bottom"`, `"Left"`, `"Right"`, `"TopLeft"`, and so on, or `"Center"`)
*   `gravity`: `string` (Which direction the popup extends from that point, same value set as `anchor`)
*   `constraint_adjustment`: `table` (Array of `"SlideX"`, `"SlideY"`, `"FlipX"`, `"FlipY"`, `"ResizeX"`, `"ResizeY"` naming how the compositor may move the popup to keep it on screen. Defaults to `{ "FlipY", "SlideX" }`, dropdown behavior; the protocol's own default is no adjustment. Applied in fixed precedence: flip, then slide, then resize)
*   `offset`: `table` (`{ x, y }` pixel nudge applied after anchor and gravity)
*   `grab`: `boolean` (Default `true`. Takes an explicit grab, giving the popup keyboard focus and letting the compositor dismiss it on click-outside. A compositor may deny the grab, dismissing the popup immediately and firing `on_dismiss`; a normal outcome, not an error)
*   `on_dismiss`: `function` (Fires when the compositor dismisses the popup)
*   `child`: `node`

A popup may only open in response to real user input, so `grab = true` outside an input callback is rejected. Nested popups close in reverse order of opening.

### 6.4 `lock`
A session-lock surface (`ext_session_lock_surface_v1`). One per output, created when the session locks and destroyed on unlock. While locked the compositor shows only these, so a `panel` cannot be part of a lock screen (ADR-0042).
*   `id`: `string` (Unique identifier)
*   `child`: `node` (The lock screen's node tree, authored like any other)

No `visible`, `monitor`, `anchor`, or size: a lock surface covers its output, exists on every output, and its lifetime is the lock's, not the config's. Authentication runs through a `textfield` with `secure_submit` (§ 5.2 item 8), so the password reaches the Supervisor's PAM worker without entering the Lua VM (ADR-0005, ADR-0028, ADR-0042). Locking is triggered by the Supervisor, not by returning this node; declaring it says what the lock screen looks like, not when it appears.

---

## 7. The generation-guarded command envelope contract

To keep system state consistent during concurrent, overlapping hot-reloads, Oblisk applies a generational guard on the IPC command write channel.

### 7.1 Seamless Lua integration (the mlua boundary)
The Lua config never manages or appends generation-tracking parameters. The Rust `mlua` host intercepts all method invocations on exported singletons and transparently wraps them in a **generation-guarded envelope** before serializing to the control socket.

### 7.2 Guarded JSON-RPC 2.0 envelope schema
Every write transaction carries the sender's active **generation id** and the target capability's **revision sequence number**:

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

### 7.3 Generational gating fields
*   `generation_id`: `integer` (The chronological process epoch index the Supervisor assigns the Renderer instance on boot)
*   `expected_revision`: `integer` (The logical revision index of the target capability's state snapshot this command is reacting to)
*   **The guard rule**: the Supervisor keeps a chronological ledger of active Renderer generations. A command whose `generation_id` is less than the current active generation, or whose `expected_revision` is stale, is dropped instantly, preventing a dying process from racing or duplicating commands during a transition swap.
