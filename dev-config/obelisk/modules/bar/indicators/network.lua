-- Mirrors NetworkIndicator.qml: signal glyph opens the network panel. The SSID belongs in the
-- tooltip and panel; the bar has room for strength, not a network name.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local icon_button = require("components.icon_button")
local tooltip = require("components.tooltip")
local ui_state = require("lib.ui_state")
local network_panel = require("modules.bar.panels.network_panel")

local SLOT = "network"

local network_module = icon_button(obelisk.network:map(util.network_glyph), function(rect)
    local opening = not (ui_state.panel_open:get() and ui_state.panel_kind:get() == network_panel.kind)
    ui_state.toggle_panel(network_panel.kind, rect)
    if opening then
        network_panel.scan_while_open()
    end
end, {
    slot = SLOT,
    selected = ui_state.panel_showing(network_panel.kind),
    -- Lit for a link carrying the default route, not a bare association: `connected` answers the
    -- question the bar asks. A wifi link then takes its band's colour, as `networkBandColor` gives
    -- it: the band is the one fact about a connection worth a glance, and ethernet carries none.
    foreground = obelisk.network:map(function(n)
        if n == nil or not n.connected then
            return theme.TEXT_OFF
        end
        local _, colour = util.band_of(util.active_access_point(n))
        return colour
    end),
})

local network_tooltip = tooltip({
    id = "network_tooltip",
    slot = SLOT,
    children = {
        cell(obelisk.network:map(function(n)
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
        cell(obelisk.network:map(function(n)
            return string.format("%d network(s) in range", #((n or {}).available_networks or {}))
        end), theme.DIM, theme.font.xs),
    },
})

return { indicator = network_module, tooltip = network_tooltip }
