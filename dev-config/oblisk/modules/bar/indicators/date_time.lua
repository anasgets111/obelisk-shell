-- Mirrors DateTimeDisplay.qml.
--
-- The clock, and the whole reason `system` exists (docs/adr/0053). `system.time` is a unix epoch in
-- seconds pushed once per wall-clock second, so `os.date` formats it the same way it would format
-- `os.time()`. The difference is that this one moves: `os.date(os.time())` freezes at whatever
-- instant the config was evaluated, because nothing re-evaluates it.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")

local clock = cell(util.label(oblisk.system, function(s)
    return os.date("%H:%M:%S", s.time)
end), theme.FG, 15)

local date = cell(util.label(oblisk.system, function(s)
    return os.date("%a %d %b", s.time)
end), theme.DIM, 11)

return { clock = clock, date = date }
