-- Mirrors RightSide.qml: status indicators, tray, then the clock at the edge.
--
-- No brightness module, matching the reference. Its level and controls live in
-- `modules/bar/panels/power_menu.lua`, where there is room for labels.
--
-- `privacy` is on the left beside `rescue`, unlike RightSide.qml; both are intermittent alerts.
local theme = require("config.theme")
local volume_module = require("modules.bar.indicators.volume")
local network = require("modules.bar.indicators.network")
local bluetooth = require("modules.bar.indicators.bluetooth")
local tray_module = require("modules.bar.indicators.sys_tray")
local bell = require("modules.bar.indicators.notification_bell")
local date_time = require("modules.bar.indicators.date_time")
local ui_state = require("lib.ui_state")
local calendar_panel = require("modules.bar.panels.minimal_calendar")

-- One control holds bell and clock, as in `DateTimeDisplay.qml`. The bell opens history and the
-- date
-- opens the calendar; one ground makes them read as one clock.
--
-- The row owns the ground; transparent inner buttons remove the seam and shade the whole hover
-- area.
local clock_slot = date_time.slot

local function transparent_button(child, on_click)
    return button {
        height = "Fill",
        align_v = "Center",
        padding = { left = theme.spacing.sm, right = theme.spacing.sm },
        on_click = function(rect, mouse_button)
            if mouse_button ~= "left" then
                return
            end
            on_click(rect)
        end,
        children = { child },
    }
end

local clock_pill = row {
    height = theme.item_height,
    align_v = "Center",
    hover = hover(clock_slot),
    radius = theme.item_radius,
    background = hover(clock_slot):map(function(is_hovered)
        return is_hovered and theme.GLASS_CONTROL_HOVER or theme.GLASS_CONTROL
    end),
    border_width = theme.border_width,
    border_color = hover(clock_slot):map(function(is_hovered)
        return is_hovered and theme.GLASS_BORDER_HOVER or theme.GLASS_BORDER
    end),
    children = {
        transparent_button(bell.bell, bell.open),
        transparent_button(date_time.clock, function(rect)
            ui_state.toggle_panel(calendar_panel.kind, rect)
        end),
    },
}

return row {
    width = "Fill",
    height = "Fill",
    align_h = "End",
    align_v = "Center",
    spacing = theme.spacing.sm,
    children = {
        volume_module,
        network.indicator,
        bluetooth.indicator,
        tray_module,
        -- The clock row owns the tooltip hover region, so the whole control triggers it.
        clock_pill,
    },
}
