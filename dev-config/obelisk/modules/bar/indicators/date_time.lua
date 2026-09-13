-- Mirrors DateTimeDisplay.qml: one control holds the notification state and clock.
--
-- Date and time share `%a %d %b  %I:%M %p`; separate cells read as two modules. The mirror uses
-- `TimeService.format("datetime")`.
--
-- Seconds are omitted. A per-second clock re-resolves for a digit nobody reads; `system.time`
-- still pushes at its own cadence.
--
-- Twelve-hour with AM/PM, fixed. `TimeService.qml` asks `Qt.locale()` whether the short time format
-- carries an `AP` marker and picks `HH:mm` or `hh:mm AP` from the answer; a config has no locale to
-- ask, so the choice is made here instead of guessed.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local tooltip = require("components.tooltip")
local calendar = require("modules.bar.panels.minimal_calendar")
local weather = require("lib.weather")

local SLOT = "clock"

-- `DateTimeDisplay.qml`'s clock is `bold: true`; it is the bar's one always-on readout and the
-- weight is what separates it from the indicators either side.
--
-- `text: WeatherService.currentTemp + " " + TimeService.format("datetime")` puts the reading in
-- front of the time. The mirror's `currentTemp` starts as the literal "Loading...", which it then
-- shows on the bar until the first request lands; here the clock simply stands alone until there is
-- a reading, which `weather_code`'s `-1` default says.
local clock = cell(computed({ obelisk.system, weather.code, weather.temperature },
    function(s, code, celsius)
        local shown = os.date("%a %d %b  %I:%M %p", s and s.time)
        if (code or -1) >= 0 then
            shown = string.format("%d°C %s %s", celsius or 0, weather.info(code).icon, shown)
        end
        return { { text = shown, bold = true } }
    end), theme.text_contrast(theme.GLASS_CONTROL), theme.font.sm, { align_v = "Center" })

-- `DateTimeDisplay.qml` hangs `MinimalCalendar` off this tooltip. The whole control opens
-- notifications; a click on the always-visible readout should not open a calendar panel.
--
-- This is the only tooltip with explicit `width`/`height`; other tooltips are measured
-- (`components/tooltip.lua`). Its `width = "Fill"` rows and fixed-cell grid leave no content-sized
-- extent to measure. Height adds the calendar's signal, the two lines, `panel_card` spacing and
-- padding; a month is four to six weeks tall.
local DATE_LINE = math.ceil(theme.font.sm * 1.2)
local TIME_LINE = math.ceil(theme.font.xs * 1.2)
-- `weatherInfo().desc`, "in <place>" and "Last updated <ago>" sit above the month in the mirror.
-- Two more `sm`/`xs` lines and the two gaps they add, on a tip whose height is declared rather than
-- measured.
local WEATHER_LINES = DATE_LINE + TIME_LINE + theme.spacing.xs * 2

local clock_tooltip = tooltip({
    id = "clock_tooltip",
    slot = SLOT,
    width = calendar.width + theme.spacing.sm * 2,
    -- The grid's bottom row is a `DAY_SIDE` cell around a smaller glyph, so it carries its own air.
    -- The date line has none; shared `xs` left it against the border, while `md` matches the card.
    padding_v = theme.spacing.md,
    height = calendar.height:map(function(grid)
        return grid + DATE_LINE + TIME_LINE + WEATHER_LINES + theme.spacing.md * 2 + theme.spacing.xs * 2
    end),
    children = {
        -- The description and where it was measured. The mirror is two `OText`s in a `Row`; two
        -- runs in one cell is the same two colours in a box that can elide, which a content-sized
        -- row cannot. This tip declares its width, and "Thunderstorm: Slight or moderate in Giza,
        -- Egypt" is half again wider than a month grid.
        cell(computed({ weather.code, weather.location }, function(code, where)
            if (code or -1) < 0 then
                return ""
            end
            local place = where and where.place_name or ""
            return {
                { text = weather.info(code).desc },
                { text = place ~= "" and (" in " .. place) or "", color = theme.DIM },
            }
        end), theme.FG, theme.font.sm, { width = "Fill", align = "Center" }),
        cell(computed({ weather.updated_at, obelisk.system }, function(at, s)
            local ago = weather.time_ago(at, s and s.time)
            return ago ~= "" and ("Last updated " .. ago) or ""
        end), theme.DIM, theme.font.xs, { width = "Fill", align = "Center" }),
        cell(util.label(obelisk.system, function(s)
            return os.date("%A %d %B %Y", s.time)
        end), theme.FG, theme.font.sm, { width = "Fill", align = "Center" }),
        cell(util.label(obelisk.system, function(s)
            return os.date("%I:%M:%S %p", s.time)
        end), theme.DIM, theme.font.xs, { width = "Fill", align = "Center" }),
        calendar.node,
    },
})

return { clock = clock, slot = SLOT, tooltip = clock_tooltip }
