# Four surface roles: `panel`, `window`, `popup`, `lock`

ADR-0038 recorded xdg-shell toplevels and xdg-popup as deliberate non-goals, on the reasoning that
layer-shell alone covers bars, docks, launchers, OSDs, and wallpapers, and that ashell ships without
popups. That scope was set aside on 2026-08-28: the target is Quickshell's freedom, meaning floating
windows, panels, popups, session lock, and per-screen variants, driven from Lua against a native
Rust renderer. This ADR replaces those two non-goals with a design.

## Decision 1: four constructors, not one `surface` with a `kind` field

In Wayland, a `wl_surface` is inert until a protocol assigns it a **role**. `xdg_toplevel`,
`xdg_popup`, `zwlr_layer_surface_v1`, and `ext_session_lock_surface_v1` are four roles. Oblisk
mirrors that directly:

| Lua constructor | Role | Protocol |
| :--- | :--- | :--- |
| `panel` | Layer surface | `zwlr_layer_surface_v1` |
| `window` | Toplevel | `xdg_toplevel` |
| `popup` | Popup | `xdg_popup` |
| `lock` | Lock surface | `ext_session_lock_surface_v1` |

One constructor with a `kind` discriminant was considered and rejected. The property sets are
mostly disjoint, so a single schema would accept `layer` on a toplevel, `title` on a layer surface,
and `anchor_rect` on both, with validation reduced to a per-kind allowlist and error messages that
name a property the node should never have had. Four constructors give each role an honest schema
that `layout::node` can validate directly. Quickshell reached the same shape with four types
(`PanelWindow`, `FloatingWindow`, `PopupWindow`, `WlSessionLockSurface`).

`renderer/src/lua/nodes.rs` generates constructors from a `NODE_KINDS` array, so this is an array
edit plus per-role parsing, not new machinery.

**The `surface` constructor is renamed to `panel`.** "Surface" stops being a Lua-callable name and
becomes the umbrella concept: a declared `wl_surface` plus its role, which is what `CONTEXT.md`'s
**Surface** entry already describes. The cost is one line in `dev-config/oblisk/shell.lua` and two
in `oblisk-reference-fixtures.md`. Keeping `surface` as an alias for `panel` was rejected: there are
no users to migrate, and an alias would permanently blur the umbrella term against one of its four
members.

## Decision 2: popups parent to a panel or a window, and take a real grab

The protocol supports popups on layer surfaces directly. `zwlr_layer_surface_v1` carries its own
`get_popup`:

> This assigns an xdg_popup's parent to this layer_surface. This popup should have been created via
> xdg_surface::get_popup with the parent set to NULL, and this request must be invoked before
> committing the popup's initial state.

and `xdg_surface.get_popup` anticipates the handoff: "If null is passed as a parent, a parent
surface must be specified using some other protocol, before committing the initial state."

So a popup is created the same way regardless of parent (`create_positioner`, then `get_popup` with
a null parent), and is then rooted under either an `xdg_surface` or a layer surface before its first
commit. A bar's dropdown is a first-class `xdg_popup` with the full `configure` / `popup_done` /
`grab` machinery, not a second layer surface with hand-computed coordinates.

This corrects ADR-0038, which recorded click-outside-dismissal as having no compositor-agnostic
answer, on the basis that Quickshell's `HyprlandFocusGrab` is Hyprland-only. That is true for layer
surfaces and false for popups. `xdg_popup.grab` covers both halves:

> During a popup grab, the client owning the grab will receive pointer and touch events for all
> their surfaces as normal [...] while the top most grabbing popup will always have keyboard focus.

and dismissal: "An explicit grab will be dismissed when the user dismisses the popup [...] by the
user clicking outside the surface, using the keyboard, or even locking the screen." The dismissal
arrives as `popup_done`, after which the client destroys the popup.

Three constraints the implementation must respect, all from the spec:

- **A denied grab is a normal outcome, not an error path.** "If the compositor denies the grab, the
  popup will be immediately dismissed." Treat `popup_done` arriving immediately after `grab` as
  expected, and make sure a Lua-side popup can close cleanly from it.
- **Grab must answer a real input event** (button press, key press, touch down) and must be
  requested before the popup is mapped; after mapping it raises `invalid_grab`. This makes Phase 21
  (input routing) a hard prerequisite for popups, not merely a nice ordering.
- **Nested popups are destroyed in reverse creation order.** The engine owns that ordering, not the
  config.

## Decision 3: a popup's anchor rect comes from the click that opened it

`xdg_positioner` requires a non-zero size and a non-zero anchor rectangle, or `get_popup` raises
`invalid_positioner`. The anchor rect is parent-surface-relative, which is exactly the coordinate
space a resolved node already lives in.

Rather than give nodes ids so Lua can anchor to one by name, `button`'s `on_click` gains an argument
carrying the clicked node's resolved rect, and the config passes it straight to the popup's
`anchor_rect`. This needs no new identity concept, matches how ashell derives its menu positions
from the triggering button's on-screen rect, and keeps the retained scene the single source of
geometry. `set_offset` handles the nudge cases on top.

`set_constraint_adjustment` defaults to `none`, meaning no repositioning at all when a popup would
fall off-screen. Oblisk defaults it to `flip_y | slide_x` instead, which is the behavior a config
author expects from a dropdown, with the raw bitfield available for configs that want to be
specific. The spec's precedence is fixed at flip, then slide, then resize.

## Decision 4: floating windows reuse the staging discipline already built

xdg-shell's initial-commit rule is the same one the layer-shell path already implements:

> the client must perform an initial commit without any buffer attached. The compositor will reply
> with initial wl_surface state [...] followed by an xdg_surface.configure event. The client must
> acknowledge it and is then allowed to attach a buffer to map the surface.

`zwlr_layer_surface_v1` states this almost verbatim. PBA's null-buffer staging (services § 15.2)
therefore generalizes across all four roles with no protocol-specific special case, which is the
main reason adding toplevels is cheaper than it looks.

The differences that do matter: `xdg_toplevel`'s configure carries a **state array**
(`maximized`, `fullscreen`, `resizing`, `activated`, `tiled_*`) that layer-shell has no analogue
for, and the ack goes through the wrapping `xdg_surface`, not the role object. `set_min_size` and
`set_max_size` are advisory ("The client should not rely on the compositor to obey the maximum
size"), while a fullscreen configure is binding.

`window` gets `title`, `app_id`, `min_size`, `max_size`, and `on_close`. `xdg_toplevel.close` is
explicitly a request, not a command ("The client may choose to ignore this request"), so `on_close`
is a Lua callback that may decline, not a teardown notification.

**Decorations are not built.** Without `zxdg_decoration_manager_v1` a client "continues to
self-decorate as they see fit", and even with it `set_mode` is a preference the compositor may
override. Oblisk requests server-side and accepts whatever it gets; it does not ship a client-side
titlebar frame. A shell's own floating windows are its dialogs, launchers, and settings panels,
which a config styles itself. Revisit if someone builds a window that genuinely wants a system
titlebar.

## Decision 5: `smithay-client-toolkit` covers this, with one escape hatch

SCTK 0.21.1 wraps everything needed: `XdgShell` (bound `xdg_wm_base`), `Window` and `WindowHandler`
(with `WindowConfigure` carrying `new_size`, `decoration_mode`, and the state bitflags), `Popup` and
`PopupHandler` (with `PopupConfigure` and a `done` callback on `popup_done`), `XdgPositioner`, and
`LayerSurface::get_popup`. `Popup::from_surface` takes an `Option` parent specifically for the
null-parent path this design uses.

The one gap is `xdg_popup.grab`, which SCTK does not wrap at all. `Popup::xdg_popup()` returns the
raw protocol object, so the call is `popup.xdg_popup().grab(seat, serial)` with the engine owning
the grab bookkeeping. This is the same shape as ADR-0009's `wp-text-input-v3` handling, which is
already the established precedent for a protocol SCTK leaves unwrapped: use SCTK where it wraps,
reach through where it does not, and do not hand-dispatch what it already covers (ADR-0008).

## Scope

`lock` is named here as the fourth role and its ownership is settled separately, because the open
question is not what properties it takes but which process holds `ext_session_lock_v1`, which
revisits ADR-0010.

Sequencing against `build-steps.md`: all four roles need the paint pass (Phase 19) and the
Lua-declared surface manager (Phase 20) first, since none of them can currently receive content.
`popup` additionally needs input routing (Phase 21) for its grab and its anchor rect. Nothing here
changes the reload model: adding or removing a declared surface of any role is a topology change,
and per-role instancing follows ADR-0038 decision 3 unchanged.
