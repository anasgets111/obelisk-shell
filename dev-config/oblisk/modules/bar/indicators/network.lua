-- Mirrors NetworkIndicator.qml: one glyph whose bars say how good the link is, opening the network
-- panel. The SSID it used to spell out lives in the tooltip and in the panel, which is where the
-- mirror keeps it too -- a bar has room for signal strength, not for a network name.
local theme = require("config.theme")
local icons = require("config.icons")
local cell = require("components.cell")
local icon_button = require("components.icon_button")
local tooltip = require("components.tooltip")
local ui_state = require("lib.ui_state")
local network_panel = require("modules.bar.panels.network_panel")

local SLOT = "network"

-- Four buckets, matching `NetworkService.getWifiIcon`'s own tiering of a 0..100 strength.
local function network_glyph(n)
    if n == nil then
        return icons.wifi_none
    end
    if n.ssid == "Ethernet" then
        return icons.ethernet
    end
    -- A dead radio and a live one joined to nothing are different pictures, and until
    -- `NetworkState` carried `wifi_enabled` this could only draw the second one.
    if not n.networking_enabled or not n.wifi_enabled then
        return icons.wifi_off
    end
    if n.ssid == nil then
        return icons.wifi_none
    end
    local tier = math.floor(((n.strength or 0) / 100) * 3.999) + 1
    return icons.wifi[math.max(1, math.min(4, tier))]
end

local network_module = icon_button(oblisk.network:map(network_glyph), function(rect)
    oblisk.network:invoke("scan")
    ui_state.toggle_panel(network_panel.kind, rect)
end, {
    slot = SLOT,
    selected = ui_state.panel_showing(network_panel.kind),
    -- Lit for a link that carries the default route, not for a bare association: `connected` is
    -- the question a glance at the bar is asking.
    foreground = oblisk.network:map(function(n)
        return (n ~= nil and n.connected) and theme.FG or theme.TEXT_OFF
    end),
})

local network_tooltip = tooltip({
    id = "network_tooltip",
    slot = SLOT,
    width = 220,
    height = 60,
    children = {
        cell(oblisk.network:map(function(n)
            if n == nil then
                return "disconnected"
            end
            if n.ssid == "Ethernet" then
                return "ethernet"
            end
            if n.ssid then
                return string.format("%s (%d%%)", n.ssid, n.strength or 0)
            end
            if not n.networking_enabled then
                return "networking off"
            end
            if not n.wifi_enabled then
                return "wi-fi off"
            end
            return n.scanning and "scanning" or "disconnected"
        end), theme.FG, theme.font.sm),
        cell(oblisk.network:map(function(n)
            return string.format("%d network(s) in range", #((n or {}).available_networks or {}))
        end), theme.DIM, theme.font.xs),
    },
})

return { indicator = network_module, tooltip = network_tooltip }
