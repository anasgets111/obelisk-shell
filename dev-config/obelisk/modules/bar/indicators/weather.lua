-- Mirrors `Bar/Indicators/WeatherWidget.qml`: three day cards that open into a ten-day grid.
--
-- Like `modules/bar/indicators/system_info.lua`, this lives under `indicators/` because the mirror
-- does, but nothing puts it on the bar: `NotificationHistoryPanel.qml` instantiates it above
-- `SystemInfoWidget`, and `modules/bar/panels/notification_history.lua` is the only caller.
--
-- Thirteen cards read the same four parallel arrays, so the body is one `computed` over the
-- forecast returning nodes, as `modules/bar/panels/minimal_calendar.lua` builds its month. Per-field
-- signals would resolve that table thirteen times a pass to say the same thing. It rebuilds about
-- eighty nodes a pass while the sidebar is open, and ADR-0124's freeze takes that to zero when shut.
--
-- The mirror animates a clipped `Layout.preferredHeight` open; `visible` is this config's shape for
-- expansion, and an invisible node takes no size or spacing gap.
local theme = require("config.theme")
local icons = require("config.icons")
local weather = require("lib.weather")
local cell = require("components.cell")
local icon_button = require("components.icon_button")
local panel_card = require("components.panel_card")

-- `GridLayout { columns: 4 }`.
local COLUMNS = 4
-- `past_days=1` puts yesterday first, so today is the second entry rather than the first.
local YESTERDAY, TODAY, TOMORROW = 1, 2, 3

-- `Math.round`, which is half-up rather than `%.0f`'s half-to-even.
local function degrees(value)
    return string.format("%d°", math.floor((value or 0) + 0.5))
end

local function has_data(daily)
    return type(daily) == "table" and type(daily.time) == "table" and #daily.time > 0
end

---@param daily table
---@param index integer
---@param opts { label?: string, today?: boolean, height?: integer }
local function day_card(daily, index, opts)
    local code = math.floor((daily.weathercode or {})[index] or -1)
    -- `showLabel: !root.expanded`: the three named days lose their names once the grid is open,
    -- because then every card is a weekday and "Today" beside "Wed" reads as two scales at once.
    local heading = opts.label or weather.weekday((daily.time or {})[index])
    local centred = { width = "Fill", align = "Center" }
    return panel_card({
        cell({ { text = heading, bold = true } }, opts.today and theme.FG or theme.DIM, theme.font.sm, centred),
        cell(weather.info(code).icon, theme.FG, theme.font.xl, centred),
        cell({ { text = degrees((daily.temperature_2m_max or {})[index]), bold = true } }, theme.FG, theme.font.lg,
            centred),
        cell(degrees((daily.temperature_2m_min or {})[index]), theme.DIM, theme.font.sm, centred),
    }, {
        width = "Fill",
        height = opts.height,
        -- `tone: isToday ? "active" : "standard"`.
        background = opts.today and theme.ACCENT_SUBTLE or theme.GLASS_CONTENT,
        radius = theme.radius.lg,
        border_width = theme.border_width,
        border_color = opts.today and theme.ACCENT or theme.GLASS_BORDER,
        spacing = theme.spacing.xs,
        padding = {
            top = theme.spacing.sm,
            right = theme.spacing.xs,
            bottom = theme.spacing.sm,
            left = theme.spacing.xs,
        },
    })
end

---@param id string Names this instance's `expanded` state and its hover slots.
return function(id)
    local expanded = state("weather_expanded_" .. id, false)

    local body = computed({ weather.daily, expanded }, function(daily, open)
        if not has_data(daily) then
            return {}
        end
        local rows = { row {
            width = "Fill",
            spacing = theme.spacing.sm,
            children = {
                -- `opacity: Theme.opacitySolid` and `opacityStrong` set yesterday and tomorrow
                -- behind today; a named day keeps its name only while the grid is shut.
                day_card(daily, YESTERDAY, { label = not open and "Yesterday" or nil }),
                day_card(daily, TODAY, { label = not open and "Today" or nil, today = true }),
                day_card(daily, TOMORROW, { label = not open and "Tomorrow" or nil }),
            },
        } }
        if not open then
            return rows
        end
        -- `model: forecast.time.length - 3`, four to a row, with the last row short rather than
        -- stretched: a lone Thursday card three columns wide is not a grid.
        local days = #daily.time
        for first = TOMORROW + 1, days, COLUMNS do
            local children = {}
            for index = first, math.min(first + COLUMNS - 1, days) do
                children[#children + 1] = day_card(daily, index, { height = theme.item_height * 3 })
            end
            for _ = #children + 1, COLUMNS do
                children[#children + 1] = rect { width = "Fill" }
            end
            rows[#rows + 1] = row { width = "Fill", spacing = theme.spacing.sm, children = children }
        end
        return rows
    end)

    local ready = weather.daily:map(has_data)
    local blank = ready:map(function(yes) return not yes end)

    -- `OButton` spanning the row: its label toggles the grid and its right end carries the age of
    -- the reading.
    local toggle = button {
        width = "Fill",
        height = theme.item_height,
        radius = theme.radius.md,
        visible = ready,
        hover = hover("weather-toggle-" .. id),
        background = theme.GLASS_CONTENT,
        border_width = theme.border_width,
        border_color = theme.GLASS_BORDER,
        on_click = function(_, mouse_button)
            if mouse_button == "left" then
                expanded:set(not expanded:get())
            end
        end,
        children = { row {
            width = "Fill",
            height = "Fill",
            align_v = "Center",
            spacing = theme.spacing.sm,
            padding = { left = theme.item_radius, right = theme.item_radius },
            children = {
                cell(expanded:map(function(open)
                    return { { text = open and "Show Less" or "10 Day Forecast", bold = true } }
                end), theme.FG, theme.font.sm, { align_v = "Center" }),
                cell(computed({ weather.updated_at, obelisk.system }, function(at, system)
                    return "Updated " .. weather.time_ago(at, system and system.time)
                end), theme.DIM, theme.font.xs, { width = "Fill", align = "End", align_v = "Center" }),
            },
        } },
    }

    return column {
        width = "Fill",
        spacing = theme.spacing.sm,
        children = {
            row {
                width = "Fill",
                spacing = theme.spacing.sm,
                align_v = "Center",
                children = {
                    toggle,
                    -- The mirror's filling `Item`: holds the right edge while the toggle is hidden.
                    rect { width = "Fill", visible = blank },
                    -- No `tooltipText`: a tooltip is its own popup surface here.
                    icon_button(icons.refresh, weather.refresh, { slot = "weather-refresh-" .. id }),
                },
            },
            column { width = "Fill", spacing = theme.spacing.sm, children = body },
            -- The mirror's own two strings, and its own bare centred text rather than a card.
            -- Its Retry button is absent: the refresh above is the same click, always there.
            cell(weather.failed:map(function(bad)
                return bad and "Weather Unavailable" or "Loading Forecast..."
            end), theme.DIM, theme.font.sm, { width = "Fill", align = "Center", visible = blank }),
        },
    }
end
