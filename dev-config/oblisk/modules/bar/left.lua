local workspaces_module = require("modules.bar.indicators.workspace_strip")
local media = require("modules.bar.indicators.media")
local window_title_module = require("modules.bar.indicators.active_window")
local tray_module = require("modules.bar.indicators.sys_tray")
local brightness_module = require("modules.bar.indicators.brightness")

return row {
    width = "40%",
    height = "Fill",
    align_h = "Start",
    align_v = "Center",
    spacing = 6,
    children = { workspaces_module, window_title_module, media, tray_module, brightness_module },
}
