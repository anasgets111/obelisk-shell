# Surfaces come from `shell.lua`, not a fixed set of Rust-owned roles

> **`exclusive` is no longer a boolean.** docs/adr/0078 gives it a third value, `"Ignore"`, for
> layer-shell's `-1`: reserve nothing *and* ignore what other surfaces reserved. Decision 2's
> in-place list is unchanged and so is everything else here -- `set_exclusive_zone` was already on
> it, and the third value travels the same live-surface path the other two do.

> Decision 2 is amended by ADR-0049. "Created once, at startup" holds for `panel`, the only role
> that existed when this was written, and for `lock`. It cannot hold for the `popup` and `window`
> roles ADR-0040 added: `xdg_popup` requires its grab before mapping with the serial of a real
> input event, and its positioner is consumed at `get_popup` time, so a popup created at startup can
> neither re-grab nor follow a different anchor. For those two roles `visible` creates and destroys
> the Wayland object rather than mapping and unmapping it. The declared set is still fixed for a
> generation's life, so ADR-0001's topology split is unchanged.

The 2026-08-28 renderer review found the two halves of the Renderer disagreeing about what a surface
is, and the docs disagreeing three ways alongside them.

`renderer/src/wayland/mod.rs` owns a closed `SurfaceRole { MainBar, OverlayCanvas, WallpaperLayer }`
enum. `run()` calls `create_main_bar` / `create_overlay_canvas` / `create_wallpaper_layers`
unconditionally, immediately after two registry roundtrips, before any Lua has been evaluated.
`main_bar` is a hardcoded `Layer::Top`, 32px tall, 32px exclusive zone, namespace
`"oblisk-main-bar"`. Meanwhile `renderer/src/layout/scene.rs` keys top-level surfaces by their own
`id` property in a `HashMap<String, RetainedNode>` precisely because "surfaces are identified, not
ordered" (ADR-0023), `layout::node::SurfaceTopology` already parses `id`/`layer`/`anchor`/`monitor`
off each declared surface, and `supervisor/src/reload.rs` already carries per-output surface ids like
`"wallpaper_layer@DP-1"` through the PBA handshake.

The observable result: `dev-config/oblisk/shell.lua` is `return surface { id = "bar", layer =
"Overlay" }`, and every field in it is discarded. The compositor gets a `Top`-layer bar named
`oblisk-main-bar` regardless. The declarative config does not decide what the compositor sees.

`docs/oblisk-supervisor-services-dbus.md` § 15.2 already specifies the correct order for the
Candidate: evaluate `shell.lua` first (point 1), then bind layer-shell surfaces (point 2). The
implementation does the reverse and never joins the two.

## The doc contradiction

Four documents describe three different surface models:

| Source | Model |
| :--- | :--- |
| `oblisk-idl-api-specs.md` § 4 | "exactly **two static layer surfaces**"; all popups/OSDs/modals are nodes inside `overlay_canvas` |
| `oblisk-layout-engine-geometry.md` § 2.1, § 5 | "exactly **two window surfaces**", "our dual-surface model" |
| `oblisk-reference-fixtures.md` § 1 | "our simplified, zero-dependency, **dual-surface** architecture" |
| ADR-0007, `build-steps.md` Phase 3.4 | **three** static surfaces, wallpaper added as a third role |
| `oblisk-idl-api-specs.md` § 6.1, `CONTEXT.md` (Topology change), ADR-0023 | **N** surfaces, Lua-declared, keyed by `id`, arbitrary layer/anchor/monitor |

ADR-0007 is the tell. The fixed set broke the first time a real use case arrived: wallpaper could not
live inside `overlay_canvas` because `Overlay` is the topmost layer by protocol definition, so a
fourth role was added by hand. Every further use case does the same thing again.

## What the reference implementations do

Checked against three shipping Wayland shells on 2026-08-28.

**Quickshell** (Qt Quick, QML) creates windows dynamically at runtime. `ProxyWindowBase::ensureQWindow()`
allocates lazily, never at process start. The documented per-monitor idiom is
`Variants { model: Quickshell.screens; PanelWindow { screen: modelData } }`, reactive against a
live screen list, so bars appear and disappear with monitor hotplug. Its window types are
`PanelWindow` (wlr-layer-shell), `FloatingWindow` (xdg-shell toplevel), `PopupWindow` (xdg-popup),
and `WlSessionLock`/`WlSessionLockSurface` (ext-session-lock-v1).

**ashell** (Rust, iced) is the decisive case, because its product scope is exactly a status bar,
the narrowest thing Oblisk claims to be more general than. It still could not keep a fixed surface
set. `iced_layershell`'s `surface_manager.rs` allocates `SurfaceId::unique()` from an atomic
counter, queues `LayerShellCommand::NewSurface(id, LayerShellSettings)` and
`LayerShellCommand::DestroySurface(id)`, and holds `HashMap<wl_surface, SurfaceData>`. ashell's
dropdown menus (`src/outputs.rs`, `Outputs::toggle_menu`) are each a separate layer-shell surface
with its own anchor computed from the triggering button's on-screen rect, opened and closed at
runtime, flipping `keyboard_interactivity` to `OnDemand` when the menu needs typing. A bar with
dropdowns already exceeds a fixed surface set.

**Noctalia v5** (C++, no Qt, GLES2 over EGL) is the counterexample, and it is instructive rather
than contradictory. It keeps a fixed set of surface types (bar, dock, lockscreen, notification
toasts, desktop-widgets host) because it is a product, not a toolkit: its config is TOML picking
widgets into named slots (`[bar.main] start/center/end = [...]`), with a Luau plugin system layered
on for anything beyond that. Oblisk has explicitly chosen the other side of that fork. Its config is
a Lua program returning a node tree, which is the Quickshell position, not the Noctalia one.

## Decision

1. **The evaluated topology is the only source of Wayland surfaces.** Each `surface` node returned
   by `shell.lua` maps to one `zwlr_layer_surface_v1` per output it targets. `SurfaceRole` and the
   three `create_*` calls in `wayland::run` are deleted. `main_bar`, `overlay_canvas`, and
   `wallpaper_layer` survive as ordinary ids in the default config, not as Rust constants.

2. **A generation creates exactly the surfaces its own evaluation declared, once, at startup.**
   This does not introduce in-place surface creation or destruction, and ADR-0001's split is
   unchanged. Adding or removing a `surface`, or changing its `layer`/`anchor`/`monitor`/
   `namespace`, is a topology change: the Supervisor spawns a candidate, and the candidate builds
   its own surface set from its own evaluation. What changes here is only where a generation's
   surface set comes from at that startup, a Lua evaluation rather than three `create_*` calls.

   Within a live generation, two things move without a swap. `visible` maps and unmaps a surface,
   so toggling a launcher costs a commit rather than a process spawn. The fields layer-shell lets a
   client change on a live surface (`margin`, exclusive zone, `keyboard_interactivity`, size) are
   applied in place as value changes.

   Reconciling a live native object graph against a freshly evaluated one is exactly what the
   process boundary exists to avoid, and that reasoning is unchanged here.

3. **A surface targeting multiple outputs produces one surface instance per output.** The
   `"{id}@{output}"` surface-id convention `supervisor/src/reload.rs` already carries through PBA
   generalizes from wallpaper to every surface, replacing the current per-role special case.

   Monitor hotplug adds and removes instances in place, with no generation swap. This is consistent
   with decision 2 rather than an exception to it: plugging in a monitor is not a config edit, and
   the set of declared surfaces does not change, only the set of instances a `monitor = "All"`
   declaration expands to. A swap here would respawn the shell every time a laptop docks, which is
   the wrong trade for an event the config did not cause. Quickshell reaches the same place from the
   other direction, with `Variants` bound to a reactive `Quickshell.screens` list.

4. **`surface` gains `namespace`, `keyboard_interactivity`, and `margin`** (see the § 6.1 edit
   accompanying this ADR). All three are load-bearing for the stated goal, and all three are
   present in both reference toolkits:
   - `namespace` is the layer-shell namespace string. Compositor rules key off it (Hyprland's
     `layerrule` for blur and animations matches on namespace). Today it is hardcoded per role, so
     a user cannot write a compositor rule against their own panel. `iced_layershell`'s
     `LayerShellSettings` carries it.
   - `keyboard_interactivity` (`"None"`/`"OnDemand"`/`"Exclusive"`) maps to layer-shell's own field.
     Without it a launcher cannot take typing, which alone blocks the most common non-bar shell
     component. Quickshell exposes it as `PanelWindow.focusable`; `iced_layershell` carries the full
     three-way enum, and ashell flips it per dropdown.
   - `margin` is the anchor offset. Both toolkits carry it; a floating panel inset from a screen
     edge needs it and cannot get it from padding, which is inside the surface.

5. **Input regions stay per surface.** `oblisk-layout-engine-geometry.md` § 5's bounding-box union
   is not deleted, it is generalized: it applies to any surface whose visible content is smaller
   than the surface itself, which is the common case for a fullscreen transparent panel and a no-op
   for a tightly-sized bar. `layout::overlay_input_regions` already computes this correctly and
   still has no production caller (ADR-0023 item 5).

## Rejected: keep the fixed roles, host every popup inside `overlay_canvas`

This is the shipped model (IDL § 4, layout § 2.1). Rejected on five counts, in descending order of
how badly each blocks the goal:

- **One namespace for everything.** A single `oblisk-overlay-canvas` surface means every panel
  drawn in it shares one compositor identity. Blur the launcher but not the volume OSD is not
  expressible, because the compositor sees one surface.
- **One `keyboard_interactivity` for everything.** The field is per surface in the protocol. A
  launcher wanting `Exclusive` and an OSD wanting `None` cannot coexist in one surface; the
  launcher's focus mode leaks onto everything else currently visible.
- **One layer for everything.** `Overlay` sits above every application window by protocol
  definition. A dock that should sit below a fullscreen window and a notification that should sit
  above it are both drawn in the same surface. This is the exact reasoning that already forced
  ADR-0007, applied to every remaining case.
- **No per-output content.** One `overlay_canvas` cannot show different content per monitor without
  a per-output surface, which is the thing the fixed set does not have.
- **No exclusive zone per panel.** A dock reserving screen space needs its own surface with its own
  exclusive zone. One shared non-exclusive overlay cannot reserve space for part of itself.

The counter-argument on record in IDL § 4 is "zero-overhead footprint... without resource
thrashing" and layout § 5's "absolutely no dynamic window creation delays". Both reference
implementations create surfaces at runtime and neither reports this as a cost; ashell specifically
allocates a layer surface per dropdown open. A `wl_surface` plus a `zwlr_layer_surface_v1` is two
protocol objects and a roundtrip, not a resource worth an architecture.

## Deliberately not built

> Superseded by ADR-0040 and ADR-0042 on 2026-08-28. Quickshell-equivalent freedom (floating
> windows, popups, session lock, per-screen variants) was named as the goal, which turns all four
> deferrals below into work. The original reasoning is kept because two of the four were wrong on
> the facts, not merely overruled on scope, and that is worth not repeating.

- **xdg-shell toplevels** (Quickshell's `FloatingWindow`). A standalone application window is a
  genuinely different claim from a shell component, and layer-shell already covers bars, docks,
  launchers, OSDs, notification panels, and wallpapers. Revisit when a real config wants a window
  the compositor tiles. *Now the `window` role in ADR-0040. The scope call was reasonable; the cost
  estimate was too high, since xdg-shell's initial-commit discipline is identical to layer-shell's,
  so PBA's null-buffer staging generalizes with no special case.*
- **xdg-popup** (Quickshell's `PopupWindow`). The compositor-side wins are real: automatic
  repositioning when a popup would leave the screen, and protocol-native dismiss on click-outside.
  ashell ships without it, declares it unsupported, and hand-rolls menus as layer surfaces with
  manually computed anchors, which is evidence that the layer-shell path is adequate rather than
  merely tolerable. Build it when manual anchor math actually hurts. *Now the `popup` role in
  ADR-0040.*
- **Click-outside-to-dismiss.** Quickshell's answer is `HyprlandFocusGrab`, which is Hyprland-only,
  through a compositor-specific protocol extension. No compositor-agnostic equivalent was found.
  This is a real hole in the ecosystem, not an Oblisk oversight; a config dismisses its own popups
  from Lua today. **Wrong.** `xdg_popup.grab` is the compositor-agnostic mechanism, giving the
  grabbing popup keyboard focus and compositor-driven dismissal via `popup_done`. The mistake was
  generalizing from layer surfaces, which genuinely have no such mechanism, to popups, which do.
  See ADR-0040 decision 2.
- **Session-lock surfaces.** ADR-0010 already puts lock authority in the Supervisor, on its own
  Wayland connection, precisely so it outlives a Renderer crash. That does not move. **Wrong, and
  it moves.** `ext-session-lock-v1` requires the compositor to keep the session locked when the lock
  client dies, so the crash argument does not hold, and a locked session hides every non-lock
  surface, so ADR-0010's plan to keep painting the Lua lock UI from the Renderer could never have
  worked. See ADR-0042.

## Consequences

ADR-0007 is not reversed. Wallpaper still gets its own `Background`-layer surface for exactly the
reason recorded there, and the paint-order argument is unchanged. What changes is who declares it:
`shell.lua`, not `create_wallpaper_layers`. The **Wallpaper surface** term in `CONTEXT.md` stops
being "the third static surface".

`oblisk-idl-api-specs.md` § 4, `oblisk-layout-engine-geometry.md` § 2.1 and § 5, and
`oblisk-reference-fixtures.md` § 1's preamble state the two-surface model as fact and are corrected
alongside this ADR. `build-steps.md` Phase 3.4's three-surface registration text is corrected to
name the default config's surfaces rather than a Rust-owned set.

This ADR settles the model, not the delivery. It cannot be implemented until the Renderer's scene
and its Wayland objects live on the same thread, which is ADR-0039.
