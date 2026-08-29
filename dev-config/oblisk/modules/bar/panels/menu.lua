local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local ui_state = require("lib.ui_state")

local clicks = ui_state.clicks
local popup_anchor = ui_state.popup_anchor
local settings_open = ui_state.settings_open
local menu_open = ui_state.menu_open

local menu_button = button {
    width = 90,
    height = 24,
    background = theme.SURFACE,
    radius = 6,
    on_click = function(rect)
        clicks:set(clicks:get() + 1)
        popup_anchor:set(rect)
        settings_open:set(not settings_open:get())
        menu_open:set(true)
    end,
    children = { cell(clicks:map(function(n)
        return string.format("menu %d", n)
    end), theme.ACCENT) },
}

local surface = popup {
    id = "click_menu",
    -- The `id` of the surface this anchors to, not a node: the protocol roots a popup under a
    -- parent surface at creation (§ 6.3).
    parent = "bar",
    -- Bound as a signal, which is the spelling § 6.3 and ADR-0050 decision 3 prescribe, so the
    -- popup opens over whichever button was actually clicked.
    anchor_rect = popup_anchor,
    -- Required and non-zero on both axes: a popup has no "Fill" (§ 6.3), because there is
    -- nothing for it to fill.
    width = 220,
    height = 130,
    anchor = "BottomLeft",
    gravity = "BottomRight",
    constraint_adjustment = { "FlipY", "SlideX" },
    offset = { x = 0, y = 4 },
    -- `visible` going true is what creates the `xdg_popup`, and it may only do so from inside a
    -- click, because that is the only turn a grab serial is armed for (ADR-0049's amendment).
    visible = menu_open,
    -- Fired when the compositor dismisses this, which for a grabbing popup is a click anywhere
    -- outside it (§ 6.3). Writing the flag back is the config's half of ADR-0051 decision 2: the
    -- engine has already destroyed the object and latched the declaration shut, and this false
    -- is what unlatches it so the next click can reopen it.
    on_dismiss = function()
        menu_open:set(false)
    end,
    child = column {
        padding = { top = 10, right = 12, bottom = 10, left = 12 },
        spacing = 6,
        background = "#181825ee",
        radius = 10,
        border_width = 1,
        border_color = theme.SURFACE,
        children = {
            cell(util.label(oblisk.battery, function(b)
                if not b.present then
                    return "on ac power"
                end
                return string.format("battery %d%% %s", b.percent, b.charging and "charging" or "discharging")
            end), theme.FG, 12),
            cell(util.label(oblisk.network, function(n)
                return string.format("%d network(s) in range", util.count(n.available_networks))
            end), theme.DIM, 12),
            cell(util.label(oblisk.bluetooth, function(b)
                return string.format("%d bluetooth device(s)", util.count(b.connected_devices))
            end), theme.DIM, 12),
            cell(util.label(oblisk.keyboard, function(k)
                -- `or 1` would not help here: `layout_count` is 0 until the compositor's first
                -- layout resync, and 0 is truthy in Lua, so the fallback never fires and the
                -- line reads "layout 1 of 0". An absent count is absent, not one.
                local total = k.layout_count or 0
                if total == 0 then
                    return "layout unknown"
                end
                return string.format("layout %d of %d", (k.active_layout_index or 0) + 1, total)
            end), theme.DIM, 12),
        },
    },
}

return { button = menu_button, surface = surface }
