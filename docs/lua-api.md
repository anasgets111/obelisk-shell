# Lua API

This is the config-author reference. [Services](services.md) owns backend
behavior and reload lifetimes; [roadmap](roadmap.md) owns gaps and proposed work;
[CONTEXT](../CONTEXT.md) owns terminology; [decisions](decisions.md) owns history.

Exact capability fields and action names come from Rust types through
[generated editor stubs](../supervisor/src/stubs.rs), installed by `obelisk init`.
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
| `geometry(name)` | The laid-out `{ x, y, width, height }` of the node declaring `geometry = geometry(name)`, written by each layout; a pass that changes it earns one follow-up pass, a tween tick none |
| `delay(signal, ms)` | `signal` once it has held a new value for `ms`; a change that reverts sooner is dropped. Close-hold and trailing debounce in one shape |
| `pulse(signal, ms)` | `true` for `ms` after `signal` changes value, `false` otherwise; a change inside an open window restarts it. What fires a one-shot animation, since nothing here can call `restart()` |
| `obelisk.<capability>:on_change(fn)` | Runs `fn(current, previous)` per pushed snapshot; may invoke actions or write state |

Pass the signal itself to a node property to keep it live:

```lua
text { content = obelisk.keyboard.active_layout }
```

A capability reads nil until hydrated; maps must handle it. A property resolving to nil uses its
default. Derived callbacks have a 5 ms CPU budget. Any dirty signal currently triggers scene-wide
resolution; there is no dependency-based layout invalidation.

Named state survives an in-place reload when its seed is unchanged. Changing a comparable scalar
seed resets it; fresh table identities do not. It dies with the generation.
Capability change handlers are cleared and registered again on evaluation.

## 2. Capability state

Read state through `obelisk.<name>`; reading requests backend startup. Started backends remain
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
| `processes` | [Declared session processes](../supervisor/src/capabilities/processes/controller.rs) |
| `privacy` | [Camera, microphone and screencast users](../supervisor/src/capabilities/privacy/controller.rs) |
| `idle` | [Inhibition and external holders](../supervisor/src/capabilities/idle/state.rs) |
| `lock` | [Lock/authentication state](../supervisor/src/capabilities/lock/mod.rs) |
| `polkit` | [Authentication challenge](../supervisor/src/capabilities/polkit.rs) |
| `updates` | [Checks, packages and install progress](../supervisor/src/capabilities/updates/controller.rs) |

Renderer-owned members are separate: `obelisk.rescue` carries `is_rescue` and `error_log` for reload
failures; `obelisk.screens` carries output information. `obelisk.version` is a plain `{ major, minor, patch }`
table; `obelisk.config_dir` is the loaded config directory path. See [namespace](../renderer/src/lua/namespace.rs)
and [output state](../renderer/src/wayland/output.rs).

## 3. Actions and I/O

### 3.1 Calling a capability

```lua
obelisk.audio:invoke("set_volume", 0.5)
obelisk.applications:invoke("launch", app_id)
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
| `processes` | `declare(name, stop_signal?)`, `start(name, cmd, args?)`, `signal(name, signal)`, `stop(name)` |
| `updates` | `check()`, `configure({ interval, checked_at?, packages? })`, `install()` |
| `power` | `set_profile(name)` |
| `tray` | `activate(id, x, y)`, `secondary_activate(id, x, y)`, `scroll(id, delta, orientation)`, `menu_will_show(id, submenu_id)`, `activate_menu_item(id, menu_item_id)` |
| `lock` | `lock()` |
| `polkit` | `cancel()` |

Device, player, app, tray and notification targets use snapshot IDs.
Volumes use 0–1; percentages use 0–100; layout indices are zero-based.
MPRIS commands accept `play`, `pause`, `play_pause`, `next`, `previous`; seeks take microseconds.
File watches take an absolute directory path and optional dot-free extensions.
Session-process signals are named without their `SIG` prefix, from a closed list:
`TERM`, `INT`, `HUP`, `QUIT`, `USR1`, `USR2`, `KILL`, `STOP`, `CONT`.
Authentication for `lock` and `polkit` uses native secure submission instead of action arguments.

### 3.3 Dedicated APIs

| API | Contract |
| :--- | :--- |
| `obelisk.idle:register_threshold(seconds, on_idle, on_resume)` | Register inactivity callbacks; reset on re-evaluation |
| `obelisk.idle:inhibit(reason)` / `release_inhibit()` | Acquire/release one generation-owned hold on logind idle inhibition |
| `persistent_table { path, name, defaults }` | Absolute directory and filename; defaults fill missing keys |
| `store.key` / `store:set(key, value)` | Live key signal / write; nil deletes a key; `set` is reserved |
| `process.run(cmd, args, out_cb, exit_cb)` | Spawns a process group; streams lines to `out_cb(line, stream)`; calls `exit_cb(code)`; returns `{ kill() }` |
| `session_process { name, stop_signal? }` | Declares a program whose lifetime is the session's; returns a handle with `running`/`pid`/`started_at`/`exit_code`/`start_error` signals and `start`/`signal`/`stop` methods |

See [idle wrapper](../renderer/src/lua/idle.rs), [store wrapper](../renderer/src/lua/store.rs),
[session-process wrapper](../renderer/src/lua/session_process.rs) and [process API](../renderer/src/lua/process.rs).

`process.run` and `session_process` differ in lifetime, not in what they can launch. A
`process.run` child belongs to the generation that spawned it and its group is reaped on a
generation swap; a session process is held by the Supervisor, survives every reload, and is
reaped only at shutdown. In exchange a session process has no output callbacks -- its stdio is
inherited -- because the evaluation that started it is gone by the time most of its output
arrives.
Persistence debounce and process group reaping belong to [services](services.md).

## 4. Surface lifecycle

A config returns a surface declaration or an array of them. An empty return is valid.
The declared set is fixed for a generation; `visible` toggles mapping without destroying surfaces.
Topology changes, output hotplug and failure handling belong to
[services](services.md#14-reload-lifecycle).

## 5. UI nodes

### 5.1 Shared properties

Only names in the [node allowlist](../renderer/src/lua/nodes.rs) are accepted.
Properties can carry signals except structural identity/topology fields.
The `hover` and `scroll` properties take dedicated handles; signals inside nested property
tables do not resolve, so derive the whole table instead.

| Property | Values / behavior |
| :--- | :--- |
| `width`, `height` | Number of logical pixels, `"Fill"`, or `"NN%"`; omit for content sizing. `"Content"` is not a literal |
| `max_width`, `max_height` | Numeric size ceilings on a content-sized axis; overflow past one is what `scroll` scrolls |
| `min_width`, `min_height` | Numeric size floors on a content-sized axis. A floor above a ceiling wins, as in CSS |
| `padding`, `margin` | Number or `{ top, right, bottom, left }`; unspecified edges are zero |
| `align_h`, `align_v` | `"Start"`, `"Center"`, `"End"`, `"Stretch"` |
| `visible` | Boolean; false removes the node from layout and paint |
| `opacity` | 0–1, default 1; inherited multiplicatively. Zero still occupies space and takes input |
| `id` | Optional identity unique among siblings; unidentified siblings match positionally |
| `cursor` | CSS cursor name; innermost explicit/default cursor wins |
| `hover` | Handle from `hover(name)` |
| `on_hover` | `function(inside)` on hover edges |
| `geometry` | The signal `geometry(name)` returned; the pass writes this node's absolute rect into it |
| `animate` | `{ <property> = ms \| { duration = ms, easing = "<name>" \| { x1, y1, x2, y2 } \| { steps = n }, from = <value>, keyframes = { <value>, ... }, loops = n \| "Infinite" }, exit = { duration = ms, easing = "<name>", <property> = <target>, ... } }`; named properties ease from the displayed value to a newly resolved one instead of snapping; `from` is where a property nothing displayed yet starts; `exit` is where the node eases to once the tree drops it |
| `blur` | `true` asks the compositor to blur the desktop behind this node's box (ADR-0195). Opt-in and never inferred from a translucent `background`; the engine unions every asking node in a surface, following the transforms and clips it is painted under. Box-painting kinds and the four surface roles. Silently nothing without `ext-background-effect-v1`; strength and xray are the compositor's own configuration, which is why this is a boolean |
| `scale` | Paint-only scale about `origin`: a number for both axes or `{ x, y }`, `[0, 64]`; layout is untouched, hit-testing and input regions follow the painted box |
| `rotate` | Paint-only rotation in degrees, clockwise, about `origin`; `[-8192, 8192]` like every geometry number |
| `translate` | Paint-only `{ x, y }` shift in logical pixels, `[-8192, 8192]` each, applied after `scale` and `rotate` |
| `origin` | `{ x, y }` fractions of the node's box that `scale` and `rotate` pivot on, `[0, 1]` each, refused outside; default its centre |

Sizes and maximum sizes accept 0–8192 logical pixels. See
[geometry parsing](../renderer/src/layout/node/style.rs).

`animate` may name any property the node has; a name the node does not accept is refused. What
the value is decides whether it tweens, the way Qt registers interpolators by type: a number, a
`"NN%"` size, a `#` colour and an edge table of numbers each ease against a value of the same
shape, and anything else (`"Fill"`, a boolean, a table of colours, a change of shape) snaps.
Durations are `[1, 60000]` ms and an entry may hold the property still for a `delay` of
`[0, 60000]` ms first, which is CSS's `transition-delay` (ADR-0153); the delay is added to the
tween's life rather than taken out of it, and on a sequence it offsets the whole run once, not
each cycle. Easing names are QML's without the prefix: `Linear`, and `In`,
`Out` and `InOut` of `Quad` (`InOutQuad` is the default), `Cubic`, `Quart`, `Quint`, `Sine`,
`Expo`, `Circ`, `Back`, `Elastic` and `Bounce`. `Back`, `Elastic` and `Bounce` leave `[0, 1]` on
purpose; the property's own range pulls the result back. In place of a name, `easing` takes a
table: four numbers are CSS `cubic-bezier(x1, y1, x2, y2)`, with `x1` and `x2` bounded to `[0, 1]`
and the `y` free, and `{ steps = n }` is CSS `steps(n)`, `n` within `[1, 1000]`, holding each value
and reaching the target only at the end (ADR-0151). A tween runs
between the engine's own passes on compositor frame callbacks and reads no Lua. A property
nothing displayed yet, on a new node or one that lacked it, is taken as it is unless the entry
names a `from`, which is where it starts. A property `animate` stops naming snaps. See
[tweens](../renderer/src/layout/node/animate.rs).

An entry naming `spring` is a mass on a spring instead of a curve of progress (ADR-0154):
`{ spring = { stiffness = 220, damping = 26 } }`, both required, `stiffness` within
`(0, 100000]` and `damping` within `(0, 10000]`, with `2 * sqrt(stiffness)` the damping that stops
it overshooting. It has no `duration` and no `easing` — naming either beside a spring is refused,
as are `keyframes` and `loops` — because how long it takes falls out of the two constants. What a spring does
that no easing can is keep its speed through a change of target: an eased tween whose target moves
mid-flight starts a fresh curve from a standstill at the value on screen, while a spring hands its
running rate to the run that replaces it. Its overshoot is clamped by the property's own range,
the same way `OutBack`'s is. There is no `mass`: it divides out of both constants.

An entry naming `keyframes` walks the property through that list instead of easing it to the value
a pass resolved (ADR-0152): at least two values, the first where it starts and each later one a
segment eased into over the entry's `duration` and `easing`, or over its own when the frame is
written `{ value = <v>, duration = ms, easing = <easing> }`. A segment of `duration = 0` is a jump,
and one between two equal values is a hold, which together are QML's `PropertyAction` and
`PauseAnimation`; at least one segment must last, since a list of nothing but jumps takes no time
to walk and repeating it forever would ask for a frame every frame while showing one still value.
`loops` is a whole count within `[1, 10000]` or `"Infinite"`, `1` when absent, and is refused
without a `keyframes` list to count. A sequence takes the property over for as long as it runs and reads nothing resolved for
it; there is no separate `running` flag, because `animate` is itself bindable and an entry that is
not there is a sequence that is not running, leaving the property at its resolved value. A counted
sequence holds its last frame once it has played out and does not start again. `pulse(signal, ms)`
is what re-fires one: `animate = pulse(clicks, 400):map(function(on) return on and { ... } or {} end)`
puts the entry there for the length of the window and takes it away after, so the next change
starts the sequence over.

`animate.exit` is the one entry that is not a property name. Its block holds a `duration`, an
optional `easing` and `delay`, and the targets the node eases to once a pass stops returning it: a child the
parent no longer holds stays as a leaving node, painted at the rect it was laid out at, until
those tweens finish (ADR-0150). Each target starts from the value the node displays, or from the
property's identity when it never set one (`1` for `opacity` and `scale`, `0` for the rest). A
leaving node is painted and nothing else: it takes no room in its parent's flow, no clicks, no
scroll and no `geometry` write, and its `id` is not matched again, so re-adding it builds a new
node beside the one still fading. Because no solver runs over it, only what paint reads moves it:
`opacity`, the colours, `scale`, `rotate`, `translate` and a pixel `width`/`height`. A `margin` or
an alignment in the block eases its number and changes nothing on screen; `translate` is what
slides a card out. A child with no `exit` block, or one that was already hidden, is
gone the pass it is dropped, and so is everything under a node that leaves without an exit block
of its own: only the child a pass stopped returning is asked to depart, never its descendants, so
declare the exit on whatever the tree actually drops. Departing also ends every tween the node was
already running, so the block's `duration` is the whole of the node's remaining life. Exit does not
run for `visible = false`; `delay(signal, ms)` holds a whole surface open instead.

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
and [authentication ownership](services.md#7-idle-lock-and-polkit).

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

A panel's `width`/`height` are its layer-shell `set_size` request, and omitting one measures it
from the content, so a stack that grows reserves the room it grew into. The exception is an axis
anchored to both its edges, where an omitted extent stays the compositor's, as `"Fill"` always is:
the protocol allows a size there, but smithay spans the axis and drops it while wlroots centres it,
and asking for nothing is what makes the two agree. `max_width`/`max_height` cap a measurement. `"Fill"` on an axis anchored to one
edge or neither is a protocol error, and that surface is refused rather than created.

Windows/popups use `visible` to open/close. A window's `on_close` is a request the config handles;
min/max sizes are compositor hints. Popup parent is a panel or window ID; its anchor rect must be
nonzero, and its `width`/`height` are omitted to measure the content the same way. Anchors/gravity accept edges, corners or Center.
Constraint adjustments accept SlideX/Y, FlipX/Y, ResizeX/Y; default is FlipY and SlideX.
`grab` defaults true and needs an input serial; use false for hover-opened tooltips.

See [surface parsing](../renderer/src/lua/surfaces.rs),
[panel properties](../renderer/src/layout/node/surface.rs),
[window/popup properties](../renderer/src/layout/node/toplevel.rs) and
[instance expansion](../renderer/src/layout/instance.rs).

## 7. Wire format envelope

Lua capability invocations serialize to a JSON-RPC 2.0 envelope over the control socket.
See [wire format and dispatch limits](services.md#13-control-socket-and-wire-format).

## 8. Tooling

| Command | Behavior |
| :--- | :--- |
| `obelisk init -c <dir>` | Config/editor setup; generates capability field and action stubs |
| `obelisk check -c <dir>` | Evaluates config/surface declarations without Wayland, GPU or subprocess execution |
| `obelisk set <name> <value>` | Writes declared named state; parses JSON, otherwise uses a string |
| `obelisk toggle <name>` | Toggles declared boolean state |
| `obelisk toggle <name> <value>` | Sets declared state to the value, or back to its declared initial when it already holds it; one keybind for a modal whose state names the one showing |

`check` does not validate live service behavior or rendered layout.
See [CLI](../supervisor/src/cli.rs) and [check implementation](../renderer/src/check.rs).
