-- A small dim label that splits a panel body into named sections, nothing more than a styled
-- `cell` with its own file: `modules/bar/panels/settings.lua` reads better as "system" and
-- "bluetooth" than as an unbroken column of readouts, and every other panel this config grows will
-- want the same split.
local theme = require("config.theme")

return function(content)
    return text { content = content, foreground = theme.DIM, font_size = 10 }
end
