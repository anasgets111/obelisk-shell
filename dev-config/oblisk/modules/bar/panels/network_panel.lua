-- Mirrors Bar/Panels/NetworkPanel.qml. Opened by `modules/bar/indicators/network.lua`, which is
-- the arrangement Quickshell uses throughout: the indicator is the affordance, the panel is the
-- detail, and neither file knows how the popup gets on screen (`modules/shell/panel_host.lua`).
--
-- Read-only, because § 3.2 gives `network` no write actions. The rows that would make this a real
-- panel -- pick an access point, forget one, toggle wifi -- are commands that do not exist yet.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local section_header = require("components.section_header")

local KIND = "network"

local body = {
    section_header("network"),
    cell(util.label(oblisk.network, function(n)
        for _, ap in ipairs(n.available_networks or {}) do
            if ap.active then
                return string.format("%s at %d%%", ap.ssid, ap.strength or 0)
            end
        end
        return "not connected"
    end), theme.FG, 12),
    cell(util.label(oblisk.network, function(n)
        return string.format("%d network(s) in range", util.count(n.available_networks))
    end), theme.DIM, 11),
    cell(util.label(oblisk.network, function(n)
        return n.scanning and "scanning" or "idle"
    end), theme.DIM, 11),
}

return { kind = KIND, body = body }
