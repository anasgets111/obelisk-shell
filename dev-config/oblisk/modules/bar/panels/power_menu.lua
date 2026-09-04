-- Mirrors Bar/Panels/PowerMenu.qml, and replaces the `menu N` click counter that used to sit on
-- the bar. That button existed to prove Phase 21's input path and nothing else: it counted its own
-- clicks, opened the settings window and the popup at once, and had no counterpart in any real bar.
--
-- Quickshell's PowerMenu offers log out, restart and power off, each behind a ten-second countdown
-- that a second click skips and a right click or the cancel slot stops. Those three are here, the
-- same way the mirror does them: `PowerManagementService` shells out to `systemctl` and to the
-- compositor, and `process.run` is that. Lock and sleep are here too, straight through, since
-- neither loses unsaved work; sleep is the mirror service's `suspend()`, which its menu never shows
-- and a laptop wants.
--
-- The countdown is a deadline in `oblisk.system.time`, not a timer: `system` pushes once a second,
-- so "seconds left" is a `computed` off it and the commit is one `on_change` (ADR-0115) watching
-- the clock pass the deadline. It keeps running with the panel closed, the way the mirror's pill
-- holds itself open, and the bar button counts down in its place so it stays in view.
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

-- One session action as a row. Clicking it starts the countdown; clicking it while it counts is
-- the mirror's "execute now"; the cancel button beside it is the mirror's cancel slot. The other
-- two rows dim while one counts, since a click on them would do nothing.
local function action_row(action)
    local is_this = pending:map(function(key)
        return key == action.key
    end)
    return panel_row {
        slot = "power-" .. action.key,
        icon = action.icon,
        title = action.title,
        color = theme.RED,
        subtitle = computed({ is_this, seconds_left }, function(mine, left)
            return mine and string.format("in %ds, click to %s now", left, action.title) or nil
        end),
        opacity = computed({ counting, is_this }, function(any, mine)
            return (any and not mine) and theme.opacity.muted or nil
        end),
        trailing = icon_button(icons.close, cancel_countdown, {
            slot = "power-cancel-" .. action.key,
            size = theme.control.xs,
            icon_size = theme.icon.xs,
            visible = is_this,
        }),
        on_activate = function()
            local key = pending:get()
            if key == action.key then
                commit_pending()
            elseif key == "" then
                start_countdown(action.key)
            end
        end,
    }
end

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

-- Left zone, first module, which is where Quickshell's LeftSide.qml puts it. A glass circle like
-- every other control, with a red glyph.
--
-- The red was the ground until now, and a solid #f38ba8 disc was the loudest thing on a bar whose
-- whole point is that nothing on it is opaque. One rule everywhere on this bar: a filled ground
-- means "this is on", a coloured glyph means "this is what it does". Ending the session is the
-- second kind. `rescue` and `privacy` keep their filled grounds, because both are alerts that are
-- absent until they are not.
--
-- While an action counts down the circle shows the seconds left on a red ground, which is the
-- mirror's countdown slot, so a closed panel does not hide a pending power off.
local power_button = icon_button(computed({ counting, seconds_left }, function(any, left)
    return any and tostring(left) or icons.shutdown
end), function(rect)
    ui_state.toggle_panel(KIND, rect)
end, {
    slot = "power",
    selected = ui_state.panel_showing(KIND),
    foreground = counting:map(function(any)
        return any and theme.BG or theme.RED
    end),
    background = counting:map(function(any)
        return any and theme.RED or theme.GLASS_CONTROL
    end),
    background_hover = counting:map(function(any)
        return any and theme.RED or theme.GLASS_CONTROL_HOVER
    end),
})

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
    section_header("power"),
    action_row(ACTIONS[1]),
    action_row(ACTIONS[2]),
    action_row(ACTIONS[3]),
    -- The mirror's `FillBar` on the countdown slot: how much of the ten seconds has gone.
    meter(seconds_left, function(left)
        return (COUNTDOWN - left) * 100 / COUNTDOWN
    end, theme.RED, "Fill", nil, counting),
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
