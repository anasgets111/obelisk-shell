-- The bell half of the mirror's DateTimeDisplay, which puts the notification state and the clock in
-- one control rather than two. Returned as parts so `right_side.lua` can assemble that, since the
-- two halves open different panels: the bell opens the history, the date opens the calendar.
--
-- Two glyphs, and which one shows is the whole readout: waiting with a count, or nothing. The count
-- is part of the glyph's own string rather than a badge beside it, which is what the mirror does.
--
-- The mirror's third glyph, do-not-disturb, reads `notifications.dnd` and wins over the count: a
-- bell that is off says so before it says how many it is holding back. The toggle is in the
-- history panel this opens.
local theme = require("config.theme")
local icons = require("config.icons")
local cell = require("components.cell")
local ui_state = require("lib.ui_state")
local notification_history = require("modules.bar.panels.notification_history")

local function waiting(n)
    return #((n and n.feed) or {})
end

local bell = cell(oblisk.notifications:map(function(n)
    if n and n.dnd then
        return icons.bell_off
    end
    local count = waiting(n)
    if count > 0 then
        return icons.bell_active .. " " .. tostring(count)
    end
    return icons.bell
end), oblisk.notifications:map(function(n)
    if n and n.dnd then
        return theme.DIM
    end
    return waiting(n) > 0 and theme.ACCENT or theme.text_contrast(theme.GLASS_CONTROL)
end), theme.font.md, { align_v = "Center" })

return {
    kind = notification_history.kind,
    bell = bell,
    open = function(rect)
        ui_state.toggle_panel(notification_history.kind, rect)
    end,
}
