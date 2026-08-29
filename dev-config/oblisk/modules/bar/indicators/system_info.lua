-- Mirrors SystemInfoWidget.qml.
--
-- sysinfo has no data and will read "--" forever on a live session. Left in rather than deleted,
-- because the reason is worth seeing: its pollers start dormant (`watch::channel(Duration::ZERO)`)
-- and only `sysinfo:configure({cpu_interval = ...})` wakes them, which needs the Lua write path
-- from Phase 25. The field names below are the real ones (`cpu_percent`, not `cpu_pct`); the
-- previous version of this file read `cpu_pct` and would have silently shown 0% forever the day
-- Phase 25 landed, which nobody would have caught because dormant and wrong look identical here.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local pill = require("components.pill")

return pill({ cell(util.label(oblisk.sysinfo, function(s)
    return string.format("cpu %d%% ram %d%%", s.cpu_percent or 0, s.ram_percent or 0)
end), theme.DIM, 11) })
