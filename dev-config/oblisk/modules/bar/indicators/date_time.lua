-- Mirrors DateTimeDisplay.qml, which is one control holding the notification state and the clock
-- rather than two sitting beside each other.
--
-- The date and the time are one string, not two cells. `%a %d %b  %H:%M` reads as a clock; a dim
-- date cell next to a large time cell reads as two modules that happen to be adjacent, which is
-- what this drew before. The mirror's own format is `TimeService.format("datetime")`.
--
-- Seconds are gone with them. A clock that ticks every second is a re-resolve every second for a
-- digit nobody reads on a bar, and § 4.2's `system.time` pushes at whatever cadence it pushes at
-- either way.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local tooltip = require("components.tooltip")

local SLOT = "clock"

local clock = cell(util.label(oblisk.system, function(s)
    return os.date("%a %d %b  %H:%M", s.time)
end), theme.text_contrast(theme.GLASS_CONTROL), theme.font.sm, { align_v = "Center" })

local clock_tooltip = tooltip({
    id = "clock_tooltip",
    slot = SLOT,
    width = 200,
    height = 60,
    children = {
        cell(util.label(oblisk.system, function(s)
            return os.date("%A %d %B %Y", s.time)
        end), theme.FG, theme.font.sm),
        cell(util.label(oblisk.system, function(s)
            return os.date("%H:%M:%S %Z", s.time)
        end), theme.DIM, theme.font.xs),
    },
})

return { clock = clock, slot = SLOT, tooltip = clock_tooltip }
