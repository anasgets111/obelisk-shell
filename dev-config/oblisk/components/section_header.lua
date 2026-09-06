-- Small dim section label. `modules/bar/panels/settings.lua` is easier to scan as "system" and
-- "bluetooth" than as one column, and other panels use the same split.
local theme = require("config.theme")

return function(content)
    return text { content = content, foreground = theme.DIM, font_size = theme.font.xs }
end
