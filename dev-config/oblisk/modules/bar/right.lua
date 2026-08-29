local rescue_cell = require("modules.bar.indicators.rescue")
local privacy_module = require("modules.bar.indicators.privacy")
local updates_module = require("modules.bar.indicators.updates")
local keyboard_module = require("modules.bar.indicators.keyboard_layout")
local network_module = require("modules.bar.indicators.network")
local bluetooth_module = require("modules.bar.indicators.bluetooth")
local volume_module = require("modules.bar.indicators.volume")
local battery_module = require("modules.bar.indicators.battery")
local menu = require("modules.bar.panels.menu")
local lock_button = require("modules.bar.indicators.session")

return row {
    width = "40%",
    height = "Fill",
    align_h = "End",
    align_v = "Center",
    spacing = 6,
    children = {
        rescue_cell,
        privacy_module,
        updates_module,
        keyboard_module,
        network_module,
        bluetooth_module,
        volume_module,
        battery_module,
        menu.button,
        lock_button,
    },
}
