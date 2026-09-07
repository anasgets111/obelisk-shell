# Lua API

This is the config-author reference. [Services](oblisk-supervisor-services-dbus.md) owns backend
behavior and reload lifetimes; [roadmap](roadmap.md) owns gaps and proposed work;
[CONTEXT](../CONTEXT.md) owns terminology; [decisions](decisions.md) owns history.

Exact capability fields and action names come from Rust types through
[generated editor stubs](../supervisor/src/stubs.rs), installed by `oblisk init`.
Keep schema inventories there rather than maintaining a second copy in Markdown.

## 1. Values and signals

### 1.1 Config VM

Lua 5.4 provides `coroutine`, `table`, `string`, `utf8`, `math`, and config-local
`require`. `os` exposes `time`, `date`, `clock`, and `getenv`.
There is no `io`, `debug`, FFI or native module loading.
Config modules reload on re-evaluation.

`json.decode(text)` returns a value, or nil and an error string. JSON null maps to Lua nil.
There is no `json.encode`. Lua 5.4 `require` can return loader data as a second result:
bind modules to locals before returning a surface array.

Lua-authored scalar signals reject nonfinite numbers, integers outside `[-(2^53-1), 2^53-1]`, and strings
over 64 KiB. These are scalar checks, not recursive validation of every table.
Node properties and command arguments have their own parsers.
See [VM setup](../renderer/src/lua/mod.rs), [JSON conversion](../renderer/src/lua/json.rs),
[scalar checks](../renderer/src/lua/marshal.rs) and [signal validation](../renderer/src/lua/signal.rs).

### 1.2 Reactivity

| Expression | Meaning |
| :--- | :--- |
| `signal:get()` | Current value; storing this result does not create a live property |
| `signal:map(fn)` | Derived signal; keep the callback free of side effects |
| `computed({signals...}, fn)` | Derived signal with explicit dependencies |
| `state(name, initial)` | Writable signal; `:set(value)` marks the scene dirty |
| `oblisk.<capability>:on_change(fn)` | Runs `fn(current, previous)` per pushed snapshot; may invoke actions or write state |

Pass the signal itself to a node property to keep it live:

```lua
text { content = oblisk.keyboard.active_layout }
```

A capability reads nil until hydrated; maps must handle it. A property resolving to nil uses its
default. Derived callbacks have a 5 ms CPU budget. Any dirty signal currently triggers scene-wide
resolution; there is no dependency-based layout invalidation.

Named state survives an in-place reload when its seed is unchanged. Changing a comparable scalar
seed resets it; fresh table identities do not. It dies with the generation.
Capability change handlers are cleared and registered again on evaluation.

## 2. Capability state

Read state through `oblisk.<name>`; reading requests backend startup. Started backends remain
for the Supervisor's lifetime. These links lead to the actual serialized state definitions.

| Capability | State definition |
| :--- | :--- |
| `keyboard` | [Layout, lock LEDs and backlight](../supervisor/src/capabilities/keyboard/controller.rs) |
| `battery` | [Composite battery and estimates](../supervisor/src/capabilities/battery/controller.rs) |
| `brightness` | [Backlight percentage](../supervisor/src/capabilities/brightness/controller.rs) |
| `audio` | [Devices, defaults and app mixer](../supervisor/src/capabilities/audio/mixer/state.rs) |
| `network` | [Connection, scan and credential-request state](../supervisor/src/capabilities/network/mod.rs) |
| `bluetooth` | [Adapter and device state](../supervisor/src/capabilities/bluetooth/mod.rs) |
| `notifications` | [Feed, spans, actions and DND](../supervisor/src/capabilities/notifications/mod.rs) |
| `mpris` | [Player metadata and position](../supervisor/src/capabilities/mpris/player.rs) |
| `workspaces` | [Per-output workspaces and active client](../supervisor/src/capabilities/workspaces/controller.rs) |
| `system` | [Clock](../supervisor/src/capabilities/system/controller.rs) |
| `sysinfo` | [CPU, memory and temperatures](../supervisor/src/capabilities/sysinfo/controller.rs) |
| `power` | [Profiles and energy state](../supervisor/src/capabilities/power/controller.rs) |
| `tray` | [Items](../supervisor/src/capabilities/tray/item.rs) and [menus](../supervisor/src/capabilities/tray/menu.rs) |
| `applications` | [Desktop entries and app-ID index](../supervisor/src/capabilities/applications/controller.rs) |
| `files` | [Watched directory listings](../supervisor/src/capabilities/files/controller.rs) |
| `storage` | [Declared JSON files](../supervisor/src/capabilities/storage/controller.rs) |
| `privacy` | [Camera, microphone and screencast users](../supervisor/src/capabilities/privacy/controller.rs) |
| `idle` | [Inhibition and external holders](../supervisor/src/capabilities/idle/state.rs) |
| `lock` | [Lock/authentication state](../supervisor/src/capabilities/lock/mod.rs) |
| `polkit` | [Authentication challenge](../supervisor/src/capabilities/polkit.rs) |
| `updates` | [Checks, packages and install progress](../supervisor/src/capabilities/updates/controller.rs) |

Renderer-owned members are separate: `oblisk.rescue` carries `is_rescue` and `error_log` for reload
failures; `oblisk.screens` carries output information. `oblisk.version` is a plain `{ major, minor, patch }`
table; `oblisk.config_dir` is the loaded config directory path. See [namespace](../renderer/src/lua/namespace.rs)
and [output state](../renderer/src/wayland/output.rs).

## 3. Actions and I/O

### 3.1 Calling a capability

```lua
oblisk.audio:invoke("set_volume", 0.5)
oblisk.applications:invoke("launch", app_id)
```

Arguments follow the action name. There is no `capability:action(...)` sugar and no synchronous
result from `invoke`; observe capability state for outcomes. Unknown actions and malformed
arguments are logged and dropped by dispatch.

### 3.2 Action arguments

Positional arguments validated by capability dispatch. Read-only capabilities have no actions.

| Capability | Actions |
| :--- | :--- |
| `audio` | `set_volume(volume)`, `set_muted(bool)`, `toggle_mute()`, `set_default_sink(id)`, `set_default_source(id)`, `set_source_volume(volume)`, `set_source_muted(bool)`, `toggle_source_mute()`, `set_app_volume(id, volume)`, `set_app_muted(id, bool)` |
| `brightness` | `set(percent)` |
| `keyboard` | `set_backlight(percent)`, `switch_layout(index)` |
| `network` | `set_networking_enabled(bool)`, `set_wifi_enabled(bool)`, `set_ethernet_enabled(bool)`, `scan()`, `connect(ssid, hidden)`, `cancel_connect()`, `forget(ssid)` |
| `bluetooth` | `set_enabled(bool)`, `start_discovery()`, `stop_discovery()`, `pair(mac)`, `connect(mac)`, `disconnect(mac)`, `forget(mac)` |
| `notifications` | `dismiss(id)`, `invoke_action(id, key)`, `reply(id, text)`, `set_sound(urgency, path)`, `set_dnd(bool)`, `hold_expiry(seconds)` |
| `mpris` | `control(id, command)`, `seek(id, position_us)`, `seek_relative(id, offset_us)` |
| `workspaces` | `focus(id)`, `toggle_special(name)` |
| `applications` | `refresh()`, `launch(id)`, `open_url(url)` |
| `files` | `watch(path, extensions?)`, `unwatch(path)` |
| `sysinfo` | `configure({ cpu_interval?, ram_interval?, temp_interval? })` |
| `updates` | `check()`, `configure({ interval, checked_at?, packages? })`, `install()` |
| `power` | `set_profile(name)` |
| `tray` | `activate(id, x, y)`, `secondary_activate(id, x, y)`, `scroll(id, delta, orientation)`, `menu_will_show(id, submenu_id)`, `activate_menu_item(id, menu_item_id)` |
| `lock` | `lock()` |
| `polkit` | `cancel()` |

Device, player, app, tray and notification targets use snapshot IDs.
Volumes use 0–1; percentages use 0–100; layout indices are zero-based.
MPRIS commands accept `play`, `pause`, `play_pause`, `next`, `previous`; seeks take microseconds.
File watches take an absolute directory path and optional dot-free extensions.
Authentication for `lock` and `polkit` uses native secure submission instead of action arguments.

### 3.3 Dedicated APIs

| API | Contract |
| :--- | :--- |
| `oblisk.idle:register_threshold(seconds, on_idle, on_resume)` | Register inactivity callbacks; reset on re-evaluation |
| `oblisk.idle:inhibit(reason)` / `release_inhibit()` | Acquire/release one generation-owned hold on logind idle inhibition |
| `persistent_table { path, name, defaults }` | Absolute directory and filename; defaults fill missing keys |
| `store.key` / `store:set(key, value)` | Live key signal / write; nil deletes a key; `set` is reserved |
| `process.run(cmd, args, out_cb, exit_cb)` | Spawns a process group; streams lines to `out_cb(line, stream)`; calls `exit_cb(code)`; returns `{ kill() }` |

See [idle wrapper](../renderer/src/lua/idle.rs), [store wrapper](../renderer/src/lua/store.rs) and [process API](../renderer/src/lua/process.rs).
Persistence debounce and process group reaping belong to [services](oblisk-supervisor-services-dbus.md).

## 4. Surface lifecycle

A config returns a surface declaration or an array of them. An empty return is valid.
The declared set is fixed for a generation; `visible` toggles mapping without destroying surfaces.
Topology changes, output hotplug and failure handling belong to
[services](oblisk-supervisor-services-dbus.md#14-reload-lifecycle).

## 5. UI nodes

### 5.1 Shared properties

Only names in the [node allowlist](../renderer/src/lua/nodes.rs) are accepted.
Properties can carry signals except structural identity/topology fields.
The `hover` and `scroll` properties take dedicated handles; signals inside nested property
tables do not resolve, so derive the whole table instead.

| Property | Values / behavior |
| :--- | :--- |
| `width`, `height` | Number of logical pixels, `"Fill"`, or `"NN%"`; omit for content sizing. `"Content"` is not a literal |
| `max_width`, `max_height` | Numeric size ceilings |
| `padding`, `margin` | Number or `{ top, right, bottom, left }`; unspecified edges are zero |
| `align_h`, `align_v` | `"Start"`, `"Center"`, `"End"`, `"Stretch"` |
| `visible` | Boolean; false removes the node from layout and paint |
| `opacity` | 0–1, default 1; inherited multiplicatively. Zero still occupies space and takes input |
| `id` | Optional identity unique among siblings; unidentified siblings match positionally |
| `cursor` | CSS cursor name; innermost explicit/default cursor wins |
| `hover` | Handle from `hover(name)` |
| `on_hover` | `function(inside)` on hover edges |
| `animate` | `{ <property> = ms \| { duration = ms, easing = "<name>" } }`; named properties ease from the displayed value to a newly resolved one instead of snapping |

Sizes and maximum sizes accept 0–8192 logical pixels. See
[geometry parsing](../renderer/src/layout/node/style.rs).

`animate` names numeric properties (`width`, `height`, `max_width`, `max_height`, `margin`,
`padding`, `spacing`, `radius`, `border_width`, `opacity`, `font_size`, `size`) and colour
properties (`background`, `border_color`, `foreground`). Durations are `(0, 60000]` ms. Easing
names are QML's without the prefix: `Linear`, `InQuad`, `OutQuad`, `InOutQuad` (default),
`InCubic`, `OutCubic`, `InOutCubic`, `OutBack`. A tween runs
between the engine's own passes on compositor frame callbacks and reads no Lua; a first value, a
`"Fill"`/percent/edge-table endpoint and a property `animate` stops naming all snap. Any other
property is refused. See [tweens](../renderer/src/layout/node/animate.rs).

Boxes, rows, columns, buttons and surface roots also accept `background`, `radius`,
`border_color`, `border_width` and `clip`.
Colours use `#RRGGBB` or `#RRGGBBAA`. Borders may specify per-edge colours/widths;
an edge needs both. `clip = "Box"` is the default; `"Rounded"` clips children with the radius.
See [paint parsing](../renderer/src/layout/node/paint_style.rs).

### 5.2 Node-specific properties

| Node | Properties / behavior |
| :--- | :--- |
| `rect` | `children`; box painting |
| `row` | `children`, `spacing`, `scroll`; horizontal flow |
| `column` | `children`, `spacing`, `scroll`; vertical flow |
| `text` | `content`, `font_size`, `foreground`, `text_align`, `elide`, `wrap`, `max_lines`, `on_link` |
| `icon` | `name`, `size`, `foreground`; name is a theme name or absolute image path |
| `image` | `source`, `fit`, `async`; fit is `"cover"` by default, `"contain"` or `"stretch"` |
| `button` | `children`, `on_click`, `on_drag`, `on_wheel`, `submit` |
| `list` | `source`, `itemfn`, optional `key`, `direction`, `spacing`, `scroll` |
| `textfield` | `placeholder`, `font_size`, `foreground`, `text_align`, `autofocus`, `on_change`, `on_submit`, `on_cancel`, `on_navigate`, `secure_submit`, `mask_character` |

`text.content` is a string or runs `{ text, bold?, italic?, underline?, color?, href? }`.
`on_link(url)` handles activation. `text_align` is Start/Center/End; `elide` is None/End;
`wrap` is None/Word. `max_lines = 0` is unlimited; wrapping needs a bounded width.
Text size and icon size default to 12. Text and icon content defaults can render empty before hydration.

`image.async = true` decodes off-thread and draws nothing until ready; false is the default.
Icons resolve in the Renderer. See [content parsing](../renderer/src/layout/node/content.rs).

A list calls `itemfn(element)` for every source element, including offscreen items.
`key(element)` must return a unique sibling string; without it identity is positional.
Direction is `"Vertical"` by default or `"Horizontal"`.
See [list construction](../renderer/src/layout/node/spec.rs).

### 5.3 Input and local state

| API | Contract |
| :--- | :--- |
| `on_click(rect, button)` | Rect is surface-local `{ x, y, width, height }`; button is left/right/middle |
| `on_drag(rect, pointer, phase)` | Left drag; pointer is button-local and unclamped; phase is start/move/end |
| `on_wheel(rect, steps)` | Vertical notches, including fractional touchpad steps; positive increases an upward-stepped control |
| `hover(name)` | Read-only boolean handle; bind it to a node's `hover` |
| `hover_rect(name)` | Surface-local rect signal; retains the last rect after leave |
| `scroll(name)` | Read-only logical offset; bind it to a row, column or list |
| `scroll(name):reveal(index)` | Request visibility of a 1-based child; layout clamps the offset |
| `fonts { families... }` | Dense ordered family list, applied at generation startup; no per-node font family |

A click fires on release inside the pressed target. Drag ends on release or surface leave;
a left click can also fire after drag end. The innermost wheel handler or scrolling container wins.
A button with `submit = true` submits the scope's armed `secure_submit` field.
See [input handling](../renderer/src/wayland/input.rs).

Ordinary text fields expose their full draft through `on_change(text)` and `on_submit(text)`.
Submit empties the draft; losing focus preserves it. Escape clears it, and `on_cancel` also drops
focus. Navigation callbacks receive up/down/page_up/page_down/tab/backtab.

`secure_submit = { capability, action }` selects the native secret path.
`mask_character` alone does not make a field secure.
The default mask is a bullet; an empty glyph hides length.
Supported targets are lock/authenticate, polkit/authenticate and network/connect.
Secrets never reach Lua callbacks. See [secure target parsing](../renderer/src/layout/node/spec.rs)
and [authentication ownership](oblisk-supervisor-services-dbus.md#7-idle-lock-and-polkit).

## 6. Surface declarations

Each role has `id` and `child`; top-level IDs are unique. Declare at most one lock.
Panels and locks have per-output instances; windows and popups have one instance each.

| Role | Protocol | Additional properties |
| :--- | :--- | :--- |
| `panel` | layer-shell | `layer`, `anchor`, `monitor`, `namespace`, `exclusive`, `keyboard_interactivity` |
| `window` | xdg_toplevel | `title`, `app_id`, `min_size`, `max_size`, `on_close` |
| `popup` | xdg_popup | `parent`, `anchor_rect`, `anchor`, `gravity`, `constraint_adjustment`, `offset`, `grab`, `on_dismiss` |
| `lock` | ext_session_lock_surface_v1 | Output coverage and lifetime are protocol-controlled |

Panel layers are Background/Bottom/Top/Overlay; anchors are edge booleans.
Monitor is a connector name or `"All"`. Exclusive is false, true, or `"Ignore"` to ignore
others' reserved space. Keyboard interactivity is None/OnDemand/Exclusive.
`child = function(output)` on panels/locks builds per-output content; nil yields an empty instance.

Windows/popups use `visible` to open/close. A window's `on_close` is a request the config handles;
min/max sizes are compositor hints. Popup parent is a panel or window ID; anchor rect and
width/height must be nonzero. Anchors/gravity accept edges, corners or Center.
Constraint adjustments accept SlideX/Y, FlipX/Y, ResizeX/Y; default is FlipY and SlideX.
`grab` defaults true and needs an input serial; use false for hover-opened tooltips.

See [surface parsing](../renderer/src/lua/surfaces.rs),
[panel properties](../renderer/src/layout/node/surface.rs),
[window/popup properties](../renderer/src/layout/node/toplevel.rs) and
[instance expansion](../renderer/src/layout/instance.rs).

## 7. Wire format envelope

Lua capability invocations serialize to a JSON-RPC 2.0 envelope over the control socket.
See [wire format and dispatch limits](oblisk-supervisor-services-dbus.md#13-control-socket-and-wire-format).

## 8. Tooling

| Command | Behavior |
| :--- | :--- |
| `oblisk init -c <dir>` | Config/editor setup; generates capability field and action stubs |
| `oblisk check -c <dir>` | Evaluates config/surface declarations without Wayland, GPU or subprocess execution |
| `oblisk set <name> <value>` | Writes declared named state; parses JSON, otherwise uses a string |
| `oblisk toggle <name>` | Toggles declared boolean state |

`check` does not validate live service behavior or rendered layout.
See [CLI](../supervisor/src/cli.rs) and [check implementation](../renderer/src/check.rs).
