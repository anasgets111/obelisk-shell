-- Mirrors LeftSide.qml, including the order: power menu first, then the status modules that config
-- keeps on the left, then the launcher opener, then the workspace strip last.
--
-- `rescue` has no counterpart there and goes first anyway, because a config error is the one thing
-- that must not be pushed off the edge by whatever is beside it. It occupies no width unless the
-- config has actually failed.
--
-- `privacy` follows it for the same reason and against the mirror, which puts it on the right: both
-- are alerts rather than readouts, both are absent most of the time, and the right zone lost the
-- room for it when the tray started laying out horizontally (`right_side.lua` records that).
local rescue_cell = require("modules.bar.indicators.rescue")
local privacy_module = require("modules.bar.indicators.privacy")
local power_menu = require("modules.bar.panels.power_menu")
local updates_module = require("modules.bar.indicators.updates")
local keyboard_module = require("modules.bar.indicators.keyboard_layout")
local battery = require("modules.bar.indicators.battery")
local launcher = require("modules.bar.indicators.launcher_button")
local workspaces_module = require("modules.bar.indicators.workspace_strip")

return row {
    width = "46%",
    height = "Fill",
    align_h = "Start",
    align_v = "Center",
    spacing = 6,
    children = {
        rescue_cell,
        privacy_module,
        power_menu.button,
        updates_module,
        keyboard_module,
        battery.indicator,
        launcher.button,
        workspaces_module,
    },
}
