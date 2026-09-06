-- Mirrors DateTimeDisplay.qml: one control holds the notification state and clock.
--
-- Date and time are one string, `%a %d %b  %H:%M`, rather than adjacent cells. A dim date cell next
-- to a large time cell read as two modules, which this drew before. The mirror uses
-- `TimeService.format("datetime")`.
--
-- Seconds are omitted. A per-second clock re-resolves for a digit nobody reads; § 4.2's
-- `system.time` still pushes at its own cadence.
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
