-- Mirrors BluetoothIndicator.qml. Clicking opens the bluetooth panel, `network.lua`'s shape.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local pill = require("components.pill")
local ui_state = require("lib.ui_state")
local bluetooth_panel = require("modules.bar.panels.bluetooth_panel")

return pill({
    button {
        height = 18,
        align_v = "Center",
        on_click = function(rect, mouse_button)
            if mouse_button ~= "left" then
                return
            end
            ui_state.open_panel(bluetooth_panel.kind, rect)
        end,
        children = { cell(util.label(oblisk.bluetooth, function(b)
            if not b.enabled then
                return "bt off"
            end
            local connected = b.connected_devices or {}
            if #connected == 0 then
                return "bt"
            end
            local first = connected[1]
            -- `battery` is -1 when BlueZ exposes no `Battery1` for the device, which is not 0
            -- percent and must not render as it.
            if first.battery and first.battery >= 0 then
                return string.format("%s %d%%", util.truncate(first.name, 12), first.battery)
            end
            return util.truncate(first.name, 12)
        end), theme.ACCENT) },
    },
})
