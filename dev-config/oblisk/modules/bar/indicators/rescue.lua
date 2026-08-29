-- Mirrors no Quickshell file. That config cannot fail the way this one can: a QML error is a
-- console warning and a blank item, while a raise here fails the whole re-evaluation, so `rescue`
-- exists to say so on the bar itself.

local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local pill = require("components.pill")

-- Only rendered when the config itself has failed, so `rescue` is the one signal whose absence is
-- the healthy case (§ 2.10).
local rescue_cell = row {
    height = "Fill",
    align_v = "Center",
    visible = util.shown_when(oblisk.rescue, function(r)
        return r.error_log ~= nil and r.error_log ~= ""
    end),
    children = { pill({ cell("config error", theme.RED) }, "#45253aff") },
}

return rescue_cell
