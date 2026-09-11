-- Mirrors LeftSide.qml's order: power, left-side status modules, launcher, wallpaper, special
-- workspaces (Hyprland only), then the workspace strip.
--
-- `rescue` has no counterpart and goes first so a config error cannot be pushed off the edge. It
-- occupies no width unless configuration failed.
--
-- `privacy` used to sit beside `rescue`, grouping a camera or microphone alert with a config error.
-- It now leads `RightSide.qml`'s row for session hardware.
local theme = require("config.theme")
local rescue_cell = require("modules.bar.indicators.rescue")
local power_menu = require("modules.bar.panels.power_menu")
local updates_module = require("modules.bar.indicators.updates")
local idle_inhibitor = require("modules.bar.indicators.idle_inhibitor")
local keyboard_module = require("modules.bar.indicators.keyboard_layout")
local battery = require("modules.bar.indicators.battery")
local launcher = require("modules.bar.indicators.launcher_button")
local wallpaper_button = require("modules.bar.indicators.wallpaper_button")
local special_workspaces = require("modules.bar.indicators.special_workspaces")
local workspaces_module = require("modules.bar.indicators.workspace_strip")

-- `width = "Fill"`: this and `right_side.lua` split the content-sized centre's remainder evenly.
-- `modules/bar/init.lua` records what this replaced.
return row {
    width = "Fill",
    height = "Fill",
    align_h = "Start",
    align_v = "Center",
    spacing = theme.spacing.sm,
    children = {
        rescue_cell,
        power_menu.button,
        updates_module.indicator,
        idle_inhibitor.indicator,
        keyboard_module,
        battery.indicator,
        launcher.button,
        wallpaper_button.button,
        special_workspaces,
        workspaces_module,
    },
}
