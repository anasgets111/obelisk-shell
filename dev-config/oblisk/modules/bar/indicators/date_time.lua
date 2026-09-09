-- Mirrors DateTimeDisplay.qml: one control holds the notification state and clock.
--
-- Date and time are one string, `%a %d %b  %I:%M %p`, rather than adjacent cells. A dim date cell next
-- to a large time cell read as two modules, which this drew before. The mirror uses
-- `TimeService.format("datetime")`.
--
-- Seconds are omitted. A per-second clock re-resolves for a digit nobody reads; § 4.2's
-- `system.time` still pushes at its own cadence.
--
-- Twelve-hour with AM/PM, fixed. `TimeService.qml` asks `Qt.locale()` whether the short time format
-- carries an `AP` marker and picks `HH:mm` or `hh:mm AP` from the answer; a config has no locale to
-- ask, so the choice is made here instead of guessed.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local tooltip = require("components.tooltip")
local calendar = require("modules.bar.panels.minimal_calendar")

local SLOT = "clock"

-- `DateTimeDisplay.qml`'s clock is `bold: true`; it is the bar's one always-on readout and the
-- weight is what separates it from the indicators either side.
local clock = cell(util.label(oblisk.system, function(s)
    return os.date("%a %d %b  %I:%M %p", s.time)
end):map(function(shown)
    return { { text = shown, bold = true } }
end), theme.text_contrast(theme.GLASS_CONTROL), theme.font.sm, { align_v = "Center" })

-- `DateTimeDisplay.qml` hangs `MinimalCalendar` off this tooltip, which is where the month grid
-- belongs: the clock's click opens the notifications panel, and a calendar nobody asked for should
-- not be what a click on the bar's one always-visible readout produces.
--
-- The one tooltip that still declares its size. Every other one omits `width`/`height` and is
-- measured (`components/tooltip.lua`); this one's rows fill the card instead of sizing it -- the
-- two lines are `width = "Fill"` and the grid is a fixed cell -- so there is nothing for a
-- content-sized measurement to read. The height is the calendar's own plus the two lines above it
-- and `panel_card`'s spacing and padding, following the calendar's signal because a month is four
-- to six weeks tall.
local DATE_LINE = math.ceil(theme.font.sm * 1.2)
local TIME_LINE = math.ceil(theme.font.xs * 1.2)

local clock_tooltip = tooltip({
    id = "clock_tooltip",
    slot = SLOT,
    width = calendar.width + theme.spacing.sm * 2,
    -- The month grid's bottom row is a `DAY_SIDE` cell around a much smaller glyph, so it carries
    -- its own air; the date line at the top has none, and the shared `xs` under the border left it
    -- against the edge. `md` is what the panel card uses for the same reason.
    padding_v = theme.spacing.md,
    height = calendar.height:map(function(grid)
        return grid + DATE_LINE + TIME_LINE + theme.spacing.md * 2 + theme.spacing.xs * 2
    end),
    children = {
        cell(util.label(oblisk.system, function(s)
            return os.date("%A %d %B %Y", s.time)
        end), theme.FG, theme.font.sm, { width = "Fill", align = "Center" }),
        cell(util.label(oblisk.system, function(s)
            return os.date("%I:%M:%S %p", s.time)
        end), theme.DIM, theme.font.xs, { width = "Fill", align = "Center" }),
        calendar.node,
    },
})

return { clock = clock, slot = SLOT, tooltip = clock_tooltip }
