-- Mirrors MinimalCalendar.qml: current month grid with today marked, opened by the clock.
--
-- Pure arithmetic and existing nodes, so it needs no capability, subprocess, or engine feature. A
-- grid is a `column` of `row`s once the offsets are computed.
--
-- Use `os.date`/`os.time`, with `oblisk.system.time` supplying *today*. § 2.x has no calendar
-- capability and should not grow one.
local theme = require("config.theme")
local cell = require("components.cell")
local section_header = require("components.section_header")

local KIND = "calendar"
local COLUMNS = 7
local ROWS = 6
local DAY_NAMES = { "Su", "Mo", "Tu", "We", "Th", "Fr", "Sa" }

-- Square day cells keep the grid calendar-like.
local DAY_SIDE = theme.s(30, 24)

-- `ROWS * COLUMNS` entries in reading order; out-of-month entries are `nil`, so leading and
-- trailing
-- blanks use the same loop.
--
-- Lua's normalizing `os.time` makes `day = 0` the previous month's last day and `day = 32` roll
-- into
-- the next, avoiding a month-length table and leap-year branch.
local function month_grid(now)
    local today = os.date("*t", now)
    local first = os.date("*t", os.time({ year = today.year, month = today.month, day = 1, hour = 12 }))
    -- `wday` is 1-based from Sunday, matching `DAY_NAMES`.
    local lead = first.wday - 1
    local days_in_month = os.date("*t", os.time({ year = today.year, month = today.month + 1, day = 0, hour = 12 })).day

    local cells = {}
    for index = 1, ROWS * COLUMNS do
        local day = index - lead
        cells[index] = {
            index = index,
            day = (day >= 1 and day <= days_in_month) and day or nil,
            is_today = day == today.day,
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
        radius = theme.radius.sm,
        background = entry.is_today and theme.ACCENT or nil,
        children = {
            cell(tostring(entry.day), entry.is_today and theme.BG or theme.FG, theme.font.sm, {
                width = "Fill",
                align = "Center",
                align_v = "Center",
            }),
        },
    }
end

local function week_rows(now)
    local cells = month_grid(now)
    local rows = {}
    for week = 0, ROWS - 1 do
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
            names[column] = cell(name, theme.TEXT_OFF, theme.font.xs, {
                width = DAY_SIDE,
                align = "Center",
            })
        end
        return names
    end)(),
}

-- Rebuilt on each one-second clock tick, 42 cells of arithmetic, although the grid changes only at
-- midnight.
--
-- ponytail: `:map` must stay pure during scene resolution (ADR-0044 can rerun it on the same
-- inputs), so day memoization cannot write a cache. Upgrade to a day-based `computed` input or add
-- a date field beside `oblisk.system.time`; the latter is smaller.
local grid = column {
    width = "Fill",
    spacing = theme.spacing.xs,
    children = oblisk.system:map(function(s)
        return week_rows(s and s.time or os.time())
    end),
}

local body = {
    row {
        width = "Fill",
        align_v = "Center",
        children = {
            section_header("calendar"),
            cell(oblisk.system:map(function(s)
                return os.date("%B %Y", (s and s.time) or os.time())
            end), theme.FG, theme.font.sm, { width = "Fill", align = "End" }),
        },
    },
    heading,
    grid,
}

return { kind = KIND, body = body }
