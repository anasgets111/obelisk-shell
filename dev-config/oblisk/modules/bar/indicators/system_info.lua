-- Mirrors `SystemInfoWidget.qml`: a full-width "System" button that carries the readouts on its own
-- line while collapsed, and opens into metric tiles.
--
-- Despite living under `Indicators/`, the mirror never puts this on the bar -- `SystemInfoWidget` is
-- instantiated once, at the top of `NotificationHistoryPanel.qml`, under the weather. This file
-- keeps the mirror's path and is used from `modules/bar/panels/notification_history.lua` for the
-- same reason.
--
-- ## What is missing, and why it is not a bug
--
-- The mirror reads a `SystemInfoService` that shells out for GPU load, disk usage, uptime, and boot
-- time. § 2.12 is `cpu_percent`, `ram_percent`, `swap_percent`, `temp_cores` and `temp_gpu` -- CPU,
-- memory and temperatures, as the capability table names it. So the GPU usage tile, the per-disk
-- rows, and the uptime/boot footer have no data behind them and are absent rather than faked. The
-- shape they left is spent on what § 2.12 does have: swap under memory, and the GPU's temperature
-- where the GPU tile stood.
--
-- ## A factory, not a node
--
-- Each instance owns its `expanded` flag, so the same widget in two places does not open in both.
-- The mirror gets this from QML instantiation; here the caller names the instance.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local glyph = require("components.glyph")
local meter = require("components.meter")
local panel_card = require("components.panel_card")

-- `sysinfo`'s three pollers start dormant until configured (ADR-0035); without this, the readouts
-- stay at pre-first-sample `0%`. Configure here, not in `shell.lua`, because this is the only
-- module reading them.
--
-- CPU every 2s and RAM every 5s, about as slow as a readout can tick before it reads as frozen,
-- matching the mirror's cadence. Temperatures ride with RAM: they are a tile's second line, not a
-- number anyone watches move, and the hwmon pass is one read of both `temp_cores` and `temp_gpu`.
--
-- The mirror instead ref-counts `SystemInfoService.refCount` so the pollers only run while the
-- panel is open. § 2.12 has no such control -- `configure` sets an interval, and zero stops a
-- poller for everyone -- so the choice is polling always or polling never. Two file reads every
-- couple of seconds is the cheaper mistake.
oblisk.sysinfo:invoke("configure", { cpu_interval = 2, ram_interval = 5, temp_interval = 5 })

-- `SystemInfoWidget.qml`'s `statusColor(progress, fallback)`: red past 90%, peach past 75%, and the
-- readout's own colour below that. The mirror's `progress` is a fraction; § 3.2 pushes whole
-- percents, so the thresholds are 90 and 75 rather than 0.9 and 0.75.
local function status_color(percent, fallback)
    if percent >= 90 then
        return theme.RED
    elseif percent >= 75 then
        return theme.PEACH
    end
    return fallback
end

local function percent_of(state_value, field)
    return (state_value and state_value[field]) or 0
end

-- The tinted number, shared by the collapsed summary and each tile's value.
local function tint(field, fallback)
    return oblisk.sysinfo:map(function(s)
        return status_color(percent_of(s, field), fallback)
    end)
end

-- `temp_cores` is one entry per hwmon sensor, not per core, and the mirror's single `cpuTemp` is
-- the package figure. The hottest sensor is the honest stand-in: a mean over sensors that may
-- include a chipset probe reads cooler than any core actually is.
local function hottest_core(s)
    local hottest = 0
    for _, celsius in ipairs((s and s.temp_cores) or {}) do
        if celsius > hottest then
            hottest = celsius
        end
    end
    return hottest
end

-- `PanelCard`, whose standard tone is `glassContentColor` behind a `glassBorderColor` hairline at
-- `radiusLg` -- not `panel_card`'s own default, which is the opaque ground a whole panel body sits
-- on.
local function tile(children, opts)
    opts = opts or {}
    return panel_card(children, {
        width = "Fill",
        visible = opts.visible,
        background = theme.GLASS_CONTENT,
        radius = theme.radius.lg,
        border_width = theme.border_width,
        border_color = theme.GLASS_BORDER,
        spacing = theme.spacing.xs,
        padding = {
            top = theme.spacing.sm,
            right = theme.spacing.sm,
            bottom = theme.spacing.sm,
            left = theme.spacing.sm,
        },
    })
end

-- `MetricTile`: glyph, label, percentage on one line, a progress track under it, and one dim line
-- of detail.
local function metric_tile(codepoint, label, field, accent, detail)
    return tile {
        row {
            width = "Fill",
            align_v = "Center",
            spacing = theme.spacing.xs,
            children = {
                glyph(codepoint, accent, theme.icon.sm, { align_v = "Center" }),
                cell({ { text = label, bold = true } }, theme.FG, theme.font.sm, {
                    width = "Fill",
                    align_v = "Center",
                }),
                cell(util.label(oblisk.sysinfo, function(s)
                    return string.format("%d%%", percent_of(s, field))
                end):map(function(shown)
                    return { { text = shown, bold = true } }
                end), tint(field, accent), theme.font.md, { align_v = "Center" }),
            },
        },
        -- `ProgressTrack`: the fill takes `statusColor` too, so a track turning red is the same
        -- warning as its number turning red.
        meter(oblisk.sysinfo, function(s)
            return percent_of(s, field)
        end, tint(field, accent), "Fill", theme.spacing.xs),
        cell(detail, theme.DIM, theme.font.xs, { width = "Fill" }),
    }
end

-- One collapsed readout: `CPU 12%`, bold and tinted, as the mirror's `summaryRow` draws them.
local function summary_readout(label, field, accent)
    return cell(util.label(oblisk.sysinfo, function(s)
        return string.format("%s %d%%", label, percent_of(s, field))
    end):map(function(shown)
        return { { text = shown, bold = true } }
    end), tint(field, accent), theme.font.xs, { align_v = "Center" })
end

---@param id string Names this instance's `expanded` state and its hover slot.
return function(id)
    local expanded = state("sysinfo_expanded_" .. id, false)
    local hovered = hover("sysinfo-" .. id)

    -- `bgColor: expanded ? activeColor : glassContentColor`, and `textColor` is that ground's
    -- contrast -- so the title and chevron go dark against the accent rather than staying white on
    -- it.
    local ground = expanded:map(function(open)
        return open and theme.ACCENT or theme.GLASS_CONTENT
    end)
    local ink = expanded:map(function(open)
        return open and theme.text_contrast(theme.ACCENT) or theme.FG
    end)

    local head = button {
        width = "Fill",
        height = theme.item_height,
        radius = theme.radius.md,
        hover = hovered,
        background = ground,
        border_width = theme.border_width,
        border_color = theme.GLASS_BORDER,
        animate = { background = theme.animation_ms },
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
            -- `anchors.leftMargin: Theme.itemRadius` on both ends: at half the button's height the
            -- radius is the whole end of it, and a word starting at zero runs under the curve.
            padding = { left = theme.item_radius, right = theme.item_radius },
            children = {
                cell({ { text = "System", bold = true } }, ink, theme.font.sm, { align_v = "Center" }),
                -- The summary is the filling child, right-aligned inside it, so the chevron stays
                -- at the far edge whether or not the readouts are showing.
                row {
                    width = "Fill",
                    align_h = "End",
                    align_v = "Center",
                    spacing = theme.spacing.sm,
                    visible = expanded:map(function(open)
                        return not open
                    end),
                    children = {
                        summary_readout("CPU", "cpu_percent", theme.ACCENT),
                        summary_readout("RAM", "ram_percent", theme.GREEN),
                        summary_readout("SWAP", "swap_percent", theme.PEACH),
                    },
                },
                glyph(expanded:map(function(open)
                    return open and icons.chevron_down or icons.chevron_right
                end), ink, theme.icon.sm, { align_v = "Center" }),
            },
        } },
    }

    -- The mirror animates a clipped `Layout.preferredHeight` open; `visible` is the shape this
    -- config already uses for expansion (`components/notification_card.lua`), and an invisible node
    -- takes no size or spacing gap, so the card retracts cleanly without a clip.
    local details = column {
        width = "Fill",
        spacing = theme.spacing.sm,
        visible = expanded,
        children = {
            row {
                width = "Fill",
                spacing = theme.spacing.sm,
                children = {
                    metric_tile(icons.cpu, "CPU", "cpu_percent", theme.ACCENT, util.label(oblisk.sysinfo, function(s)
                        local celsius = hottest_core(s)
                        -- The mirror's own wording for a machine that exposes no sensor.
                        return celsius > 0 and string.format("%d°C", celsius) or "No temperature"
                    end)),
                    -- The mirror's second line here is `used / total`; § 2.12 pushes percentages
                    -- only. Swap is the memory fact it does push, and it has no tile of its own.
                    metric_tile(icons.ram, "Memory", "ram_percent", theme.GREEN, util.label(oblisk.sysinfo, function(s)
                        local swap = percent_of(s, "swap_percent")
                        return swap > 0 and string.format("Swap %d%%", swap) or "No swap in use"
                    end)),
                },
            },
            -- `GpuTile` spans both columns. Only its temperature line survives § 2.12, and that
            -- line is already conditional in the mirror (`visible: gpuTemp > 0`); `temp_gpu` is
            -- `-1` with no sensor, so one test covers both.
            tile({ row {
                width = "Fill",
                align_v = "Center",
                spacing = theme.spacing.xs,
                children = {
                    glyph(icons.gpu, theme.PEACH, theme.icon.sm, { align_v = "Center" }),
                    cell({ { text = "GPU", bold = true } }, theme.FG, theme.font.sm, {
                        width = "Fill",
                        align_v = "Center",
                    }),
                    -- `gpuTemp >= 85 ? critical : >= 70 ? warning : textInactiveColor`.
                    cell(util.label(oblisk.sysinfo, function(s)
                        return string.format("%d°C", (s and s.temp_gpu) or 0)
                    end), oblisk.sysinfo:map(function(s)
                        local celsius = (s and s.temp_gpu) or 0
                        if celsius >= 85 then
                            return theme.RED
                        elseif celsius >= 70 then
                            return theme.PEACH
                        end
                        return theme.DIM
                    end), theme.font.sm, { align_v = "Center" }),
                },
            } }, {
                visible = util.shown_when(oblisk.sysinfo, function(s)
                    return (s.temp_gpu or -1) > 0
                end),
            }),
        },
    }

    return column {
        width = "Fill",
        spacing = theme.spacing.sm,
        children = { head, details },
    }
end
