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

local function active_ap(n)
    for _, ap in ipairs((n or {}).available_networks or {}) do
        if ap.active then
            return ap
        end
    end
    return nil
end

-- Four buckets, matching `NetworkService.getWifiIcon`'s own tiering of a 0..100 strength.
local function network_glyph(n)
    if n == nil then
        return icons.wifi_none
    end
    local ap = active_ap(n)
    if ap == nil then
        -- Disconnected, not radio-off: `NetworkState` in `supervisor/src/dbus/network/mod.rs`
        -- carries `scanning` and the AP list and nothing else, so there is no `wifi_enabled` to
        -- read and no way to tell an off radio from an on one with nothing joined. The mirror
        -- draws `icons.wifi_off` for that case and this cannot.
        return icons.wifi_none
    end
    local tier = math.floor(((ap.strength or 0) / 100) * 3.999) + 1
    return icons.wifi[math.max(1, math.min(4, tier))]
end

local network_module = icon_button(oblisk.network:map(network_glyph), function(rect)
    oblisk.network:invoke("scan")
    ui_state.open_panel(network_panel.kind, rect)
end, {
    slot = SLOT,
    selected = ui_state.panel_showing(network_panel.kind),
    foreground = oblisk.network:map(function(n)
        return active_ap(n) ~= nil and theme.FG or theme.TEXT_OFF
    end),
})

local network_tooltip = tooltip({
    id = "network_tooltip",
    slot = SLOT,
    width = 220,
    height = 60,
    children = {
        cell(oblisk.network:map(function(n)
            local ap = active_ap(n)
            if ap then
                return string.format("%s (%d%%)", ap.ssid or "wi-fi", ap.strength or 0)
            end
            return (n ~= nil and n.scanning) and "scanning" or "disconnected"
        end), theme.FG, theme.font.sm),
        cell(oblisk.network:map(function(n)
            return string.format("%d network(s) in range", #((n or {}).available_networks or {}))
        end), theme.DIM, theme.font.xs),
    },
})

return { indicator = network_module, tooltip = network_tooltip }
