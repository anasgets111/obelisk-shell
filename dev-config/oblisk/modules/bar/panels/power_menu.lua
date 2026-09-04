-- Mirrors Bar/Panels/PowerMenu.qml, and replaces the `menu N` click counter that used to sit on
-- the bar. That button existed to prove Phase 21's input path and nothing else: it counted its own
-- clicks, opened the settings window and the popup at once, and had no counterpart in any real bar.
--
-- Quickshell's PowerMenu is a pill on the bar: log out, restart and power off, each behind a
-- ten-second countdown that a second click skips and a right click or the cancel slot stops. The
-- pill is `power_button` below, built the same way the mirror does it: `PowerManagementService`
-- shells out to `systemctl` and to the compositor, and `process.run` is that.
--
-- The panel under it has no counterpart in the mirror. Lock, sleep and settings are its rows and
-- brightness its slider; settings has no other door, and lock and sleep lose nothing and so need
-- no countdown. Sleep is the mirror service's `suspend()`, which its menu never shows.
--
-- The countdown is a deadline in `oblisk.system.time`, not a timer: `system` pushes once a second,
-- so "seconds left" is a `computed` off it and the commit is one `on_change` (ADR-0115) watching
-- the clock pass the deadline.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local panel_row = require("components.panel_row")
local meter = require("components.meter")
local icon_button = require("components.icon_button")
local section_header = require("components.section_header")
local ui_state = require("lib.ui_state")

local KIND = "power"

-- The mirror's `initialCountdown`.
local COUNTDOWN = 10

-- Which session action is counting down, `""` for none, and the `oblisk.system.time` it fires at.
-- Two `state()`s rather than one table: a table is not a signal type.
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

-- The mirror's `actions`, in its order. `logout` is `CompositorImpl.exitSession` for niri.
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

-- Wraps rather than clamping, and it stops at `BRIGHTNESS_STEP` rather than 0. A control that can
-- black the panel out with one stray click is a control nobody clicks twice, and
-- `brightness:set(0)` on an `intel_backlight` does exactly that.
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

-- The mirror's `ExpandingPill`: one circle on the bar, the power off, that widens on hover into
-- three -- log out, restart, power off -- and narrows back when the pointer leaves. `hover` on the
-- row holding the three, not on each circle, since a hover region answers containment and a
-- pointer crossing the gap between two circles never leaves the row. That is what the mirror's
-- collapse timer exists to paper over, and why `workspace_strip.lua` thought a pill needed one.
-- No width animation; the engine has none, and the volume pill snaps open the same way.
--
-- While an action counts down the pill holds itself open and the three circles change roles, the
-- mirror's three slots: the chosen action keeps its glyph under an accent ring, the circle next to
-- it shows the seconds left over a fill that grows as they pass, and the third is a cancel. A left
-- click on the chosen action runs it now, a click on the cancel or a right click anywhere stops it.
--
-- A right click while nothing counts opens the panel below, which the mirror does not have: lock,
-- sleep, settings and brightness live there, and settings has no other door.
local SLOT_COUNT = #ACTIONS
local pill_hovered = hover("power-pill")
local expanded = computed({ pill_hovered, counting }, function(is_hovered, any)
    return is_hovered or any
end)

-- Which circle carries the countdown: the last, unless the last is the chosen action.
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
    return button {
        width = theme.item_width,
        height = theme.item_height,
        align_h = "Center",
        align_v = "Center",
        hover = slot_hovered,
        radius = theme.item_radius,
        background = ground,
        border_width = theme.border_width,
        border_color = computed({ is_chosen, slot_hovered }, function(chosen, is_hovered)
            if chosen then
                return theme.ACCENT
            end
            return is_hovered and theme.GLASS_BORDER_HOVER or theme.GLASS_BORDER
        end),
        visible = expanded:map(function(open)
            return open or index == SLOT_COUNT
        end),
        children = {
            -- The mirror's `FillBar` on the countdown slot: the seconds gone, as a ground that grows
            -- from the left under the number.
            rect {
                width = computed({ role, seconds_left }, function(what, left)
                    if what ~= "countdown" then
                        return "0%"
                    end
                    local gone = math.max(0, math.min(COUNTDOWN, COUNTDOWN - left))
                    return string.format("%d%%", math.floor(gone * 100 / COUNTDOWN + 0.5))
                end),
                height = "Fill",
                radius = theme.item_radius,
                background = theme.ON_HOVER,
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
    }
end

local slots = {}
for index = 1, SLOT_COUNT do
    slots[index] = slot(index)
end

local power_button = row {
    height = theme.item_height,
    align_v = "Center",
    spacing = theme.spacing.sm,
    hover = pill_hovered,
    children = slots,
}

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
            -- Straight to the capability, no confirmation and no countdown. Quickshell's ten-second
            -- countdown guards actions that lose unsaved work; locking loses nothing.
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
    -- The bar has no brightness module -- Quickshell's does not either -- so § 3.2's one command
    -- with an argument in it is driven from here (Phase 25 item 2). Two buttons rather than one
    -- reading left-up/right-down, because a control that means something different on each mouse
    -- button is one nobody can guess at.
    --
    -- One row, not three. The level draws as a bar between its own two buttons, which is what
    -- `Slider.qml` is in the reference config and what `components/meter.lua` already draws for the
    -- battery and the volume. There is no drag: a press carries a rect and a button name, and
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
