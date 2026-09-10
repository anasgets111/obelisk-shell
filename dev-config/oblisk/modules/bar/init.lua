-- Mirrors Bar.qml.
--
local theme = require("config.theme")
local left = require("modules.bar.left_side")
local center = require("modules.bar.center_side")
local right = require("modules.bar.right_side")

-- Three zones: two `Fill` sides share spare space equally, while each side uses its own `align_h`.
-- That keeps the centre's midpoint at the bar's midpoint.
--
-- Fixed percentages worked around an engine bug: `Fill` used to resolve against the whole parent,
-- pushing later zones off the end. `scene.rs` now sizes it from the siblings' remainder.
--
-- The reference arrangement puts power, updates, keyboard, battery, launcher and workspaces left;
-- media or the focused title centre; status, tray and clock right. Registered
-- `StatusNotifierItem`s and the independently growing workspace strip occupy opposite sides.
--
-- Retired splits, measured as modules changed shape:
--
--   40/20/40 -> 43/14/43  a 20% centre stopped holding a clock and a date, then held one module
--                         at a time (`center_side.lua`)
--   43/14/43 -> 44/13/43  the tray began laying out horizontally and needed real width
--   44/13/43 -> 46/13/41  the workspace strip became a button per workspace instead of one cell
--
-- 46/41 put the centre at 52.5%, about 48px right of true on a 1920px output. Two `Fill` sides
-- restore the midpoint while borrowing space from each other.
--
-- `row` still does not shrink children to fit siblings, so an overfull left zone paints past its
-- edge. The budget follows content; `socket.rs`'s
-- `the_shipped_dev_configs_bar_zones_hold_their_modules_without_overflowing` catches it.
return panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true, left = true, right = true },
    -- Reserves screen area from the compositor-configured height.
    exclusive = true,
    -- `"None"` is the engine default (`layout::node::parse_keyboard_interactivity`), written out
    -- because this setting has been wrong twice.
    --
    -- `"OnDemand"` exercised Phase 21 item 2, but niri focuses an `on_demand` layer surface when it
    -- *maps*. Starting the shell stole focus with no click and made the session unusable.
    --
    -- It was then bound to `ui_state.panel_open` so the `xdg_popup` network password prompt could
    -- inherit the bar's keyboard grab. niri grants that grab only if the parent held it at map
    -- time, so every panel, including the calendar, took the keyboard.
    --
    -- `panel_host` is now a layer surface that asks only when needed
    -- (`modules/shell/panel_host.lua`), so the bar never takes the keyboard.
    keyboard_interactivity = "None",
    width = "Fill",
    height = theme.bar_height,
    --
    -- `GLASS_SURFACE` is half-alpha, so the bar composites over the wallpaper. An opaque ground
    -- made it read as a black strip with boxes instead of glass.
    --
    -- No vertical padding: a 31px item in a 38px bar leaves 3.5px each side; old 4px padding made
    -- the content box 30px, one pixel too short. Horizontal padding is `spacing.md`,
    -- `Theme.qml`'s `panelMargin`.
    child = row {
        width = "Fill",
        height = "Fill",
        background = theme.GLASS_SURFACE,
        -- The bar is the one sheet that is always up, so it is the one blur anybody notices.
        blur = true,
        padding = { left = theme.spacing.md, right = theme.spacing.md },
        children = { left, center, right },
    },
}
