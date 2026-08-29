-- Mirrors KeyboardLayoutIndicator.qml.
--
-- Truncated, and the number is a budget rather than a taste: a zone with a fixed width is a fixed
-- number of characters, and every text module in it spends from the same total. See the surfaces
-- section for what adding `power` cost.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local pill = require("components.pill")

return pill({ cell(util.label(oblisk.keyboard, function(k)
    local name = util.truncate(k.active_layout or "?", 10)
    if k.caps_lock then
        name = name .. " CAPS"
    end
    return name
end), theme.DIM) })
