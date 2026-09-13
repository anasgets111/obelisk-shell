-- Mirrors `IdleSettingsPanel.qml`: an `OModal`, a full-screen layer over a scrim with one centred
-- glass card, like `modules/global/launcher.lua` and `wallpaper_picker.lua`.
--
-- Not a bar panel. Its three-action AC/battery matrix needs both profiles visible; the panel host's
-- 340px card cannot fit it.
--
-- Dropped: action-order combo (`lib/store.lua` makes timeout order canonical), "respect inhibitors"
-- (foreign inhibitors are unconditional, ADR-0139), and the unrelated input-overlay section.
--
-- The mirror's static `FlowSummary`, "Lock · 5 min → Display · 30 sec → Suspend · Off", shows
-- configuration, not progress. One equal-width chamber per runnable stage fills over its own window
-- and shows how close the screen is to going dark.
--
-- Chambers are not delay-proportional: with a 30-second stage followed by 15 minutes, the first
-- would be 3% of the card and its glyph would not fit. Each prints its delay, so proportions are
-- readable but not to scale.
--
-- A chamber prints its stage delay, not the running total. The first pass showed "1m" in the row
-- and "1m 30s" beside it. The masthead keeps the total by counting down to the next stage.
--
-- Replace the timeline, rather than dim it, when a hold blocks countdown or no action is scheduled;
-- a bar that can never fill is worse than the reason.
local theme = require("config.theme")
local icons = require("config.icons")
local cell = require("components.cell")
local glyph = require("components.glyph")
local toggle = require("components.toggle")
local panel_card = require("components.panel_card")
local panel_header = require("components.panel_header")
local panel_row = require("components.panel_row")
local panel_action_icon = require("components.panel_action_icon")
local ui_state = require("lib.ui_state")
local modal = require("components.modal")
local idle = require("lib.idle")
local store = require("lib.store")

local settings = store.idle:map(idle.read)

-- Whether UPower reports a battery. `present` is false on desktops, so no battery column;
-- the mirror gates its second column the same way.
local has_battery = obelisk.battery:map(function(b)
    return b ~= nil and b.present
end)


local subtitle = computed(
    { store.idle, idle.active_profile, idle.elapsed, idle.reasons, idle.arming },
    function(stored, profile, elapsed, reasons, arming)
        if #reasons > 0 then
            return "held awake · " .. table.concat(reasons, ", ")
        end
        local resolved = idle.read(stored)
        if not resolved.enabled then
            return "automatic actions are paused"
        end
        local plan = idle.plan(resolved, profile)
        if plan.total == 0 then
            return "no actions enabled on this profile"
        end
        local where = profile == "battery" and "on battery" or "on ac power"
        if elapsed == 0 then
            local first = plan.list[1]
            return string.format("%s · %s after %s", where, first.title, idle.format(first.at))
        end
        -- Count down the armed stage's delay, not the running total. Its clock starts when the
        -- prior stage finishes, and only this answers "how long have I got".
        for _, entry in ipairs(plan.list) do
            if entry.key == arming.key then
                return string.format(
                    "idle %s · %s in %s",
                    idle.clock(elapsed),
                    entry.title,
                    idle.clock(math.max(0, entry.delay - arming.elapsed))
                )
            end
        end
        return string.format("idle %s · every stage has run", idle.clock(elapsed))
    end
)

-- Rule under the masthead, as in `IdleSettingsPanel.qml`: header is state, below is settings.
local header_rule = rect { width = "Fill", height = theme.border_width, background = theme.BORDER_SUBTLE }

-- Between section rows, inset past the leading glyph; `ActionSettingRow` uses the same inset.
local row_rule = rect {
    width = "Fill",
    height = theme.border_width,
    margin = { left = theme.icon.md + theme.spacing.sm * 2 },
    -- `BORDER`, not `BORDER_SUBTLE`: subtle is 35% of a near-card colour, and the first pass's 1px
    -- line was effectively invisible.
    background = theme.BORDER,
}

local header = panel_header {
    title = "idle & power",
    subtitle = subtitle,
    -- Reuse the bar circle's glyph so opener and modal read as one control.
    icon = idle.manual:map(function(manual)
        return manual and icons.awake or icons.idle
    end),
    -- `OModal` uses `size: "xxl"` over `controlHeightXl`; a bar panel's half-sized default looked
    -- like a window at 820px wide.
    title_size = theme.font.xxl,
    plate = theme.control.xl,
    -- The live count is not put under a 28px title at 10px.
    subtitle_size = theme.font.md,
    subtitle_color = theme.DIM,
    active = computed({ settings, idle.inhibited }, function(resolved, held)
        return resolved.enabled and not held
    end),
    -- The master switch sits beside the flow it controls, as `FlowSummary` does; no label needed.
    on_close = function()
        ui_state.close_modal("idle_settings")
    end,
}

-- ## Timeline

local plan_now = idle.schedule

local counting_down = computed({ settings, plan_now, idle.inhibited }, function(resolved, plan, held)
    return resolved.enabled and plan.total > 0 and not held
end)

-- Chamber state comes from `idle.arming`: earlier stages are full, later stages are empty, and the
-- stage fills over its delay. The first version used the running total and showed an unstarted
-- stage as partly done.
local function chamber_progress(entry)
    return computed({ idle.arming, plan_now }, function(arming, plan)
        local position, armed_position
        for index, item in ipairs(plan.list) do
            if item.key == entry.key then
                position = index
            end
            if item.key == arming.key then
                armed_position = index
            end
        end
        if position == nil or armed_position == nil then
            return 0
        end
        if position < armed_position then
            return 1
        end
        if position > armed_position then
            return 0
        end
        return math.max(0, math.min(1, arming.elapsed / math.max(1, entry.delay)))
    end)
end

local function chamber(entry)
    local progress = chamber_progress(entry)
    -- Signals resolve before `width` is parsed (ADR-0044), as in `components/meter.lua`.
    local fill = progress:map(function(fraction)
        return string.format("%d%%", math.floor(fraction * 100 + 0.5))
    end)
    local ink = progress:map(function(fraction)
        return fraction > 0 and theme.FG or theme.DIM
    end)
    return rect {
        width = "Fill",
        height = "Fill",
        children = {
            rect { width = fill, height = "Fill", background = theme.ACCENT_MEDIUM },
            row {
                width = "Fill",
                height = "Fill",
                align_h = "Center",
                align_v = "Center",
                spacing = theme.spacing.xs,
                children = {
                    glyph(entry.icon, ink, theme.icon.sm, { align_v = "Center" }),
                    cell(idle.format(entry.delay), ink, theme.font.xs, { align_v = "Center" }),
                },
            },
        },
    }
end

-- Wrap the `list` in a `row`: `list` is `NodeBase`, not `BoxBase` (`lua-meta/nodes.lua`, `nodes.rs`
-- `BOX_KINDS`), so it places but does not paint. Put `background`, `radius`, and `clip` on the
-- parent. `just types` misses unknown table keys; the engine rejects the re-resolve at runtime.
local timeline = row {
    width = "Fill",
    height = theme.idle_track_height,
    radius = theme.radius.sm,
    -- Square chambers butt together; the parent rounds the outer ends. `clip` is needed because
    -- children do not inherit the parent's arc.
    clip = "Rounded",
    background = theme.GLASS_CONTENT,
    visible = counting_down,
    children = {
        list {
            width = "Fill",
            height = "Fill",
            direction = "Horizontal",
            source = plan_now:map(function(plan)
                return plan.list
            end),
            key = function(entry)
                return entry.key
            end,
            itemfn = chamber,
        },
    },
}

local function banner(codepoint, content, tint, ground, visible)
    return row {
        width = "Fill",
        height = theme.idle_track_height,
        align_v = "Center",
        radius = theme.radius.sm,
        background = ground,
        spacing = theme.spacing.sm,
        padding = { left = theme.spacing.md, right = theme.spacing.md },
        visible = visible,
        children = {
            glyph(codepoint, tint, theme.icon.sm, { align_v = "Center" }),
            cell(content, tint, theme.font.xs, { width = "Fill", align_v = "Center" }),
        },
    }
end

local held_banner = banner(
    icons.awake,
    computed({ idle.reasons, idle.inhibited }, idle.held_text),
    theme.ACCENT,
    theme.ACCENT_SUBTLE,
    idle.inhibited
)

local paused_banner = banner(
    icons.idle,
    settings:map(function(resolved)
        return resolved.enabled and "nothing is scheduled on this profile" or "automatic actions are off"
    end),
    theme.DIM,
    theme.GLASS_CONTENT,
    computed({ counting_down, idle.inhibited }, function(counting, held)
        return not counting and not held
    end)
)

-- ## Matrix
--
-- One row per stage in stored `order`; chevrons move the stage and its row. Use a `list`, since
-- declared children cannot be reordered.

local function setting(profile, key, suffix)
    return settings:map(function(resolved)
        return resolved[profile][key .. suffix]
    end)
end

-- Click cycles forward through seven options; right-click cycles back. The config lacks
-- `OComboBox`, and this interaction is enough without building one.
local function duration_button(profile, stage)
    local slot = "idle-sec-" .. profile .. "-" .. stage.key
    local hovered = hover(slot)
    return button {
        width = "Fill",
        height = theme.control.sm,
        align_v = "Center",
        radius = theme.radius.sm,
        hover = hovered,
        background = hovered:map(function(hot)
            return hot and theme.GLASS_HOVER or theme.GLASS_CONTENT
        end),
        on_click = function(_, mouse_button)
            if mouse_button == "middle" then
                return
            end
            local current = idle.read(store.idle:get())[profile][stage.key .. "_sec"]
            idle.write(profile, stage.key .. "_sec", idle.cycle(stage, current, mouse_button == "right" and -1 or 1))
        end,
        -- A chevron makes the numbered plate visibly interactive; the mirror's `OComboBox` does the
        -- same. It advances the value instead of opening a list, but the affordance is clear.
        children = {
            row {
                width = "Fill",
                height = "Fill",
                align_v = "Center",
                spacing = theme.spacing.xs,
                padding = { left = theme.spacing.sm, right = theme.spacing.xs },
                children = {
                    cell(setting(profile, stage.key, "_sec"):map(idle.format), theme.FG, theme.font.xs, {
                        width = "Fill",
                        align_v = "Center",
                    }),
                    glyph(icons.chevron_down, theme.DIM, theme.icon.xs, { align_v = "Center" }),
                },
            },
        },
    }
end

local function profile_control(profile, stage)
    local on = setting(profile, stage.key, "_on")
    return row {
        width = theme.idle_profile_column,
        align_v = "Center",
        spacing = theme.spacing.sm,
        visible = profile == "battery" and has_battery or nil,
        children = {
            duration_button(profile, stage),
            toggle(on, function(enabled)
                return enabled
            end, function(enabled)
                idle.write(profile, stage.key .. "_on", enabled)
            end),
        },
    }
end

-- `PanelHeader`'s `titleBold`, which every row here binds to its own on-state (`:316`, `:563`).
-- `components/panel_row.lua` bolds a title too, but only for its static `selected` boolean; these
-- rows follow a signal, so the weight has to be decided inside the map.
---@param on Signal<boolean>
---@param title string
local function bold_when(on, title)
    return on:map(function(enabled)
        return enabled and { { text = title, bold = true } } or title
    end)
end

-- `ProfileHeading` marks the active column so desk-side configuration shows the running profile.
local function column_heading(profile, label)
    return cell(
        idle.active_profile:map(function(active)
            local text = active == profile and label .. " · live" or label
            return { { text = text, bold = true } }
        end),
        idle.active_profile:map(function(active)
            return active == profile and theme.ACCENT or theme.DIM
        end),
        theme.font.xs,
        { width = theme.idle_profile_column, align = "Center" }
    )
end

local matrix_heading = row {
    width = "Fill",
    align_v = "Center",
    spacing = theme.spacing.sm,
    padding = { left = theme.spacing.sm, right = theme.spacing.sm },
    children = {
        cell({ { text = "action · in order", bold = true } }, theme.DIM, theme.font.xs, { width = "Fill" }),
        column_heading("ac", "ac power"),
        row {
            width = theme.idle_profile_column,
            visible = has_battery,
            children = { column_heading("battery", "battery") },
        },
    },
}

-- Chevrons move a stage through `order`. Hide them at the ends instead of showing no-op disabled
-- controls; `idle.move` already treats out-of-range moves as no-ops.
local function reorder(item)
    return column {
        align_v = "Center",
        children = {
            panel_action_icon(icons.chevron_up, function()
                idle.move(item.key, -1)
            end, { slot = "idle-up-" .. item.key, visible = not item.first }),
            panel_action_icon(icons.chevron_down, function()
                idle.move(item.key, 1)
            end, { slot = "idle-down-" .. item.key, visible = not item.last }),
        },
    }
end

local function stage_row(item)
    local stage = item.stage
    -- Accent while either profile enables this stage, `ActionSettingRow`'s `anyEnabled`; dim in
    -- both columns means the stage never runs.
    local any = settings:map(function(resolved)
        for _, profile in ipairs({ "ac", "battery" }) do
            if resolved[profile][item.key .. "_on"] and resolved[profile][item.key .. "_sec"] > 0 then
                return true
            end
        end
        return false
    end)
    local ink = any:map(function(enabled)
        return enabled and theme.ACCENT or theme.DIM
    end)
    local body = panel_row {
        title = bold_when(any, stage.title),
        -- Use the stage's description, not "after <the row above>": the prior row may be off in one
        -- profile. The section states the rule, row order shows it, and the timeline gives the
        -- total.
        subtitle = stage.detail,
        height = theme.idle_row_height,
        leading = row {
            align_v = "Center",
            spacing = theme.spacing.xs,
            children = { reorder(item), glyph(stage.icon, ink, theme.icon.md, { align_v = "Center" }) },
        },
        trailing = row {
            spacing = theme.spacing.sm,
            align_v = "Center",
            children = { profile_control("ac", stage), profile_control("battery", stage) },
        },
    }
    if item.last then
        return body
    end
    return column { width = "Fill", children = { body, row_rule } }
end

-- Row descriptors carry the stage, order position, and predecessor name. They rebuild only when
-- stored settings change; a reorder rebuilds three rows, a tick none.
local stage_source = settings:map(function(resolved)
    local items = {}
    for index, key in ipairs(resolved.order) do
        local stage = idle.stage(key)
        if stage then
            items[#items + 1] = {
                key = key,
                stage = stage,
                first = index == 1,
                last = index == #resolved.order,
            }
        end
    end
    return items
end)

local stage_list = list {
    width = "Fill",
    source = stage_source,
    key = function(item)
        return item.key
    end,
    itemfn = stage_row,
}

-- ## Behavior
--
-- Both are non-timeout reasons the session stays up, so they share a card instead of the mirror's
-- separate cards. Neither is per-profile: media applies on AC and battery.
local behaviour_rows = {
    panel_row {
        icon = icons.play,
        title = bold_when(settings:map(function(resolved)
            return resolved.video_auto_inhibit
        end), "keep awake for media"),
        subtitle = "video, camera, microphone, screen capture",
        height = theme.idle_row_height,
        icon_color = settings:map(function(resolved)
            return resolved.video_auto_inhibit and theme.ACCENT or theme.DIM
        end),
        trailing = toggle(settings, function(resolved)
            return resolved.video_auto_inhibit
        end, function(on)
            idle.write(nil, "video_auto_inhibit", on)
        end),
    },
    panel_row {
        icon = icons.awake,
        title = bold_when(idle.manual, "keep awake now"),
        subtitle = "the same hold the bar circle takes",
        height = theme.idle_row_height,
        icon_color = idle.manual:map(function(manual)
            return manual and theme.ACCENT or theme.DIM
        end),
        trailing = toggle(idle.manual, function(manual)
            return manual
        end, idle.set_manual),
    },
}

-- ## Assembly
--
-- Three cards, matching `IdleSettingsPanel.qml`. The first flat column of rows under two grey words
-- did not show which settings belonged together; titled cards do.

-- One compact line, not `panel_header`: the masthead already has a plate and 16px bold title.
local flow_strip = row {
    width = "Fill",
    align_v = "Center",
    spacing = theme.spacing.sm,
    children = {
        glyph(icons.play, computed({ settings, idle.inhibited }, function(resolved, held)
            return (resolved.enabled and not held) and theme.ACCENT or theme.DIM
        end), theme.icon.sm, { align_v = "Center" }),
        cell(
            computed({ settings, idle.active_profile }, function(resolved, profile)
                if not resolved.enabled then
                    return "automation paused"
                end
                return "current flow · " .. (profile == "battery" and "battery" or "ac power")
            end),
            theme.FG,
            theme.font.sm,
            { width = "Fill", align_v = "Center" }
        ),
        toggle(settings, function(resolved)
            return resolved.enabled
        end, function(on)
            idle.write(nil, "enabled", on)
        end),
    },
}

local flow_card = panel_card({ flow_strip, timeline, held_banner, paused_banner }, {
    width = "Fill",
    spacing = theme.spacing.md,
    padding = {
        top = theme.spacing.md,
        right = theme.spacing.lg,
        bottom = theme.spacing.md,
        left = theme.spacing.lg,
    },
    background = theme.GLASS_CONTENT,
    border_width = theme.border_width,
    border_color = theme.GLASS_BORDER,
})

-- `SettingsSection` is glyph-on-plate, title, description, rows. `panel_header` already has that
-- shape, so each section is a header plus card children.
local function section(codepoint, title, description, children)
    local nodes = { panel_header { title = title, subtitle = description, icon = codepoint, title_size = theme.font.xl } }
    for _, node in ipairs(children) do
        nodes[#nodes + 1] = node
    end
    return panel_card(nodes, {
        width = "Fill",
        spacing = theme.spacing.sm,
        padding = {
            top = theme.spacing.md,
            right = theme.spacing.lg,
            bottom = theme.spacing.md,
            left = theme.spacing.lg,
        },
        background = theme.GLASS_CONTENT,
        border_width = theme.border_width,
        border_color = theme.GLASS_BORDER,
    })
end

local behaviour_children = { behaviour_rows[1], row_rule, behaviour_rows[2] }

local card_children = {
    header,
    header_rule,
    flow_card,
    section(
        icons.sleep,
        "automation",
        "each stage waits for the one above it",
        { matrix_heading, stage_list }
    ),
    section(icons.settings, "behaviour", "what may keep the session awake", behaviour_children),
}

return modal({
    kind = "idle_settings",
    card = panel_card(card_children, {
        width = theme.idle_modal_width,
        align_h = "Center",
        align_v = "Center",
        spacing = theme.spacing.lg,
        padding = {
            top = theme.spacing.xl,
            right = theme.spacing.xl,
            bottom = theme.spacing.xl,
            left = theme.spacing.xl,
        },
        radius = theme.radius.lg,
        background = theme.GLASS,
        -- The card alone, not the scrim behind it: the scrim is drawn under this in the same
        -- surface, so what reaches the eye here is the blurred desktop seen through both.
        blur = true,
        border_width = theme.border_width,
        border_color = theme.BORDER,
    }),
})
