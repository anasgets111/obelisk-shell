-- Mirrors NetworkIndicator.qml: signal glyph opens the network panel. The SSID belongs in the
-- tooltip and panel; the bar has room for strength, not a network name.
local theme = require("config.theme")
local icons = require("config.icons")
local cell = require("components.cell")
local icon_button = require("components.icon_button")
local tooltip = require("components.tooltip")
local ui_state = require("lib.ui_state")
local network_panel = require("modules.bar.panels.network_panel")

local SLOT = "network"

-- Four buckets, matching `NetworkService.getWifiIcon`'s 0..100 tiering.
local function network_glyph(n)
    if n == nil then
        return icons.wifi_none
    end
    if n.ssid == "Ethernet" then
        return icons.ethernet
    end
    -- A dead radio and a live one joined to nothing are different pictures. `wifi_enabled` was
    -- added to `NetworkState` so the first can be drawn.
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
    -- Lit for a link carrying the default route, not a bare association: `connected` answers the
    -- question the bar asks.
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
