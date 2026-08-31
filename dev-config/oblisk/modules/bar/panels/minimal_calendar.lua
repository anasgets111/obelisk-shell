-- Mirrors MinimalCalendar.qml: the current month as a grid, today marked, opened by clicking the
-- clock.
--
-- Pure arithmetic and a grid of cells, which is why it is here rather than in the ADR backlog: it
-- needs no capability, no subprocess and no engine feature that did not already exist. It was
-- missing because nothing had built a grid yet, and a grid is a `column` of `row`s once something
-- computes the offsets.
--
-- `os.date` and `os.time` rather than a capability. `oblisk.system.time` is the clock this reads
-- for *today*, so the highlight follows the real date, but the month layout is arithmetic over that
-- one number and belongs in the config (§ 2.x has no calendar and should not grow one).
local theme = require("config.theme")
local cell = require("components.cell")
local section_header = require("components.section_header")

local KIND = "calendar"
local COLUMNS = 7
local ROWS = 6
local DAY_NAMES = { "Su", "Mo", "Tu", "We", "Th", "Fr", "Sa" }

-- The day cell's side. Square, so the grid reads as a calendar rather than a table of numbers.
local DAY_SIDE = theme.s(30, 24)

-- The grid for the month containing `now`, as `ROWS * COLUMNS` entries in reading order. An entry
-- is `nil` where the cell falls outside the month, which is what makes the leading and trailing
-- blanks fall out of the same loop as the days.
--
-- `os.time` with a normalising table is doing the real work: Lua's `os.time{ day = 0 }` is the last
-- day of the previous month and `day = 32` rolls into the next, so there is no month-length table
-- here and no leap-year branch. The C library owns that.
local function month_grid(now)
    local today = os.date("*t", now)
    local first = os.date("*t", os.time({ year = today.year, month = today.month, day = 1, hour = 12 }))
    -- `wday` is 1-based from Sunday, which is the same order `DAY_NAMES` is in.
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
        -- A blank that still occupies its column. An absent child would shift the rest of the week
        -- left, which is the one thing a calendar grid must not do.
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

-- Rebuilt whenever the clock ticks, which is once a second and is 42 cells of arithmetic. That is
-- cheap and it is also wasteful, because the grid only changes at midnight.
--
-- ponytail: the cheap fix is not available. `:map` runs during scene resolution and must stay pure
-- (ADR-0044's rollback means resolution can rerun on the same inputs), so this cannot memoise on
-- the day and skip the rebuild -- a cache write is a side effect, and the second run would see a
-- different table than the first. Caching this wants either a `computed` whose inputs are the
-- day rather than the second, or `oblisk.system` pushing a date field beside `time`. The second is
-- the smaller change and is where this goes if it ever matters.
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
