-- Mirrors MinimalCalendar.qml: current month grid with today marked.
--
-- Pure arithmetic and existing nodes need no capability, subprocess, or engine feature. A grid is a
-- `column` of `row`s once the offsets are computed.
--
-- Use `os.date`/`os.time`, with `obelisk.system.time` supplying *today*. § 2.x has no calendar
-- capability and should not grow one.
--
-- Not a panel. `DateTimeDisplay.qml` puts this in the clock's hover tooltip; a click opens the
-- notifications panel, so the always-visible bar readout does not open a month grid.
-- `modules/bar/indicators/date_time.lua` owns the tooltip this goes in.
local theme = require("config.theme")
local cell = require("components.cell")

local COLUMNS = 7
local DAY_NAMES = { "Su", "Mo", "Tu", "We", "Th", "Fr", "Sa" }

-- Square day cells keep the grid calendar-like.
local DAY_SIDE = theme.s(30, 24)

-- Where the month starts in the week and how long it is.
--
-- Lua's normalizing `os.time` makes `day = 0` the previous month's last day and `day = 32` roll
-- into the next, avoiding a month-length table and leap-year branch.
local function month_of(now)
    local today = os.date("*t", now)
    local first = os.date("*t", os.time({ year = today.year, month = today.month, day = 1, hour = 12 }))
    -- `wday` is 1-based from Sunday, matching `DAY_NAMES`.
    local lead = first.wday - 1
    local days_in_month = os.date("*t", os.time({ year = today.year, month = today.month + 1, day = 0, hour = 12 })).day
    return today, lead, days_in_month
end

-- `rowCount: Math.ceil((firstDayOffset + daysInMonth) / 7)`. Four to six rows; the mirror sizes
-- itself to the answer. Fixed six-week sizing left seven blank cells under September 2026.
-- That made the tooltip too tall for its contents.
local function rows_in(now)
    local _, lead, days_in_month = month_of(now)
    return math.ceil((lead + days_in_month) / COLUMNS)
end

-- `rows_in * COLUMNS` entries in reading order; leading blanks are `nil`, so the first week uses
-- the same loop as the rest. Trailing blanks exist only inside the last week the month reaches.
local function month_grid(now)
    local today, lead, days_in_month = month_of(now)
    local cells = {}
    for index = 1, rows_in(now) * COLUMNS do
        local day = index - lead
        cells[index] = {
            index = index,
            day = (day >= 1 and day <= days_in_month) and day or nil,
            is_today = day == today.day,
            -- `DAY_NAMES` is 1-based from Sunday, so the last column is Saturday. The heading marks
            -- it and the mirror marks the column under it the same way.
            is_saturday = (index - 1) % COLUMNS == COLUMNS - 1,
        }
    end
    return cells
end

local function day_cell(entry)
    if entry.day == nil then
        -- Keep the blank in its column; an absent child would shift the week left.
        return rect { width = DAY_SIDE, height = DAY_SIDE }
    end
    return rect {
        width = DAY_SIDE,
        height = DAY_SIDE,
        radius = DAY_SIDE / 2,
        background = entry.is_today and theme.ACCENT or nil,
        children = {
            -- `color: isToday ? textContrast(activeColor) : isSaturday ? textContrast(bgColor) :
            -- textColor`, and `bold: isToday`. Today is the one date read at a glance, so it
            -- carries the weight as well as the disc.
            cell(
                { { text = tostring(entry.day), bold = entry.is_today } },
                entry.is_today and theme.text_contrast(theme.ACCENT)
                or entry.is_saturday and theme.text_contrast(theme.BG)
                or theme.FG,
                theme.font.sm,
                {
                    width = "Fill",
                    align = "Center",
                    align_v = "Center",
                }
            ),
        },
    }
end

local function week_rows(now)
    local cells = month_grid(now)
    local rows = {}
    for week = 0, rows_in(now) - 1 do
        local days = {}
        for column = 1, COLUMNS do
            days[column] = day_cell(cells[week * COLUMNS + column])
        end
        rows[#rows + 1] = row {
            width = "Fill",
            spacing = theme.spacing.xs,
            children = days,
        }
    end
    return rows
end

local heading = row {
    width = "Fill",
    spacing = theme.spacing.xs,
    children = (function()
        local names = {}
        for column, name in ipairs(DAY_NAMES) do
            local color = column == 7 and theme.text_contrast(theme.BG) or theme.FG
            names[column] = cell(
                { { text = name, bold = true } },
                color,
                theme.font.xs,
                {
                    width = DAY_SIDE,
                    align = "Center",
                }
            )
        end
        return names
    end)(),
}

-- Rebuilt on each one-second clock tick, at most 42 cells of arithmetic, although the grid changes
-- only at midnight.
--
-- ponytail: `:map` must stay pure during scene resolution (ADR-0044 can rerun it on the same
-- inputs), so day memoization cannot write a cache. Upgrade to a day-based `computed` input or add
-- a date field beside `obelisk.system.time`; the latter is smaller.
local grid = column {
    width = "Fill",
    spacing = theme.spacing.xs,
    children = obelisk.system:map(function(s)
        return week_rows(s and s.time or os.time())
    end),
}

-- `Qt.formatDate(new Date(year, month), "MMMM yyyy")`, bold and centred over the grid.
local title = cell(obelisk.system:map(function(s)
    return { { text = os.date("%B %Y", (s and s.time) or os.time()), bold = true } }
end), theme.FG, theme.font.sm, { width = "Fill", align = "Center" })

-- `implicitWidth`/`implicitHeight` are measured by the mirror, but § 6 sizes a popup surface
-- explicitly, so its host cannot measure this like a `Column`. Height follows row count: a
-- five-week month is one `DAY_SIDE` shorter than a six-week one, and § 6 takes `integer|Bound`.
--
-- 1.2 is `renderer::text::shaping::LINE_HEIGHT_RATIO`. Sizing a fixed surface is the one place a
-- config has to know it; everything else lets the engine measure.
local function line_of(font_size)
    return math.ceil(font_size * 1.2)
end

return {
    width = COLUMNS * DAY_SIDE + (COLUMNS - 1) * theme.spacing.xs,
    height = obelisk.system:map(function(s)
        local rows = rows_in((s and s.time) or os.time())
        return line_of(theme.font.sm) + line_of(theme.font.xs) + rows * DAY_SIDE + (rows + 1) * theme.spacing.xs
    end),
    node = column {
        width = "Fill",
        spacing = theme.spacing.xs,
        children = { title, heading, grid },
    },
}
