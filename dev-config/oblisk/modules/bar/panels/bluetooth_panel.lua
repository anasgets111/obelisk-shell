-- Mirrors Bar/Panels/BluetoothPanel.qml. Opened by `modules/bar/indicators/bluetooth.lua`.
--
-- The `enabled` toggle used to live in `modules/bar/panels/settings.lua`, which is where it landed
-- when the settings window was the only panel this config had. A bluetooth control belongs behind
-- the bluetooth indicator, and the settings window keeps the readouts that have no indicator of
-- their own.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local section_header = require("components.section_header")
local panel_toggle_card = require("components.panel_toggle_card")

local KIND = "bluetooth"

local body = {
    section_header("bluetooth"),
    -- The one § 3.2 write this panel has, and the reason `components/toggle.lua` exists.
    panel_toggle_card("enabled", oblisk.bluetooth, function(b)
        return b.enabled
    end, function(new_value)
        oblisk.bluetooth:invoke("set_enabled", new_value)
    end),
    cell(util.label(oblisk.bluetooth, function(b)
        local connected = b.connected_devices or {}
        if #connected == 0 then
            return "nothing connected"
        end
        return string.format("%d connected device(s)", #connected)
    end), theme.DIM, 11),
}

return { kind = KIND, body = body }
