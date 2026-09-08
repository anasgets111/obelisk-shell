-- Mirrors RightSide.qml: status indicators, tray, then the clock at the edge.
--
-- No brightness module, matching the reference. Its level and controls live in
-- `modules/bar/panels/power_menu.lua`, where there is room for labels.
--
-- `privacy` leads this row, as in RightSide.qml. It had been on the left beside `rescue`.
local theme = require("config.theme")
local privacy_module = require("modules.bar.indicators.privacy")
local volume_module = require("modules.bar.indicators.volume")
local network = require("modules.bar.indicators.network")
local bluetooth = require("modules.bar.indicators.bluetooth")
local tray_module = require("modules.bar.indicators.sys_tray")
local bell = require("modules.bar.indicators.notification_bell")
local date_time = require("modules.bar.indicators.date_time")
local ui_state = require("lib.ui_state")

-- One control holds bell and clock, as in `DateTimeDisplay.qml`, and one `MouseArea` fills it: the
-- mirror's whole readout opens the notifications panel. Splitting it -- bell to history, date to a
-- calendar panel -- meant the bar's one always-visible control opened a month grid half the time,
-- and the calendar has gone back to the clock's hover tooltip where the mirror keeps it
-- (ADR-0174).
local clock_slot = date_time.slot
local hovered = hover(clock_slot)

-- `panelOpen` is the mirror's third state for this control, above hover: `border.color: panelOpen ?
-- activeColor : ...` rings it while its own panel is up, so the pill says which panel is showing
-- rather than leaving that to the panel's position.
local panel_showing = computed({ ui_state.panel_open, ui_state.panel_kind }, function(open, kind)
    return open and kind == bell.kind
end)

local lit = computed({ hovered, panel_showing }, function(is_hovered, is_open)
    return is_hovered or is_open
end)

local clock_pill = button {
    height = theme.item_height,
    align_v = "Center",
    hover = hovered,
    radius = theme.item_radius,
    background = lit:map(function(on)
        return on and theme.GLASS_CONTROL_HOVER or theme.GLASS_CONTROL
    end),
    border_width = theme.border_width,
    border_color = computed({ hovered, panel_showing }, function(is_hovered, is_open)
        if is_open then
            return theme.ACCENT
        end
        return is_hovered and theme.GLASS_BORDER_HOVER or theme.GLASS_BORDER
    end),
    animate = { background = theme.animation_ms },
    on_click = function(rect, mouse_button)
        if mouse_button == "left" then
            bell.open(rect)
        end
    end,
    children = { row {
        height = "Fill",
        align_v = "Center",
        spacing = theme.spacing.xs,
        -- `DateTimeDisplay.qml` insets its end children instead, `leftPadding` on the bell and
        -- `rightPadding` on the clock, both `spacingSm`. One padding on the row is the same inset
        -- and survives either child changing. Without it the bell and the minutes run under the
        -- corner radius, which at half the item height is the whole end of the pill.
        padding = { left = theme.spacing.sm, right = theme.spacing.sm },
        children = { bell.bell, date_time.clock },
    } },
}

return row {
    width = "Fill",
    height = "Fill",
    align_h = "End",
    align_v = "Center",
    spacing = theme.spacing.sm,
    children = {
        privacy_module,
        volume_module,
        network.indicator,
        bluetooth.indicator,
        tray_module,
        -- The clock row owns the tooltip hover region, so the whole control triggers it.
        clock_pill,
    },
}
