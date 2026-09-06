-- Not in the mirror. First on the bar so a config error cannot be pushed off its edge; it occupies
-- no width unless configuration failed (ADR-0046).
--
-- A red warning circle, the same size as its glyph neighbours, keeps a failed bar's shape stable.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local icon_button = require("components.icon_button")

return icon_button(icons.warning, nil, {
    background = theme.RED,
    visible = util.shown_when(oblisk.rescue, function(r)
        return r.error_log ~= nil and r.error_log ~= ""
    end),
})
