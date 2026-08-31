-- A small filled pill for a count, the shape a tray item total or an unread notification count
-- wants and a bare `cell` does not have: a number sitting in the middle of a row of icons reads as
-- one more icon rather than a count unless something sets it apart.
local theme = require("config.theme")

return function(content, background)
    return row {
        height = 16,
        align_v = "Center",
        padding = { left = 5, right = 5 },
        background = background or theme.MAUVE,
        radius = 8,
        children = { text { content = content, foreground = theme.BG, font_size = 10 } },
    }
end
