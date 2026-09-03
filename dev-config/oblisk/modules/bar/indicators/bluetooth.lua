-- Mirrors BluetoothIndicator.qml: three glyphs for off, on and connected, with the glyph itself
-- going accent-coloured while something is connected. The device name it used to spell out is in
-- the tooltip and the panel.
--
-- The accent is on the foreground rather than the ground, which is the mirror's own split: a ground
-- change means "this wants your attention", a foreground change means "this is doing something".
-- Bluetooth being connected is the second kind.
local theme = require("config.theme")
local icons = require("config.icons")
local cell = require("components.cell")
local icon_button = require("components.icon_button")
local tooltip = require("components.tooltip")
local ui_state = require("lib.ui_state")
local bluetooth_panel = require("modules.bar.panels.bluetooth_panel")

local SLOT = "bluetooth"

local function connected(b)
    return (b or {}).connected_devices or {}
end

local bluetooth_module = icon_button(oblisk.bluetooth:map(function(b)
    if b == nil or not b.enabled then
        return icons.bt_off
    end
    return #connected(b) > 0 and icons.bt_conn or icons.bt_on
end), function(rect)
    ui_state.toggle_panel(bluetooth_panel.kind, rect)
end, {
    slot = SLOT,
    selected = ui_state.panel_showing(bluetooth_panel.kind),
    foreground = oblisk.bluetooth:map(function(b)
        if b == nil or not b.enabled then
            return theme.TEXT_OFF
        end
        return #connected(b) > 0 and theme.ACCENT or theme.FG
    end),
})

local bluetooth_tooltip = tooltip({
    id = "bluetooth_tooltip",
    slot = SLOT,
    width = 220,
    height = 60,
    children = {
        cell(oblisk.bluetooth:map(function(b)
            if b == nil or not b.enabled then
                return "bluetooth off"
            end
            local devices = connected(b)
            if #devices == 0 then
                return "bluetooth on"
            end
            return string.format("connected (%d)", #devices)
        end), theme.FG, theme.font.sm),
        cell(oblisk.bluetooth:map(function(b)
            local first = connected(b)[1]
            if first == nil then
                return "no device connected"
            end
            if first.battery and first.battery >= 0 then
                return string.format("%s -- %d%%", first.name or "?", first.battery)
            end
            return first.name or "?"
        end), theme.DIM, theme.font.xs),
    },
})

return { indicator = bluetooth_module, tooltip = bluetooth_tooltip }
