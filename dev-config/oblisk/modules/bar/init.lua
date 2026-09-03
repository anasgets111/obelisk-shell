-- Mirrors Bar.qml.
--
local theme = require("config.theme")
local left = require("modules.bar.left_side")
local center = require("modules.bar.center_side")
local right = require("modules.bar.right_side")

-- Three zones: two that grow and a middle that does not. Each side takes `width = "Fill"` and
-- distributes its own spare space by its own `align_h`, so the two sides always hold equal shares
-- of whatever the centre leaves and the centre's midpoint is the bar's midpoint by construction.
--
-- This was three fixed percentages until now, and the percentages were a workaround for an engine
-- bug rather than a layout choice. A `Fill` child used to resolve against the parent's whole
-- content width regardless of its siblings, so the obvious flexbox shape failed: every `Fill` zone
-- took the full bar and the ones after it were pushed off the end. `scene.rs` now sizes a `Fill`
-- child from the remainder its siblings leave, which is what makes the line below work at all.
--
-- Which module sits in which zone is `~/.config/quickshell`'s arrangement rather than this file's
-- own: power menu, updates, keyboard, battery, launcher and workspaces on the left; the media
-- widget or the focused window's title in the middle; the status indicators, the tray and the
-- clock on the right. It is worth copying because it is a layout someone has actually lived with,
-- and because the two zones that grow on their own -- the tray, which follows however many
-- `StatusNotifierItem`s are registered, and the workspace strip -- end up on opposite sides of the
-- bar rather than fighting each other for one side's budget.
--
-- What the rewrite retires, recorded because the numbers cost real measurement to find. The split
-- moved three times, every time because a module changed shape:
--
--   40/20/40 -> 43/14/43  a 20% centre stopped holding a clock and a date and started holding one
--                         module at a time (`center_side.lua`)
--   43/14/43 -> 44/13/43  the tray began laying out horizontally and needed real width
--   44/13/43 -> 46/13/41  the workspace strip became a button per workspace instead of one cell
--
-- The last of those gave up a centred centre to buy the left zone room: 46/41 put the centre's
-- midpoint at 52.5%, about forty-eight pixels right of true on a 1920px output. Two `Fill` sides
-- give that back and cost nothing, because a side that needs more now borrows from the other
-- instead of from a number written here.
--
-- One failure mode survives and is worth naming. `row` still does not shrink a child to make its
-- siblings fit, so a left zone whose modules exceed its half of the remainder paints past its own
-- edge exactly as a full 46% zone did. What changed is that the budget follows the content instead
-- of a constant: `socket.rs`'s `the_shipped_dev_configs_bar_zones_hold_their_modules_without_
-- overflowing` is still the thing that catches it.
return panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true, left = true, right = true },
    -- Reserves screen area along the anchored edge, derived from the height the compositor
    -- actually configures, so it stays right if this changes.
    exclusive = true,
    -- `"None"`, which is also the engine's default (`layout::node::parse_keyboard_interactivity`)
    -- and is written out anyway because this line has been wrong twice and both reasons are worth
    -- keeping.
    --
    -- It was `"OnDemand"`, to exercise Phase 21 item 2's focus path rather than leave it dark. niri
    -- gives an `on_demand` layer surface keyboard focus when it *maps*, so starting the shell took
    -- focus off whatever was focused, with no click involved. A bar that eats the keyboard on
    -- startup does not exercise the engine harder, it makes the session it is supposed to be
    -- exercised in unusable.
    --
    -- It was then bound to `ui_state.panel_open`, so that the bar held the keyboard for as long as
    -- any panel was up. That was to reach one field: the network panel's password prompt lived on
    -- `panel_host`, which was an `xdg_popup`, and niri hands a grabbing popup the keyboard only if
    -- its parent held it at map time -- so the claim had to be made in advance, by the parent, and
    -- could not be narrowed to the moment a password was actually wanted without breaking the grab.
    -- The bar paid for the popup's problem: opening the calendar took the whole keyboard.
    --
    -- `panel_host` is a layer surface now and asks for its own keyboard, bound to the one signal
    -- that wants it (`modules/shell/panel_host.lua`). So this is back to a constant, and the bar
    -- never takes the keyboard at all.
    keyboard_interactivity = "None",
    width = "Fill",
    height = theme.bar_height,
    -- Translucent, and vertically unpadded. Both are the mirror's, and both are load-bearing.
    --
    -- `GLASS_SURFACE` is the background colour at half alpha, so the bar composites against the
    -- wallpaper panel below it instead of covering it. An opaque ground here is most of why this bar
    -- read as a black strip with boxes on it while the one it mirrors reads as glass.
    --
    -- The vertical padding is gone because an item is 31px and the bar is 38px: `align_v = "Center"`
    -- already leaves 3.5px above and below, and the 4px of padding that used to be here made the
    -- content box 30px, one pixel short of the thing it had to hold. Horizontal padding stays and
    -- grows to `spacing.md`, which is `Theme.qml`'s `panelMargin`.
    child = row {
        width = "Fill",
        height = "Fill",
        background = theme.GLASS_SURFACE,
        padding = { left = theme.spacing.md, right = theme.spacing.md },
        children = { left, center, right },
    },
}
