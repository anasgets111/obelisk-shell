-- Not in the mirror, and first on the bar anyway: a config error is the one thing that must not be
-- pushed off the edge by whatever is beside it. It occupies no width unless the config has failed
-- (ADR-0046).
--
-- A red warning circle rather than the words "config error", now that every other module on this
-- bar is a glyph. It is the same size as its neighbours, so a bar that has broken looks like a bar
-- with one red circle on it rather than a bar that has changed shape.
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
