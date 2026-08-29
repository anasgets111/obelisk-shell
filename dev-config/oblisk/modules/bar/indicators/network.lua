-- Mirrors NetworkIndicator.qml.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local pill = require("components.pill")

return pill({ cell(util.label(oblisk.network, function(n)
    for _, ap in ipairs(n.available_networks or {}) do
        if ap.active then
            return string.format("%s %d%%", util.truncate(ap.ssid, 12), ap.strength or 0)
        end
    end
    return n.scanning and "scanning" or "offline"
end), theme.ACCENT) })
