# What is not built

Every spec in this repo was written from `oblisk-idl-api-specs.md`. A spec cannot list what it
forgot, so this file works the other way round: it diffs the workspace against a Quickshell config
that is already somebody's daily shell (`anasgets111/dotfiles`, `quickshell/.config/quickshell`).

Nothing here is scheduled. Several entries need a decision before a line of code.

## The reference workload

Nineteen bar modules across three zones, eight Wayland surfaces, about forty data sources. It drives
PipeWire, UPower, BlueZ, MPRIS, NetworkManager, SystemTray, Polkit and PAM through native bindings,
speaks niri's event stream on one socket and its request channel on another, shells out to more than
twenty binaries, and renders a 256-bar audio spectrum through a GLSL fragment shader.

## By layer

| Layer | State | Detail |
| :--- | :--- | :--- |
| Capabilities, read | near complete | every native Quickshell service has a counterpart |
| Capabilities, write | near complete | ahead in one place: that config reads the power profile, `power:set_profile` sets it |
| Surface roles | complete | `panel` on four layers, `window`, `popup`, `lock`, all live-tested |
| Pointer input | three of four events | click, hover and scroll are built; nothing reports the pointer's own shape |
| Paint vocabulary | five operations | fill, radius, per-edge border, blit, rounded clip |
| Animation | absent | Lua's fastest clock is `system.time` at 1 Hz |
| Text and layout | sufficient | shaping, clipping, alignment, keyed reconciliation |
| Text metrics | partly built | `text_align` and `elide` land; wrap and `max_lines` need line breaks the shaper does not return |
| Fonts | declared | `fonts { ... }` picks the chain; no per-node family, and none is needed while fallback is per glyph |

The data layer is not the problem. Paint and animation are.

## Ranked, by modules unblocked per unit of work

1. **The animation model.** The largest item. `CONTEXT.md`'s Lease exists to hold a removed node's
   GPU resource alive for a crossfade and has had no caller since it was written, because the
   feature it serves was never specified. One constraint binds now: frame gating is written as
   "repaint when the scene changed", and a running animation is a second, orthogonal reason to
   wake. Build the gate so a reason can be added rather than replacing the condition.
   Quickshell's `Retainable` (refcounted `lock()`/`unlock()` plus a `dropped()` signal, so a config
   can say "not yet" while an exit transition runs) is the shape to copy on the day exit
   transitions exist.
2. **Backdrop blur.** One decision, two bad options, and only the client-side one works on niri.
3. **A `shape` node taking a path.** Closes the notch, the arcs and circular progress together, and
   costs far less than exposing shaders.
4. **Shaders.** The audio spectrum and the wallpaper transitions. Lowest value, and the only item
   that puts the GPU in reach of config code.

## Data no capability carries

| Missing | Used for | Nearest path today |
| :--- | :--- | :--- |
| An HTTP client | weather, IP geolocation, currency | `process.run curl` then `json.decode`. Fine as a subprocess; a capability would be scope creep. |
| Per-workspace window lists | drawing each workspace's app icon | § 2.9 carries one global `active_client` plus per-workspace `{ id, idx, name }`. ADR-0056 chose that payload, so this is a decision, not an oversight. niri's `Window.workspace_id` makes it an additive field. |
| Special workspaces | the special-workspaces pill | nothing in § 2.9 models them. niri-specific, so ADR-0056's one-compositor rule makes it cheap to build and awkward to name. |
| Source (microphone) mute | the privacy indicator's click target | § 3.2 has `set_default_source` and no `set_source_muted`. A spec hole, not a design choice: the mixer already writes node properties. |
| KDE Connect | SMS, ring, mount, remote commands | none. Lua cannot speak D-Bus, and this needs a live signal stream rather than one-shot calls. The only entry arguing for a general D-Bus escape hatch. |
| Monitor configuration | the display-settings arrangement editor | § 2.15 `screens` reads and nothing writes. Writing output config is compositor-specific, so ADR-0056's reasoning applies unchanged. |

## Judged and dropped

- **Lazy surface creation.** ADR-0049 settled it. Creating `popup` and `window` objects on show is
  forced by the protocol and delivers what Quickshell's `LazyLoader` delivers without one.
  Asynchronous incubation has no analogue: this is one Wayland object per open, not a QML tree.
- **Config-triggered reload.** `Quickshell.reload(hard)` is callable from QML. Here reload is
  supervisor-only through inotify and has no caller. ADR-0047's recursive watch covers edits, and
  ADR-0048 removed file reading from Lua, which was the last external change a config could have
  noticed. Build it if a caller appears.

## Known holes in the tooling

- **`--validate` does not exist.** `renderer/src/main.rs` never reads `env::args`, so
  `cargo run -p renderer -- --validate <path>` ignores the flag in full and starts a live renderer
  over the running session. It exits `70` in a fraction of a second when no supervisor is up
  (ADR-0059 decision 1), so it no longer holds the render node, but it still validates nothing.
  A config that fails to load should say so at a shell prompt.
