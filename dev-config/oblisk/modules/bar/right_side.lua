-- Mirrors RightSide.qml, including the order: the status indicators, then the tray, then the clock
-- last against the edge.
--
-- There is no brightness module, which is also true of the config this mirrors. The level and its
-- two controls live in `modules/bar/panels/power_menu.lua`, where a panel has the width for a
-- labelled control and the bar does not.
--
-- `privacy` is on the left instead, which RightSide.qml would not do. It belongs beside `rescue`:
-- both are alerts rather than readouts, and both are absent most of the time.
local theme = require("config.theme")
local volume_module = require("modules.bar.indicators.volume")
local network = require("modules.bar.indicators.network")
local bluetooth = require("modules.bar.indicators.bluetooth")
local tray_module = require("modules.bar.indicators.sys_tray")
local bell = require("modules.bar.indicators.notification_bell")
local date_time = require("modules.bar.indicators.date_time")
local ui_state = require("lib.ui_state")
local calendar_panel = require("modules.bar.panels.minimal_calendar")

-- One control holding the bell and the clock, which is what `DateTimeDisplay.qml` is. Two buttons
-- inside one ground rather than two controls side by side: the bell opens the notification history
-- and the date opens the calendar, and the mirror's single ground is what makes them read as one
-- clock rather than as two more indicators.
--
-- The ground is the row, and the two buttons inside it carry no background of their own, so the
-- seam between them is invisible and the hover shading covers the whole control at once.
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
            ui_state.open_panel(calendar_panel.kind, rect)
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
        -- The clock row declares the hover region its tooltip reads, so the whole control is the
        -- trigger rather than either button inside it.
        clock_pill,
    },
}
