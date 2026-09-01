# Oblisk Supervisor Services Specification
## Durable System Services, Zero-Polling Monitors, and Feature-Complete Backends

This specification defines the low-level system services and D-Bus interfaces managed entirely inside the long-lived **Oblisk Supervisor** process. These backends are implemented in native Rust, running off-thread to provide zero-polling, sub-millisecond, event-driven reactive state updates and command execution pipelines to the Lua VM.

**None of them is built until a config asks for it** (ADR-0070). The Renderer sends a `StartCapability` frame the first time an evaluation reads `oblisk.<name>`, and that frame is what constructs the controller described below. A section here describes what a capability does once started, not what this process does at boot. With a config that reads nothing, this process connects to the system bus, binds the control socket, spawns the Renderer, and does nothing else: no bus name is claimed, no D-Bus subscription is made, no poll task runs, and no authentication agent is registered.

Registering the polkit authentication agent is gated on a `textfield` declaring `secure_submit = { capability = "polkit", ... }`, since polkit has no capability member to read, and every failure to register logs rather than stopping the process. "An authentication agent already exists for the given subject" is the normal answer beside any other desktop.

---

## 1. Durable D-Bus Notifications Server (`org.freedesktop.Notifications`)

The Supervisor claims and maintains ownership of the `org.freedesktop.Notifications` interface on the Session Bus, providing a robust, persistent notification server that survives Renderer reloads.

### 1.1 Memory-Bounded Queue & Sanitation
*   **Queue Cap**: Capped at exactly 100 active notifications in memory. It operates as a FIFO queue.
*   **Property Parsing**: Captures and truncates:
    *   `app_name` to 64 bytes.
    *   `summary` to 128 bytes.
    *   `body` to 512 bytes.
*   **Safe Plain-Text Preprocessing**: The Supervisor strips out all executable scripts, style tags, and image elements using a non-backtracking regex parser, passing clean plain text to the Lua notifications signal feed.
*   **ARGB Icon Rejection & SHM Spooling**: Raw image byte arrays (`image-data` hints) are rejected to prevent RAM exhaustion. If present, the bytes are written off-thread to a sandboxed RAM disk file `/dev/shm/oblisk-notifications/notif-{id}.png` and exposed to Lua as a standard path.

### 1.2 Interactive Inline Replies and Actions
When a client application broadcasts a notification requiring an inline reply (using the `x-kde-reply` hint or actions), the Supervisor tracks the interaction. It serializes `has_reply = true` inside the signal snapshot. 
When the user submits a text reply, the Renderer dispatches a `notifications:reply(id, text)` command. The Supervisor captures this and emits `ActionInvoked(id, "inline-reply", text)` back onto the D-Bus, completing the transaction cleanly.

---

## 2. System Tray Host (`StatusNotifierWatcher` & `StatusNotifierItem`)

The Supervisor registers the `org.kde.StatusNotifierWatcher` interface at path `/StatusNotifierWatcher` on the Session D-Bus to host tray icons natively without exposing Lua to raw D-Bus marshaling.

### 2.1 Watcher Mechanics & ARGB Decoding Security
*   **Buffer Bounds Checks**: Receives raw ARGB pixel streams from client items (e.g. `nm-applet`, `discord`). It verifies that `width == height` and `width * height * 4` matches the payload byte length.
*   **Size Limits**: Capped at 128x128 pixels. Excessively large streams are rejected.
*   **SHM Spooling**: Validated pixel buffers are written as static PNGs to `/dev/shm/oblisk-tray/{service_name}.png`.
*   **Dynamic State Synchronization**: The tray item list is pushed to Lua as `tray.items` containing only safe, pre-decoded PNG paths.
*   **Click Activations**: When the user clicks a tray icon, the Renderer sends a `tray:activate(id, x, y)` command. The Supervisor maps this and invokes `Activate(x, y)` on the client's D-Bus object path.

---

## 3. Persistent Media Controls (Supervisor-Owned MPRIS)

To prevent media widget flickering and track title drops during Renderer hot-reloads, MPRIS ownership is managed by the long-lived Supervisor process.

### 3.1 D-Bus Player Discovery & Properties Caching
*   The Supervisor listens to `org.freedesktop.DBus` name changes to automatically discover active sessions (e.g., Spotify, Audacious, MPV, Firefox) matching the `org.mpris.MediaPlayer2.*` prefix.
*   It subscribes to properties changes on `org.mpris.MediaPlayer2.Player` to cache playback status, track metadata, and volume levels.
*   **The Zero-Polling Progress Sync**:
    *   To keep IPC socket communication at a minimum and avoid eating CPU cycles with high-frequency progress polling, track seek values are calculated mathematically on-the-fly.
    *   The Supervisor caches three values: `position` (microseconds), `position_updated_at` (steady monotonic clock timestamp in microseconds), and `play_state` (`"Playing"`, `"Paused"`, `"Stopped"`).
    *   These are synchronized to Lua as read-only signals. The Lua Renderer computes the instant track progress dynamically using the formula:
        $$	ext{CurrentPosition} = 	ext{position} + (	ext{Instant::now()} - 	ext{position_updated_at})$$
        (This calculation is only executed when `play_state == "Playing"`, giving fluid 60Hz progress updates with zero socket traffic!)
    *   **External Seek Tracking**: If an external app changes the progress bar, the Supervisor captures the `Seeked` D-Bus signal instantly and updates the cached position and timestamp, synchronizing it down to Lua with zero delay.

---

## 4. Feature-Complete NetworkManager D-Bus Controller (`oblisk.network`)

The Supervisor interfaces directly with NetworkManager (`org.freedesktop.NetworkManager`) using event-driven D-Bus subscriptions to handle all user networking needs with zero polling.

### 4.1 Master Switches and Radio Controls
*   **Global Networking Switch**: Writing `network:set_networking_enabled(bool)` sets the `NetworkingEnabled` property on the primary NetworkManager interface, completely enabling/disabling all physical interfaces.
*   **Wi-Fi Radio Switch**: Sets `WirelessEnabled` to toggle the wireless transmitter.
*   **Ethernet Link Control**: Writing `network:set_ethernet_enabled(bool)` queries Ethernet devices (type `1`) and disconnects/connects link carriers.

### 4.2 Wi-Fi Scanning and Frequency Band Resolution
*   **Scan Triggering**: Lua calls `network:scan()`, dispatching a `RequestScan(options: Dict)` D-Bus method call off-thread.
*   **Live Scanning Signals**: The Supervisor monitors `PropertiesChanged` on the wireless device's interface. `network.scanning` is set to `true` on scan initiation, and updates to `false` when completed.
*   **Frequency Band Calculation**:
    *   Access point objects contain a `Frequency` property (in MHz).
    *   The Supervisor reads this value and normalizes it into a human-readable `band` string:
        *   If $	ext{Freq} \in [2400, 2500]	ext{ MHz}$ $
ightarrow$ `"2.4 GHz"`
        *   If $	ext{Freq} \in [4900, 5900]	ext{ MHz}$ $
ightarrow$ `"5 GHz"`
        *   If $	ext{Freq} \in [5925, 7125]	ext{ MHz}$ $
ightarrow$ `"6 GHz"`
    *   This allows the Lua UI to show appropriate band badges and filter frequencies.
*   **Results Normalization & Deduplication**: Merges duplicate SSIDs, retains the highest signal strength, and serializes the top 20 access points into `network.available_networks` with zero polling.

### 4.3 Hidden, Secure, and Open Network Associations
*   **Open Networks**: If connecting to a network without a password, the Supervisor checks settings, creates a minimal connection dictionary, and calls `AddAndActivateConnection2`.
*   **Secure Networks**: If a password is provided, it populates `802-11-wireless-security` with Key Management set to `"wpa-psk"` and passes the credential.
*   **Hidden Networks**: If `hidden` is specified as `true`, the Supervisor includes `hidden = true` and `scan-ssid = true` inside the connection dictionary parameters to force active probe broadcasts before association.
*   **Forget Profile**: Calling `network:forget(ssid)` queries existing connection settings profiles matching the target SSID and deletes them from the disk via `Delete()` on the profile object path.

---

## 5. Feature-Complete BlueZ Bluetooth Controller (`oblisk.bluetooth`)

The Supervisor binds to `org.bluez` on the System D-Bus to track and mutate local Bluetooth accessories.

### 5.1 ObjectManager Monitoring and Pairing
*   **Zero-Polling Status**: It registers a Session-wide listener on `org.freedesktop.DBus.ObjectManager`. It captures `InterfacesAdded` and `InterfacesRemoved` signals on `org.bluez.Device1` to instantly update discovered and connected pools.
*   **Forget Device**: Calling `bluetooth:forget(mac)` resolves the corresponding BlueZ device object path and calls `RemoveDevice(path)` on the active adapter (`org.bluez.Adapter1`), clearing paired credentials from disk.

### 5.2 Device Battery Telemetry and Device Categorization
*   **Battery Status**: Monitors the `org.bluez.Battery1` interface properties. On value changes, the Supervisor extracts the `Percentage` property and maps it to the device's signal entry.
*   **Device Categorization**: 
    *   To allow Lua configurations to render appropriate icons automatically, the Supervisor parses each device's D-Bus `Class` property (a 32-bit integer) and `Icon` property.
    *   It categorizes the device and populates the `category` signal field with one of: `"keyboard"`, `"mouse"`, `"headphones"`, `"headset"`, `"phone"`, `"computer"`, or `"generic"`.

### 5.3 PipeWire Audio Codec Control
*   When the user calls `bluetooth:set_audio_codec(mac, codec)`, the Supervisor's background audio thread finds the matching BlueZ SPA audio node in PipeWire.
*   It issues a `SetParam` command on the node with parameter ID `SPA_PARAM_Route`, specifying the target codec profile (`"LDAC"`, `"AAC"`, or `"SBC"`). PipeWire tears down and re-negotiates the Bluetooth link instantly.

---

## 6. Direct PipeWire Audio & Stream Mixer Controller (`oblisk.audio`)

Oblisk is strictly **PipeWire-only**, dropping legacy ALSA or PulseAudio server wrappers. The Supervisor hosts a background thread that binds natively to the PipeWire API.

### 6.1 Event-Driven Stream and Node Monitoring
The Supervisor registers event-driven callbacks on the PipeWire Registry (`pw_registry`).
*   **Mute & Volume Events**: On change, PipeWire broadcasts volume properties. The Supervisor catches these and updates `audio.volume` and `audio.muted` instantly.
*   **Default Node Routing**: 
    *   The list of output sinks and input sources is tracked and updated reactively.
    *   Calling `audio:set_default_sink(id)` or `audio:set_default_source(id)` dispatches a Metadata write transaction back to the default metadata node, routing physical streams with zero latency.

### 6.2 Application-Specific Audio Mixer (App Mixer)
*   The Supervisor tracks all playback audio nodes mapped to application streams (class `Stream/Output/Audio`).
*   It exposes them to Lua inside the `audio.apps` signal table.
*   Calling `audio:set_app_volume(id, volume)` or `audio:set_app_muted(id, bool)` targets the specific PipeWire node ID, modifying application sound levels without touching global system volume.

---

## 7. Durable Idle Capability (`ext-idle-notifier-v1`)

To provide complete design freedom to the user, Oblisk rejects hardcoded inactivity timeouts.

### 7.1 Dynamic, Multiple Threshold Registration
*   The Supervisor binds once to the Wayland compositor’s `ext_idle_notifier_v1` protocol interface.
*   When the Lua VM executes `idle:register_threshold(seconds, on_idle, on_resume)`, the Renderer dispatches a registration packet to the Supervisor over the IPC.
*   The Supervisor allocates a distinct `ext_idle_notification_v1` listener for the specified duration.
*   **The Handoff loop**:
    *   When the compositor broadcasts an inactivity state breach event (`ext_idle_notification_v1::idled`), the Supervisor captures it and pushes a simple JSON event payload containing the matched threshold duration down to the Renderer.
    *   The Renderer looks up the registered callback and executes `on_idle()` instantly.
    *   When user input is resumed on the seat, `resumed` triggers and executes `on_resume()`.
*   This architecture allows Lua to register unlimited custom thresholds (e.g. dim backlight after 30s, lock screen after 5m, DPMS sleep after 10m) with zero active timers or polling loops.
*   Registrations do not outlive an evaluation. Re-running `shell.lua` drops every callback the previous one registered, because they belong to the tree being replaced; the Supervisor keeps its listener, and re-registering the same duration is a no-op there.
*   Two registrations for the same duration are one Wayland listener and two callbacks. The Supervisor allocates per distinct duration and fans out; the event names the threshold, not the registration.

### 7.2 Idle Inhibit
*   `idle:inhibit(reason)` and `idle:release_inhibit()` hold off auto-suspend-on-idle through `org.freedesktop.login1.Manager.Inhibit(what="idle", mode="block")`, on the system bus the Supervisor already has (ADR-0032). Not the Wayland `idle-inhibit-unstable-v1` protocol, which inhibits per surface and would need the Renderer to own it.
*   The hold is a counted, per-generation reference on one logind fd: the fd opens on the 0-to-1 transition and closes on 1-to-0, so a media player and a presentation mode can both hold it without either release killing the other.
*   logind closes the fd if the holding process dies, so a Supervisor crash cannot leak a stuck inhibit.
*   Notify degrading to inert (no `ext_idle_notifier_v1`, a failed dedicated connection, a setup timeout) does not disable inhibit. The two halves share a controller, not a transport.

---

## 8. High-Performance Wallpaper Transition Engine

Wallpapers are rendered natively on the GPU within the Renderer process, utilizing double-buffered texture mapping to support seamless, hardware-accelerated animated transitions.

### 8.1 GPU Scaling, Fit, and Transition Algorithms
*   **GPU Box fitting**: All fitting algorithms—`"Cover"`, `"Contain"`, `"Stretch"`, `"Tile"`, `"Center"`, `"ScaleDown"`—are solved inside GLES3 fragment shaders, avoiding slow CPU pixel resizing.
*   **Dynamic Command Set**: Lua can dynamically change backgrounds at runtime using `wallpaper:set(monitor, filepath, [fit], [transition], [duration])`.
*   **Shader Transitions**: Executes smooth double-buffered transitions on the GPU, interpolating old and new textures via `"Crossfade"`, `"Slide"`, `"Sweep"`, or `"Zoom"` algorithms.

---

## 9. Compositor-Independent Active Window Tracking

To track the currently focused top-level application class and title without relying on compositor-specific sockets (like Hyprland's socket2 or Niri's JSON stream), the Renderer implements standard Wayland bindings:

### 9.1 Foreign Toplevel Protocols
*   The Renderer binds to **`ext-foreign-toplevel-list-v1`** or **`zwlr_foreign_toplevel_manager_v1`** if supported.
*   The Supervisor / Renderer handles window mapping, mapping classes, and titles into the reactive `workspaces.active_client` state dictionary dynamically.
*   It exposes properties `title`, `class`, `is_floating`, and `is_fullscreen` directly to Lua.

### 9.2 Asynchronous Icon Resolver Helper
*   The Supervisor manages an off-thread XDG desktop & icon theme lookup engine.
*   The helper `system:find_icon(app_id, name, fallback)` implements high-performance caching. On the first call:
    1. It maps `app_id` (e.g., `"Alacritty"` -> `"alacritty"`) to find registered XDG `.desktop` files in `/usr/share/applications/`.
    2. It extracts the `Icon` string from the file.
    3. It scans standard XDG icon directories (`/usr/share/icons/hicolor/scalable/apps/`, etc.) and system themes.
    4. It returns the absolute path of the resolved SVG/PNG icon, caching the mapping in an internal Rust LRU cache so subsequent queries resolve in under 0.1ms.

---

## 10. Compositor Workspace Adaptor Interface

Because workspaces and layout configurations are compositor-specific, the Supervisor isolates the compositor driver using a strict **Rust Workspace Trait**. 

```rust
pub trait WorkspaceAdaptor {
    fn query_outputs(&self) -> Vec<OutputState>;
    fn focus_workspace(&self, id: u32) -> Result<()>;
    fn move_window_to_workspace(&self, id: u32) -> Result<()>;
    fn toggle_special_workspace(&self, name: &str) -> Result<()>;
}
```

*   **Compositor Selection**: On startup, the Supervisor checks system environment variables (like `$HYPRLAND_INSTANCE_SIGNATURE` or `$NIRI_SOCKET`).
*   **Adaptor Loading**: It dynamically loads the matching driver (e.g. `HyprlandAdaptor` or `NiriAdaptor`), implementing the unified trait.
*   **Unyielding State Contract**: Compositor-specific IPC event broadcasts (such as Hyprland's `.socket2.sock` or Niri's RPC stream) are captured by the adaptor, normalized, and piped into the unified, reactive `workspaces.outputs` layout matrix, ensuring absolute API stability in Lua.

---

## 11. Configurable Telemetry Scheduler (`sysinfo`)

To prevent battery drain and waste CPU cycles on low-power devices, the Supervisor implements a dynamic asynchronous scheduler for hardware telemetry.

*   **Configurable Intervals**: The Lua layout controls ticks using `sysinfo:configure({ cpu_interval, ram_interval, temp_interval })`.
*   **Suspended Thread Execution**: If any interval parameter is configured as `0`, the Supervisor completely halts and suspends the corresponding background thread task.
*   **Efficient Sysfs Parsing**: CPU utilization is parsed off-thread from `/proc/stat`, RAM usage from `/proc/meminfo`, and temperatures from `/sys/class/hwmon/`, executing zero terminal-polling commands.

---

## 12. Asynchronous Non-Blocking Subprocess Stream Pipelines (`process`)

Oblisk manages non-blocking subprocess spawning cleanly using line-buffered streams.

*   **No Lua Blockage**: Reading lines from stdout/stderr triggers a non-blocking asynchronous select loop, feeding lines into Lua callbacks as they are emitted.
*   **Strict Process Group Lifecycle Gating**: To guarantee absolute safety during Renderer hot-reloads or VM failures:
    1. Every process spawned via `process.run` is assigned a distinct Unix process group (`setsid`).
    2. The Supervisor tracks active process handles.
    3. On configuration reload, the Supervisor sends `SIGTERM` to the process group (`kill(-pgid, SIGTERM)`).
    4. If the process does not terminate within a 100ms grace window, the Supervisor escalates to `SIGKILL`, completely eliminating zombie or orphaned background processes.

---

## 13. Unified Power & Thermals (`oblisk.power`)

Interfaces with **UPower** (on the system bus `org.freedesktop.UPower`) and **power-profiles-daemon** (`net.hadess.PowerProfiles`).
*   Exposes `power.active_profile` (Balanced, Performance, Power Saver), battery discharge rates in Watts (`power.energy_rate`), and power supply links.
*   Lua can execute `power:set_profile(name)` to switch profiles instantly.

---

## 14. XDG Directory Layout & Atomic State Manager

To maintain filesystem safety and protect user hardware, Oblisk separates static configs, state modifications, and dynamic cached assets into clean XDG directories:

| Path | XDG Baseline | Write State | Technical Content |
| :--- | :--- | :--- | :--- |
| `~/.config/oblisk/` | `$XDG_CONFIG_HOME` | **Read-Only** to engine | The config directory tree: `shell.lua` and every `.lua` file it `require`s (ADR-0047). |
| `~/.local/state/oblisk/` | `$XDG_STATE_HOME` | **Read-Write** | Flat interactive state file (`state.json`). |
| `/dev/shm/oblisk-$UID/` | RAM Memory-Disk | **Read-Write** (RAM) | Decoded notification images and icons. |

### 14.1 Atomic Writes for `state.json`
When Lua writes persistent states using `system:write_state(key, val)`:
1.  The Supervisor serializes the updated dictionary to `~/.local/state/oblisk/state.json.tmp`.
2.  It executes an OS flush (`std::fs::File::sync_all`).
3.  It calls an atomic rename (`std::fs::rename`) over `state.json`. This guarantees that your state database is never corrupted if your computer loses power mid-write.


## 15. Glitch-Free Renderer Hot-Reload and Overlapping Handoff Lifecycle

To achieve an invisible hot-reload with zero visual stutters, black frames, or desktop flashes, the Supervisor coordinates process switches using the **Presentation Before Authority (PBA)** protocol. This contract ensures that the compositor never experiences a gap in frame commits, and the active session remains fully interactive throughout the handoff.

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

### 15.1 Concurrent Overlapping Lifetimes
When a configuration file edit is detected (via the Supervisor's `inotify` watch on `~/.config/oblisk/`):
1.  The Supervisor **does not terminate** the active Renderer process (Generation `N`).
2.  Generation `N` remains fully authoritative, executing its active Lua VM, drawing layout nodes, and receiving all Wayland pointer/keyboard inputs.
3.  The Supervisor concurrently spawns the newly edited configuration inside a separate, isolated Candidate process (Generation `N+1`).

### 15.2 Null-Buffer Staging & Nonce-Bound Handshake
During startup, the Candidate process (Generation `N+1`) performs initial, non-visual setups:
1.  **Fast AST Evaluation**: It compiles and evaluates `shell.lua` [oblisk-reference-fixtures § 1]. High-overhead system queries are completely bypassed; instead, the Candidate instantly hydrates its signals using a state snapshot pushed by the Supervisor over the IPC.
2.  **Null-Buffer Registration**: The Candidate binds to the Wayland layer-shell protocol (`zwlr_layer_surface_v1`) [oblisk-idl-api-specs § 6.1]. It acknowledges the compositor’s initial `configure` dimensions, but it commits **null-buffers** to the compositor. The Candidate remains completely invisible, occupying zero physical on-screen display coordinates.
3.  **The Nonce Handshake**: Once initialization is complete, the Candidate signals its readiness to the Supervisor. The Supervisor verifies process integrity and writes a unique, nonce-bound **`ActivateDraw`** command over the private control socket.

### 15.3 Wayland Presentation-Feedback Verification
Upon receiving the activation nonce, the Candidate process triggers its GLES3/FemtoVG graphics rendering loop:
1.  It draws its initial layout textures on the GPU.
2.  It issues a buffer commit to the compositor, but crucially, it attaches a Wayland **`wp_presentation_feedback`** request to the committed buffer.
3.  **Hard Physical Evidence**: The Candidate waits for the hardware graphics controller to emit the `presented` event (verifying that the pixels have physically hit the screen phosphor on every single target monitor).
4.  Once received, the Candidate transmits this presentation evidence back to the Supervisor over the IPC.

### 15.4 The Swapping Seam & Instant Reaping
Only when the Supervisor has received verified presentation evidence across all connected displays does it execute the atomic swap:
1.  **Input Deselection**: The Supervisor commands Generation `N` to clear its input region. Generation `N` calls `wl_surface::set_input_region` with an empty bounds layout and drops focus.
2.  **Candidate Promotion**: The Supervisor promotes Generation `N+1` to authoritative. Generation `N+1` instantly claims active pointer focus and begins receiving seat keyboard inputs.
3.  **Stutter-Free Reaping**: The Supervisor dispatches `SIGTERM` to the process group of Generation `N`. If it does not exit within a 100ms grace window, it is reaped via `SIGKILL`. 

By delaying the destruction of Generation `N` until Generation `N+1` has verified physical screen mapping, Oblisk eliminates black screens, flashing boundaries, and coordinate mapping delays during hot-reloads.
