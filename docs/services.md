# Supervisor services

Current ownership and backend behavior. Lua call syntax and schemas are in the
[API specification](lua-api.md); proposed work is in the [roadmap](roadmap.md).
[Decisions](decisions.md) records rationale and history.

The Supervisor owns durable platform connections and validates commands. Capabilities start on
demand and remain started for the Supervisor's lifetime. State is event-driven where the backend
supports it; the clock, hardware telemetry and update checks use their own schedules.
Lua owns presentation and user policy.

Capability commands use `oblisk.<name>:invoke("action", ...)`. The exceptions are the
dedicated idle methods, `persistent_table` and `process.run`.

## 1. Notifications

The Supervisor claims `org.freedesktop.Notifications` on the session bus.

### 1.1 Queue and sanitation

| Contract | Current behavior |
| :--- | :--- |
| Retention | 100-entry FIFO queue; `feed` exposes the most recent 20 |
| Text bounds | App name 64 bytes, summary 128, body 512; truncation respects UTF-8 boundaries |
| Body | Allowlisted text/image spans, not arbitrary HTML; supports bold, italic, underline, links and trusted image paths |
| Pictures | `image_path` is separate from `app_icon`; raw pixels are validated and spooled as PNG |
| Identity | ID, arrival timestamp, desktop entry, urgency and action/reply metadata accompany each entry |
| Expiry | Marks a retained entry expired; expired transient entries are removed |
| DND | Gates sound only; critical urgency bypasses DND and automatic expiry |
| Sounds | Configured per urgency; a trusted client sound-file can override it, suppress-sound silences it; no sound-theme lookup |

Spooled files live under `$XDG_RUNTIME_DIR/oblisk/notifications/`.
The [notification types and limits](../supervisor/src/capabilities/notifications/mod.rs)
and [markup validator](../supervisor/src/capabilities/notifications/markup.rs) define the exact fields.

### 1.2 Actions and replies

`invoke_action(id, key)` accepts a declared action and removes the entry unless it is resident.
`reply(id, text)` emits `NotificationReplied(id, text)` and removes the entry unless it is resident.
`dismiss(id)` removes it explicitly. Removal signals and expiry are owned by the Supervisor.

`hold_expiry(seconds)` pauses pending expiry countdowns for up to 300 seconds; zero releases the
hold. Popup filtering, history presentation and grouping belong to Lua.

## 2. System tray

The Supervisor hosts `org.kde.StatusNotifierWatcher` and reads each item's own object path.

### 2.1 Icons and menus

| Contract | Current behavior |
| :--- | :--- |
| Icon selection | Item-local theme path, then theme name, then validated pixmap fallback |
| Pixmap validation | Positive square size, at most 128×128, exactly width × height × 4 ARGB bytes |
| Spooling | PNG under `$XDG_RUNTIME_DIR/oblisk/tray/`; Lua receives names or paths |
| Activation | `activate(id, x, y)`; menu-only items do not receive Activate; secondary activation and scroll are also supported |
| Menus | Recursive DBusMenu data; `menu_will_show` refreshes lazy content; `activate_menu_item` selects an item |

See [tray](../supervisor/src/capabilities/tray/mod.rs) and
[icon selection](../supervisor/src/capabilities/tray/icon.rs).

## 3. MPRIS

Session-bus name changes discover `org.mpris.MediaPlayer2.*` players. Property changes and
`Seeked` refresh cached metadata and position. The snapshot carries position in microseconds and
its monotonic timestamp; it does not provide a config-side high-frequency clock.

Supported controls are play, pause, play/pause, next, previous, absolute seek and relative seek.
See [controller](../supervisor/src/capabilities/mpris/controller.rs) and
[player state](../supervisor/src/capabilities/mpris/player.rs).

## 4. NetworkManager

The system-bus controller follows manager, device, wireless, active-access-point and active-connection
changes, rebuilding published state from those events.

Global networking toggles via `Enable`; Wi-Fi toggles via `WirelessEnabled`. Disabling Ethernet
disconnects wired devices; enabling activates existing autoconnect profiles.

Scanning is asynchronous. Results merge duplicate SSIDs and retain the connected AP plus the
strongest alternatives, capped at 20. Frequency supplies the band. Scan progress is published.

Saved profiles activate without duplication. Open networks need no credential. Secured connections
request native secure submission; credentials never enter Lua.
See [network](../supervisor/src/capabilities/network/mod.rs) and
[connection handling](../supervisor/src/capabilities/network/connection.rs).

## 5. BlueZ

### 5.1 Discovery and pairing

The system-bus ObjectManager and property changes maintain device state.
Supported actions are enable, start/stop discovery, pair, connect, disconnect and forget.
Stopping discovery preserves the last discovered list; starting it clears that list.

### 5.2 Battery and category

Battery1 supplies accessory percentage; device class supplies the display category.
Missing Bluetooth hardware/service degrades to an inert controller.

### 5.3 Codec coverage

`connected_devices[].codec` is currently nil. There is no accepted codec-selection action.
See [Bluetooth dispatch](../supervisor/src/capabilities/bluetooth/mod.rs).

## 6. PipeWire and privacy

Native PipeWire callbacks publish sinks, sources, default routing and stream volume/mute.
Commands set defaults and control output/input volume and mute.
Playback streams publish per-app volume and mute, targeting stream node IDs.

Input audio streams report microphone users (excluding monitor capture); output video streams report
screencast users. Camera detection combines video-device watching, process-fd inspection and
PipeWire name enrichment.
See [audio dispatch](../supervisor/src/capabilities/audio/mod.rs) and
[privacy](../supervisor/src/capabilities/privacy/mod.rs).

## 7. Idle, lock and Polkit

The Supervisor owns `ext_idle_notifier_v1`. Lua registers idle/resume callbacks per duration;
equal durations share a Wayland listener. Registrations reset on re-evaluation.

`oblisk.idle:inhibit(reason)` and `release_inhibit()` refcount one logind
`Inhibit(what="idle", mode="block")` fd across generation holds. Logind idle inhibition suppresses
threshold events and resumes reported thresholds. `idle.inhibited` reflects shell holds;
`idle.inhibitors` names external holders.

Logind's session Lock signal and config lock commands request the session lock flow. The Supervisor
owns lock decisions; the Renderer owns protocol surfaces. Only successful authentication authorizes
unlock. Renderer crashes cannot unlock the compositor.

Polkit agent registration is on-demand. Challenge state and cancel actions belong to the Supervisor;
secrets route directly from native input to the authentication helper.
See [idle](../supervisor/src/capabilities/idle/mod.rs),
[lock](../supervisor/src/capabilities/lock/mod.rs) and
[polkit](../supervisor/src/capabilities/polkit.rs).

## 8. Workspaces and active window

Compositor probing selects Niri or Hyprland modules. Workspace state includes per-output lists,
focus, population, Hyprland special workspaces, and the focused active client (title, app ID,
floating state, and Hyprland fullscreen). Actions focus a workspace or toggle special workspaces.
No complete window list is exposed.

`oblisk.screens` is Renderer-owned output state, not a display-configuration API.
See [workspaces](../supervisor/src/capabilities/workspaces/mod.rs) and
[output handling](../renderer/src/wayland/output.rs).

## 9. Telemetry and clock

`sysinfo` samples CPU (/proc/stat), RAM/swap (/proc/meminfo) and temperatures (/sys/class/hwmon/)
with configurable intervals; zero disables that sample task. `system.time` ticks once a second.
See [sysinfo](../supervisor/src/capabilities/sysinfo/mod.rs).

## 10. Processes

`process.run(cmd, args, out_cb, exit_cb)` spawns a separate process group and streams newline-stripped
lines. `out_cb(line, stream)` identifies the stream; `exit_cb(code)` uses nil for a signal exit.
The handle exposes `kill()`.

Generation retirement and Supervisor shutdown reap managed children using SIGTERM and a 100 ms grace
before SIGKILL. In-place reload preserves the generation without restarting processes.
See [process registry](../supervisor/src/process/registry.rs).

`session_process` declares the other lifetime. Those programs are held by `oblisk.processes` rather
than by a generation, so the retirement sweep never sees them; they survive every reload and are
reaped only at shutdown, with the signal each declaration named and a five-second grace before
SIGKILL. The longer grace is deliberate: a program is declared this way because it is doing
something long, and the first one to use it writes a video container it has to close on the way out.

One task per running program owns its `Child` and is the only place its pid is signalled, so no
signal can reach a recycled pid. That is what replaces the pid-plus-kernel-start-time bookkeeping a
config would otherwise need to re-find a program it had to orphan.
See [session processes](../supervisor/src/capabilities/processes/controller.rs).

## 11. Other capabilities

| Capability | Source / responsibility |
| :--- | :--- |
| `battery` | UPower DisplayDevice composite battery and time estimates |
| `power` | UPower energy state and power-profiles-daemon profile selection |
| `brightness` | Native backlight reading and control |
| `keyboard` | Lock LEDs, keyboard backlight and compositor layout switching |
| `applications` | Desktop entry indexing, app launching and URL opening |
| `updates` | Package-manager checking and install progress via backend trait |
| `files` | Config-requested directory listings followed through inotify |
| `processes` | Programs declared with `session_process`, owned across generation swaps |

See [capability registry](../supervisor/src/capabilities/mod.rs).

## 12. Paths and persistence

| Data | Location |
| :--- | :--- |
| Config | CLI `-c`, then shared config-path resolver |
| Declared JSON stores | Absolute path and filename chosen by `persistent_table` |
| Control socket / lock marker | `$XDG_RUNTIME_DIR` |
| Spooled images | `$XDG_RUNTIME_DIR/oblisk/<kind>/`; runtime fallback uses `/run/user/<uid>` |

Declared files push immediately and write 1 second after the last edit via temporary file and rename.
Pending saves do not flush at shutdown. See [storage](../supervisor/src/capabilities/storage/controller.rs).

## 13. Control socket and wire format

Communication between Renderer and Supervisor uses JSON-RPC 2.0 over a private Unix domain socket.
Commands carry generation and revision metadata:

```json
{
  "jsonrpc": "2.0",
  "method": "ExecuteCommand",
  "params": {
    "generation_id": 4,
    "capability": "audio",
    "action": "set_volume",
    "arguments": [0.5],
    "expected_revision": 42
  },
  "id": 105
}
```

Inbound frames are tagged with the connection's generation ID.
Ordinary command dispatch does not currently enforce sender authority or the envelope's
generation/revision claims. These fields are not authorization guarantees.
See [wire types](../shared/src/lib.rs), [socket](../supervisor/src/socket.rs) and
[dispatch](../supervisor/src/supervisor.rs).

## 14. Reload lifecycle

### 14.1 Evaluation

Config edits trigger evaluation in the current generation. Value changes reconcile in place;
topology changes require a candidate generation. Evaluation failure preserves the active scene and
reports via `oblisk.rescue`.

### 14.2 Candidate preparation and presentation

The candidate evaluates with dependency snapshots and prepares declared surfaces with null buffers.
A nonce-bound `ActivateDraw` from the Supervisor permits drawing. The candidate commits buffers with
`wp_presentation_feedback` and reports evidence back to the Supervisor upon display presentation.

### 14.3 Promotion

Promotion waits for evidence from every expected surface within one shared timeout.
Only then does authority transfer per surface, updating input focus and exclusive zones.
The superseded generation's surfaces are unmapped, and its process group and managed children are reaped.
See [reload](../supervisor/src/reload.rs) and [presentation handling](../renderer/src/wayland/output.rs).
