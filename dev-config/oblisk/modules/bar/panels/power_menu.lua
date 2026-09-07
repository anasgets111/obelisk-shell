-- Mirrors Bar/Panels/PowerMenu.qml and replaces the `menu N` click counter. That button only proved
-- Phase 21's input path: it counted clicks and opened settings plus the popup.
--
-- The bar pill, `power_button` below, offers log out, restart, and power off behind ten-second
-- countdowns. A second click skips; right-click or cancel stops. `process.run` shells out to
-- `systemctl` and the compositor, as `PowerManagementService` does.
--
-- The panel adds lock, sleep, settings, and a brightness slider. Settings has no other door; lock
-- and
-- sleep lose nothing, so need no countdown. Sleep is the mirror service's `suspend()`, absent
-- there.
--
-- Countdown is a deadline in `oblisk.system.time`, not a timer. `system` pushes once a second;
-- seconds-left is a `computed`, and one `on_change` commits past the deadline (ADR-0115).
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local panel_row = require("components.panel_row")
local meter = require("components.meter")
local icon_button = require("components.icon_button")
local section_header = require("components.section_header")
local expanding_pill = require("components.expanding_pill")
local ui_state = require("lib.ui_state")

local KIND = "power"

-- Mirror `initialCountdown`.
local COUNTDOWN = 10

-- Pending action (`""` for none) and its `oblisk.system.time` deadline. Two `state()`s because a
-- table is not a signal type.
local pending = state("power_pending", "")
local deadline = state("power_deadline", 0)

local counting = pending:map(function(key)
    return key ~= ""
end)

local seconds_left = computed({ oblisk.system, deadline }, function(s, at)
    return math.max(0, at - ((s and s.time) or 0))
end)

local function detached(cmd, args)
    process.run(cmd, args, function() end, function() end)
end

-- Mirror `actions`, in order. `logout` is niri's `CompositorImpl.exitSession`.
local ACTIONS = {
    { key = "logout", title = "log out", icon = icons.logout, run = function()
        detached("niri", { "msg", "action", "quit", "--skip-confirmation" })
    end },
    { key = "reboot", title = "restart", icon = icons.power, run = function()
        detached("systemctl", { "reboot" })
    end },
    { key = "poweroff", title = "power off", icon = icons.shutdown, run = function()
        detached("systemctl", { "poweroff" })
    end },
}

local function cancel_countdown()
    pending:set("")
end

local function commit_pending()
    local key = pending:get()
    cancel_countdown()
    for _, action in ipairs(ACTIONS) do
        if action.key == key then
            action.run()
        end
    end
end

local function start_countdown(key)
    local s = oblisk.system:get()
    deadline:set(((s and s.time) or 0) + COUNTDOWN)
    pending:set(key)
end

oblisk.system:on_change(function(s)
    if pending:get() ~= "" and s.time >= deadline:get() then
        commit_pending()
    end
end)

-- Wraps rather than clamps, stopping at `BRIGHTNESS_STEP` instead of 0. `brightness:set(0)` blacks
-- an `intel_backlight` panel, so a control that can do that gets no second click.
local BRIGHTNESS_STEP = 10

local function step_brightness(delta)
    local b = oblisk.brightness:get()
    if b == nil then
        return
    end
    local stepped = b.percent + delta
    if stepped > 100 then
        stepped = BRIGHTNESS_STEP
    elseif stepped < BRIGHTNESS_STEP then
        stepped = 100
    end
    oblisk.brightness:invoke("set", stepped)
end

-- `ExpandingPill`: the power-off circle expands on hover to log out/restart/power off, and stays
-- open through a countdown (`holdOpen`). `components/expanding_pill.lua` owns the expansion; each
-- slot's ground, ring and countdown fill ease like the mirror's `IconButton` and `FillBar`, and
-- the pending action's opacity pulse is a looping keyframe sequence (ADR-0152).
--
-- During a countdown the pill stays open: the chosen action keeps its glyph under an accent ring,
-- the next slot shows seconds over a growing fill, and the third cancels. Left-click the chosen
-- action to run it;
-- click cancel or right-click anywhere to stop.
--
-- Right-click with no countdown opens the panel below, which adds lock, sleep, settings, and
-- brightness; settings has no other door.
local SLOT_COUNT = #ACTIONS
local pill = expanding_pill.new({ slot = "power-pill", hold_open = counting })

-- Countdown circle: the last slot unless it is the chosen action.
local function countdown_index(key)
    if key == ACTIONS[SLOT_COUNT].key then
        return SLOT_COUNT - 1
    end
    return SLOT_COUNT
end

local function slot(index)
    local action = ACTIONS[index]
    -- `"action"`, `"countdown"` or `"cancel"`, off the pending key alone.
    local role = pending:map(function(key)
        if key == "" or key == action.key then
            return "action"
        elseif countdown_index(key) == index then
            return "countdown"
        end
        return "cancel"
    end)
    local is_chosen = pending:map(function(key)
        return key == action.key
    end)
    local slot_hovered = hover("power-" .. action.key)
    local ground = computed({ slot_hovered, role }, function(is_hovered, what)
        if what == "countdown" then
            return theme.GLASS_CONTROL
        end
        return is_hovered and theme.GLASS_CONTROL_HOVER or theme.GLASS_CONTROL
    end)
    return pill.cell(button {
        align_h = "Center",
        hover = slot_hovered,
        radius = theme.item_radius,
        -- Plain fill bar cut by the circle's arc, `FillBar.qml` under a clip.
        clip = "Rounded",
        background = ground,
        border_width = theme.border_width,
        border_color = computed({ is_chosen, slot_hovered }, function(chosen, is_hovered)
            if chosen then
                return theme.ACCENT
            end
            return is_hovered and theme.GLASS_BORDER_HOVER or theme.GLASS_BORDER
        end),
        -- `SequentialAnimation on opacity { loops: Infinite; running: counting && isActionSlot }`:
        -- the chosen action breathes while its countdown runs. The gate is the entry itself
        -- (ADR-0152) -- no entry, no sequence, and the opacity falls back to the resolved `1`,
        -- which is what the mirror's `onRunningChanged` handler had to write by hand.
        animate = is_chosen:map(function(chosen)
            local eases = { background = theme.animation_ms, border_color = theme.animation_ms }
            if chosen then
                eases.opacity = {
                    duration = theme.animation_slow_ms,
                    easing = "InOutQuad",
                    loops = "Infinite",
                    keyframes = { 1, 0.4, 1 },
                }
            end
            return eases
        end),
        children = {
            -- Mirror `FillBar`: seconds elapsed as a ground growing from the left under the number.
            rect {
                width = computed({ role, seconds_left }, function(what, left)
                    if what ~= "countdown" then
                        return "0%"
                    end
                    local gone = math.max(0, math.min(COUNTDOWN, COUNTDOWN - left))
                    return string.format("%d%%", math.floor(gone * 100 / COUNTDOWN + 0.5))
                end),
                height = "Fill",
                background = theme.ON_HOVER,
                animate = { width = theme.animation_ms },
            },
            text {
                content = computed({ role, seconds_left }, function(what, left)
                    if what == "countdown" then
                        return tostring(left)
                    elseif what == "cancel" then
                        return icons.close
                    end
                    return action.icon
                end),
                foreground = role:map(function(what)
                    return what == "action" and theme.RED or theme.FG
                end),
                font_size = role:map(function(what)
                    return what == "countdown" and theme.font.sm or theme.icon.lg
                end),
                -- The same switch the size makes: the countdown is digits and belongs in the
                -- declared family, the resting state is a Nerd Font glyph and belongs in the icon
                -- one. `nil` is the declared chain.
                font = role:map(function(what)
                    return what ~= "countdown" and theme.icon_font or nil
                end),
                align_h = "Center",
                align_v = "Center",
            },
        },
        on_click = function(rect, mouse_button)
            local key = pending:get()
            if mouse_button == "right" then
                if key ~= "" then
                    cancel_countdown()
                else
                    ui_state.toggle_panel(KIND, rect)
                end
            elseif mouse_button == "left" then
                if key == "" then
                    start_countdown(action.key)
                elseif key == action.key then
                    commit_pending()
                elseif countdown_index(key) ~= index then
                    cancel_countdown()
                end
            end
        end,
    }, pill.expanded:map(function()
        -- The mirror's `collapsedIndex` is the last slot: power off is the one circle at rest.
        return index == SLOT_COUNT
    end))
end

local slots = {}
for index = 1, SLOT_COUNT do
    slots[index] = slot(index)
end

local power_button = pill.row(slots)

local body = {
    section_header("session"),
    cell(util.label(oblisk.battery, function(b)
        if not b.present then
            return "on ac power"
        end
        return string.format("battery %d%% %s%s", b.percent, util.battery_phrase(b.state), util.battery_eta(b))
    end), theme.DIM, theme.font.xs),
    panel_row {
        slot = "power-lock",
        icon = icons.lock,
        title = "lock session",
        color = theme.MAUVE,
        on_activate = function()
            -- Direct capability call, with no confirmation or countdown. Quickshell's ten-second
            -- countdown protects unsaved work; locking loses nothing.
            oblisk.lock:invoke("lock")
        end,
    },
    panel_row {
        slot = "power-sleep",
        icon = icons.sleep,
        title = "sleep",
        color = theme.MAUVE,
        on_activate = function()
            detached("systemctl", { "suspend" })
        end,
    },
    panel_row {
        slot = "power-settings",
        icon = icons.settings,
        title = "settings",
        on_activate = function()
            ui_state.settings_open:set(true)
        end,
    },
    section_header("brightness"),
    -- No brightness module, matching Quickshell, so § 3.2's one argument-taking command is driven
    -- here (Phase 25 item 2). Two buttons are clearer than left-up/right-down semantics.
    --
    -- One row, not three. The level is a bar between two buttons, like reference `Slider.qml` and
    -- `components/meter.lua` for battery/volume. No drag: a press supplies a rect and button name;
    -- nothing tracks motion into a value.
    row {
        width = "Fill",
        height = theme.control.sm,
        align_v = "Center",
        spacing = theme.spacing.sm,
        children = {
            icon_button(icons.minus, function()
                step_brightness(-BRIGHTNESS_STEP)
            end, { size = theme.control.xs, icon_size = theme.icon.xs }),
            meter(oblisk.brightness, function(b)
                return b.percent
            end, theme.YELLOW, "Fill"),
            icon_button(icons.plus, function()
                step_brightness(BRIGHTNESS_STEP)
            end, { size = theme.control.xs, icon_size = theme.icon.xs }),
            cell(util.label(oblisk.brightness, function(b)
                return string.format("%d%%", b.percent)
            end), theme.DIM, theme.font.xs),
        },
    },
}

return { kind = KIND, button = power_button, body = body }
