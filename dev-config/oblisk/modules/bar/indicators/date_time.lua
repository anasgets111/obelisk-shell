-- Mirrors DateTimeDisplay.qml.
--
-- The clock, and the whole reason `system` exists (docs/adr/0053). `system.time` is a unix epoch in
-- seconds pushed once per wall-clock second, so `os.date` formats it the same way it would format
-- `os.time()`. The difference is that this one moves: `os.date(os.time())` freezes at whatever
-- instant the config was evaluated, because nothing re-evaluates it.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local tooltip = require("components.tooltip")

local clock = cell(util.label(oblisk.system, function(s)
    return os.date("%H:%M:%S", s.time)
end), theme.FG, 15)

local date = cell(util.label(oblisk.system, function(s)
    return os.date("%a %d %b", s.time)
end), theme.DIM, 11)

-- Quickshell's DateTimeDisplay opens a mini calendar and a weather panel on hover. There is no
-- calendar node and no weather capability, so this is the part of that tooltip the data supports:
-- the full date `%a %d %b` had to truncate, and the seconds the pill does not show.
local SLOT = "clock"

local clock_tooltip = tooltip({
    id = "clock_tooltip",
    slot = SLOT,
    width = 200,
    height = 60,
    children = {
        cell(util.label(oblisk.system, function(s)
            return os.date("%A %d %B %Y", s.time)
        end), theme.FG, 12),
        cell(util.label(oblisk.system, function(s)
            return os.date("%H:%M:%S %Z", s.time)
        end), theme.DIM, 11),
    },
})

return { clock = clock, date = date, slot = SLOT, tooltip = clock_tooltip }
