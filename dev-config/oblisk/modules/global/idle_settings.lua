-- Mirrors Bar/Panels/IdleSettingsPanel.qml, which is an `OModal`, and so is this: the same surface
-- `modules/global/launcher.lua` and `modules/global/wallpaper_picker.lua` are, a full-screen layer
-- panel over a scrim with one glass card centred on it.
--
-- Not a bar panel, and that was the first attempt. The panel host's card is 340px and this is a
-- matrix -- three actions down, AC and battery across -- so fitting it there cost the one thing
-- worth keeping, which is seeing both profiles at once.
--
-- The mirror's own layout, kept: the action rows, a timeout and a switch per profile per action,
-- the live profile marked in its column heading, the master switch in the header, and the two
-- behaviour toggles.
--
-- Dropped: the action-order combo (`lib/store.lua` says why the timeouts are the order), the
-- "respect inhibitors" switch (the Supervisor honours a foreign inhibitor unconditionally, ADR-0139,
-- and a switch that turned that off would be a switch that ignores `systemd-inhibit`), and the
-- input-overlay section, which is a different feature that was only in this modal for want of
-- anywhere else to put it.
--
-- ## The timeline
--
-- Added, and it is the one thing here the mirror has no version of. Its `FlowSummary` is a static
-- line -- "Lock · 5 min → Display · 30 sec → Suspend · Off" -- which tells you what you configured
-- and never what is happening. This is the same flow as a track that moves: one chamber per stage
-- that will actually run, soonest first, each filling over its own window. So a glance answers "how
-- close is the screen to going dark", which is the question a settings form cannot answer.
--
-- Chambers are equal width rather than proportional to their delays. Proportional is the honest
-- picture right up until one stage is 30 seconds and the next is 15 minutes, at which point the
-- first chamber is 3% of the card and its glyph does not fit in it. Every chamber prints its delay,
-- so the proportions are there to read; they are just not drawn to scale.
--
-- A chamber prints the stage's own delay, the same number the matrix row edits, not the running
-- total it works out to. The first pass printed the total and it read as a bug: the row said "1m"
-- and the chamber beside it said "1m 30s" for the same stage. The total is not lost -- the masthead
-- counts down to the next stage in real time, which is the more useful form of it anyway.
--
-- It is replaced, not dimmed, in the two states with no countdown to show -- something is holding
-- the session awake, or nothing is scheduled. A progress bar that can never fill is worse than a
-- sentence saying why.
local theme = require("config.theme")
local icons = require("config.icons")
local cell = require("components.cell")
local toggle = require("components.toggle")
local panel_card = require("components.panel_card")
local panel_header = require("components.panel_header")
local panel_row = require("components.panel_row")
local panel_action_icon = require("components.panel_action_icon")
local section_header = require("components.section_header")
local ui_state = require("lib.ui_state")
local idle = require("lib.idle")
local store = require("lib.store")

local open = ui_state.idle_settings_open
local settings = store.idle:map(idle.read)

-- Whether this machine has a battery to have a second profile for. `present` is UPower's own answer
-- and is `false` on a desktop (§ 2.2), where a battery column would be settings for hardware that
-- is not there. The mirror gates its own second column on the same fact.
local has_battery = oblisk.battery:map(function(b)
    return b ~= nil and b.present
end)

-- ## Header

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
        -- Counts down the armed stage's own delay, not a position in a running total: a stage's
        -- clock starts when the stage before it finished, so those are different numbers and only
        -- this one answers "how long have I got".
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

-- `IdleSettingsPanel.qml` rules off under its masthead, and the rule earns its line here for the
-- same reason: the header is state and everything under it is settings.
local header_rule = rect { width = "Fill", height = theme.border_width, background = theme.BORDER_SUBTLE }

-- Between two rows of a section, indented past the leading glyph so it separates the rows without
-- cutting the icon column off them. `ActionSettingRow` draws the same line at the same inset.
local row_rule = rect {
    width = "Fill",
    height = theme.border_width,
    margin = { left = theme.icon.md + theme.spacing.sm * 2 },
    -- `BORDER`, not `BORDER_SUBTLE`. Subtle is 35% of a surface colour that is already close to the
    -- card behind it, and a 1px line at that contrast is a line nobody sees -- which is what the
    -- first pass drew, and it may as well not have been there.
    background = theme.BORDER,
}

local header = panel_header {
    title = "idle & power",
    subtitle = subtitle,
    -- The bar circle's own glyph, so the modal and the thing that opened it read as one control.
    icon = idle.manual:map(function(manual)
        return manual and icons.awake or icons.idle
    end),
    -- `OModal`'s masthead is `size: "xxl"` over a `controlHeightXl` plate. A bar panel's default is
    -- half that, and at 820px across it reads as a panel that wandered into a window.
    title_size = theme.font.xxl,
    plate = theme.control.xl,
    -- The live count is the one thing in here the mirror has no version of, so it is not going
    -- under a 28px title at 10px. `theme.DIM` rather than `TEXT_OFF` for the same reason: a
    -- counter at 35% opacity is a counter nobody reads.
    subtitle_size = theme.font.md,
    subtitle_color = theme.DIM,
    active = computed({ settings, idle.inhibited }, function(resolved, held)
        return resolved.enabled and not held
    end),
    -- The master switch is not here. `FlowSummary` puts it against the flow it switches, which is
    -- the better place: a switch beside the thing it turns off needs no label saying what it does.
    on_close = function()
        open:set(false)
    end,
}

-- ## The timeline

local plan_now = computed({ settings, idle.active_profile }, function(resolved, profile)
    return idle.plan(resolved, profile)
end)

local counting_down = computed({ settings, plan_now, idle.inhibited }, function(resolved, plan, held)
    return resolved.enabled and plan.total > 0 and not held
end)

-- A chamber's own three states, off `idle.arming` alone: the armed stage fills over its delay,
-- every stage before it in the plan has already run and reads full, and every stage after it is
-- waiting its turn and reads empty. Reading it off the plan's running total instead is what the
-- first version did, and it drew a stage as part-done when its clock had not started.
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
    -- A signal resolves before `width` is parsed (ADR-0044), which is what `components/meter.lua`
    -- is built on too.
    local fill = progress:map(function(fraction)
        return string.format("%d%%", math.floor(fraction * 100 + 0.5))
    end)
    -- Lit once this stage's own clock is running or has run; dim while it is somebody else's turn.
    local ink = progress:map(function(fraction)
        return fraction > 0 and theme.FG or theme.TEXT_OFF
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
                    cell(entry.icon, ink, theme.icon.sm, { align_v = "Center" }),
                    cell(idle.format(entry.delay), ink, theme.font.xs, { align_v = "Center" }),
                },
            },
        },
    }
end

-- A `row` around the `list` rather than the list itself carrying the look. A `list` is `NodeBase`
-- and not `BoxBase` (`lua-meta/nodes.lua`, `nodes.rs`'s `BOX_KINDS`): it takes the properties that
-- place it and none of the ones that paint it, so `background`, `radius` and `clip` go on a parent.
-- Worth knowing before writing one, because `just types` does not catch it -- an unknown key in a
-- table literal is not a diagnostic -- and the engine rejects the whole re-resolve at runtime.
local timeline = row {
    width = "Fill",
    height = theme.idle_track_height,
    radius = theme.radius.sm,
    -- The chambers are square-cornered and butt together; this rounds the two outer ends, and it
    -- needs `clip` because a child does not inherit its parent's arc.
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

local function banner(glyph, content, tint, ground, visible)
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
            cell(glyph, tint, theme.icon.sm, { align_v = "Center" }),
            cell(content, tint, theme.font.xs, { width = "Fill", align_v = "Center" }),
        },
    }
end

local held_banner = banner(
    icons.awake,
    idle.reasons:map(function(reasons)
        return "held awake by " .. table.concat(reasons, ", ")
    end),
    theme.ACCENT,
    theme.ACCENT_SUBTLE,
    idle.inhibited
)

local paused_banner = banner(
    icons.idle,
    settings:map(function(resolved)
        return resolved.enabled and "nothing is scheduled on this profile" or "automatic actions are off"
    end),
    theme.TEXT_OFF,
    theme.GLASS_CONTENT,
    computed({ counting_down, idle.inhibited }, function(counting, held)
        return not counting and not held
    end)
)

-- ## The matrix
--
-- One row per stage, in `order`, so the rows are the sequence: the chevrons on the left move a stage
-- through it and the row moves with them. A `list` rather than three declared rows, because the
-- order is a stored value and declared children cannot be reordered.

local function setting(profile, key, suffix)
    return settings:map(function(resolved)
        return resolved[profile][key .. suffix]
    end)
end

-- Click cycles up the stage's option list, right-click cycles back. `OComboBox` is what the mirror
-- reaches for and this config has no such component; a value that changes when you press it is what
-- it would take to justify building one, and it turns out to be enough for a list of seven.
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
        -- A chevron, because a plate with a number on it is not visibly a control -- which is what
        -- the first version of this was, and the mirror's `OComboBox` has one for the same reason.
        -- It advances the value rather than dropping a list down, so it is a small lie about the
        -- mechanism and the truth about the affordance, and the affordance is the part that has to
        -- be legible from across the card.
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
                    cell(icons.chevron_down, theme.TEXT_OFF, theme.icon.xs, { align_v = "Center" }),
                },
            },
        },
    }
end

-- One profile's cell of the matrix: the delay and the switch that turns the stage on in it.
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

-- `ProfileHeading`: the column that is actually in force is named as such and drawn accent, so a
-- laptop being configured at the desk still says which set of numbers is running.
local function column_heading(profile, label)
    return cell(
        idle.active_profile:map(function(active)
            return active == profile and label .. " · live" or label
        end),
        idle.active_profile:map(function(active)
            return active == profile and theme.ACCENT or theme.TEXT_OFF
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
        cell("action · in order", theme.TEXT_OFF, theme.font.xs, { width = "Fill" }),
        column_heading("ac", "ac power"),
        row {
            width = theme.idle_profile_column,
            visible = has_battery,
            children = { column_heading("battery", "battery") },
        },
    },
}

-- The two chevrons that move a stage through `order`. Hidden rather than disabled at the ends: a
-- greyed-out arrow on the top row is a control asking to be clicked and then refusing, and
-- `idle.move` already treats out of range as a no-op, so nothing depends on them being hidden.
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
    -- Accent while either profile will run this stage, `ActionSettingRow`'s `anyEnabled`: a row dim
    -- in both columns is a stage that never happens, whichever cable is in.
    local any = settings:map(function(resolved)
        for _, profile in ipairs({ "ac", "battery" }) do
            if resolved[profile][item.key .. "_on"] and resolved[profile][item.key .. "_sec"] > 0 then
                return true
            end
        end
        return false
    end)
    local ink = any:map(function(enabled)
        return enabled and theme.ACCENT or theme.TEXT_OFF
    end)
    local body = panel_row {
        title = stage.title,
        -- The stage's own description, not "after <the row above>". That reading is only true when
        -- the row above is switched on, and a row's switches are per profile -- blanking can be on
        -- for AC and off for battery, which would make one label wrong in one column. The section's
        -- own description says the rule once, the row order shows it, and the timeline prints the
        -- running total it works out to.
        subtitle = stage.detail,
        height = theme.idle_row_height,
        leading = row {
            align_v = "Center",
            spacing = theme.spacing.xs,
            children = { reorder(item), cell(stage.icon, ink, theme.icon.md, { align_v = "Center" }) },
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

-- The descriptors the rows are built from: the stage, where it sits in the order, and the name of
-- whatever runs before it. Rebuilt only when the stored settings change, so a reorder rebuilds
-- three rows and a tick rebuilds none.
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

-- ## Behaviour
--
-- Both of these are the same fact from two directions -- something other than a timeout deciding
-- the session stays up -- so they sit together rather than in the mirror's separate card. Neither
-- is per-profile: a film is a film on either cable.
local behaviour_rows = {
    panel_row {
        icon = icons.play,
        title = "keep awake for media",
        subtitle = "video, camera, microphone, screen capture",
        height = theme.idle_row_height,
        icon_color = settings:map(function(resolved)
            return resolved.video_auto_inhibit and theme.ACCENT or theme.TEXT_OFF
        end),
        trailing = toggle(settings, function(resolved)
            return resolved.video_auto_inhibit
        end, function(on)
            idle.write(nil, "video_auto_inhibit", on)
        end),
    },
    panel_row {
        icon = icons.awake,
        title = "keep awake now",
        subtitle = "the same hold the bar circle takes",
        height = theme.idle_row_height,
        icon_color = idle.manual:map(function(manual)
            return manual and theme.ACCENT or theme.TEXT_OFF
        end),
        trailing = toggle(idle.manual, function(manual)
            return manual
        end, idle.set_manual),
    },
}

-- ## Assembly
--
-- Three cards, which is `IdleSettingsPanel.qml`'s own structure and was the thing most worth taking
-- from it. A flat column of rows under two grey words -- the first version of this file -- says
-- nothing about which settings belong together; a titled card with a description under the title
-- says it without a word of explanation.

-- One compact line rather than a `panel_header`: the masthead above already carries a plate and a
-- 16px bold title, and a second of those four lines down would be a second masthead.
local flow_strip = row {
    width = "Fill",
    align_v = "Center",
    spacing = theme.spacing.sm,
    children = {
        cell(icons.play, computed({ settings, idle.inhibited }, function(resolved, held)
            return (resolved.enabled and not held) and theme.ACCENT or theme.TEXT_OFF
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

-- `SettingsSection`: a glyph on a plate, a title, one line saying what the card is for, then the
-- rows. `panel_header` is that shape already, so a section is a header plus its children in a card.
local function section(glyph, title, description, children)
    local nodes = { panel_header { title = title, subtitle = description, icon = glyph, title_size = theme.font.xl } }
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

return panel {
    id = "idle_settings",
    namespace = "oblisk-idle-settings",
    layer = "Top",
    anchor = { top = true, bottom = true, left = true, right = true },
    exclusive = false,
    width = "Fill",
    height = "Fill",
    visible = open,
    child = rect {
        width = "Fill",
        height = "Fill",
        children = {
            -- The scrim and the click-outside catcher in one node: `hit::descend` walks children in
            -- reverse and stops at the card, so only a click beside the card lands here.
            button {
                width = "Fill",
                height = "Fill",
                cursor = "default",
                background = theme.SCRIM,
                on_click = function()
                    open:set(false)
                end,
            },
            panel_card(card_children, {
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
                border_width = theme.border_width,
                border_color = theme.BORDER,
            }),
        },
    },
}
