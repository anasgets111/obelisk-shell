-- Mirrors LeftSide.qml, including the order: power menu first, then the status modules that config
-- keeps on the left, then the launcher opener and the wallpaper button, then the special workspaces
-- (Hyprland only, absent elsewhere) and the workspace strip last.
--
-- `rescue` has no counterpart there and goes first anyway, because a config error is the one thing
-- that must not be pushed off the edge by whatever is beside it. It occupies no width unless the
-- config has actually failed.
--
-- `privacy` follows it for the same reason and against the mirror, which puts it on the right: both
-- are alerts rather than readouts, and both are absent most of the time.
local theme = require("config.theme")
local rescue_cell = require("modules.bar.indicators.rescue")
local privacy_module = require("modules.bar.indicators.privacy")
local power_menu = require("modules.bar.panels.power_menu")
local updates_module = require("modules.bar.indicators.updates")
local idle_inhibitor = require("modules.bar.indicators.idle_inhibitor")
local keyboard_module = require("modules.bar.indicators.keyboard_layout")
local battery = require("modules.bar.indicators.battery")
local launcher = require("modules.bar.indicators.launcher_button")
local wallpaper_button = require("modules.bar.indicators.wallpaper_button")
local special_workspaces = require("modules.bar.indicators.special_workspaces")
local workspaces_module = require("modules.bar.indicators.workspace_strip")

-- `width = "Fill"`, not a percentage. This and `right_side.lua` are the two `Fill` children of the
-- bar's row, so they split whatever the content-sized centre zone leaves, evenly and at any module
-- width. `modules/bar/init.lua` records what that replaced and why.
return row {
    width = "Fill",
    height = "Fill",
    align_h = "Start",
    align_v = "Center",
    spacing = theme.spacing.sm,
    children = {
        rescue_cell,
        privacy_module,
        power_menu.button,
        updates_module,
        idle_inhibitor.indicator,
        keyboard_module,
        battery.indicator,
        launcher.button,
        wallpaper_button.button,
        special_workspaces,
        workspaces_module,
    },
}
