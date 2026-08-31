# Paint properties are parsed once at apply time, and a bad one fails the pass

`layout::paint::build` ran all fourteen paint-property parsers on every node of every mapped surface
on every dirty turn, and treated a malformed value as absent. Both halves are now gone:
`node::paint_style` parses them once while `Scene::apply` resolves the node, and a failure fails the
pass.

## Why the cadence was wrong

ADR-0063 decision 1 made the display list the thing that decides whether a surface repaints, which
means `build` has to run before a surface can decline a frame. ADR-0044 decision 2 then raised how
often that happens: one dirty flag for the whole scene, so any capability push re-resolves every
surface. The parse became the price of finding out that nothing had changed, at a cadence nobody
chose, for properties whose values had already been resolved once that same pass.

## Why the failure rule was the bigger half

One resolved property map had three opinions about what a broken value means:

| Reader | On a malformed value |
|---|---|
| `scene.rs` geometry | `?`, the apply fails, the scene rolls back, `oblisk.rescue` |
| `layout::paint` | log, substitute the absent-key default, keep painting |
| `wayland::surface::apply_resolved_state` | log, keep the last applied value |

Paint's rule was defended on the grounds that a config error in one node's paint properties should
not blank the surface around it. That defence was written when nothing else validated paint
properties. It stopped being true once rescue and rollback existed: a bad `align_v` one line away
already takes the tree down, and a `background` that is an integer is the same class of bug. The
lenient rule also meant `background = 5` printed a line per frame, forever, each one re-formatting
the rejected value's whole `Debug` form (build-steps.md Phase 19 item 13 measures the hostile case
at 20 MB). Rate-limiting that log would have hidden the real problem.

The third rule, `apply_resolved_state`'s, stays. It runs at configure cadence over surface-role
properties, not on the frame path, and it has a last-applied value to keep, which a paint pass does
not.

## What is left at paint time

Anything needing an input the resolve pass does not have. An `icon` or an `image` needs the physical
scale to turn its logical edge into a pixel count, and a `textfield` needs to know whether it holds
the keyboard focus. Both are arithmetic over already-parsed data. `PaintStyle::TextField` therefore
carries the placeholder, the mask character and the destination, and `build_node` still chooses
between placeholder and mask from the focus it is handed.

`PaintStyle` holds no Lua value, for ADR-0063 decision 2's reason: mlua compares tables by identity,
so a property whose signal resolves to a table would compare unequal every pass.

## Consequences

`wayland::input::focused_target` is infallible now, and the press path's "malformed `secure_submit`,
so this field takes focus with no destination" branch is deleted. A tree carrying a `secure_submit`
that does not parse cannot reach a pointer event, because the pass that would have built it failed.
`layout::secure_submit::secure_submit_targets` lost its matching skip-the-malformed-one rule for the
same reason. The guarantee did not disappear; it moved to where the parse now happens.

Coverage widened, and this is the part a reader will trip over. `build_node` returns early on an
invisible node and on a subtree clipped to nothing, so a malformed `background` under `visible =
false` was never parsed and never noticed. Every resolved node is parsed now, visible or not, so the
same config drops to rescue at boot instead of at the moment something makes the node visible. A
hidden `textfield` with a malformed `secure_submit` is the sharpest case: it used to be skipped
twice over, once by the visibility gate and once by `secure_submit_targets`'s own
skip-the-malformed-one rule. That is the intended direction: a config error that hides until a panel
expands is worse than one that says so immediately, and the geometry parsers already had this
coverage. `a_malformed_paint_property_on_an_invisible_node_still_fails_the_pass` pins it.

`layout::node`'s interface shrank by twelve functions. `parse_background`, `parse_radius`,
`parse_border_color`, `parse_border_width`, `parse_font_size`, `parse_foreground`,
`parse_icon_name`, `parse_image_source`, `parse_fit`, `parse_placeholder`, `parse_mask_character`
and `parse_secure_submit` were all `pub` for `layout::paint`'s sake. They are internal to `node`
now, and the way to ask what a node paints is to ask for its `PaintStyle`.

`layout::paint` names no property and imports no `mlua`. `ResolvedNode.properties` stays, because
`hover`, `on_close` and `on_dismiss` want the raw `Value` and the role specs re-parse it at their own
cadence, so this is an added field rather than a replaced one.

The geometry parsers were not moved with it, against build-steps.md Phase 19 item 13's note that
both halves wanted doing together. There is no second half: they already run at apply time under the
`?` rule this gives the paint properties.
