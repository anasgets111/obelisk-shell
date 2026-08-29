-- One run of text at the bar's default size and weight. The smallest thing every module is built
-- out of, which is why it is a component rather than a local in `shell.lua`.
local theme = require("config.theme")

return function(content, color, size)
    return text { content = content, foreground = color or theme.FG, font_size = size or 13 }
end
