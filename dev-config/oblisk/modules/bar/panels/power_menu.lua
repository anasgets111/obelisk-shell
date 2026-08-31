-- Mirrors Bar/Panels/PowerMenu.qml, and replaces the `menu N` click counter that used to sit on
-- the bar. That button existed to prove Phase 21's input path and nothing else: it counted its own
-- clicks, opened the settings window and the popup at once, and had no counterpart in any real bar.
--
-- Quickshell's PowerMenu offers log out, restart and power off. This one offers lock, because lock
-- is the only session command a capability exposes (ADR-0052 decision 1) and the other three have
-- nowhere to land -- there is no logind capability behind them. Drawing three dead buttons to
-- match the mirror exactly would be worse than being one action short of it.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local panel_action = require("components.panel_action")
local section_header = require("components.section_header")
local ui_state = require("lib.ui_state")

local KIND = "power"

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
    ui_state.arm_osd("brightness")
end

-- Left zone, first module, which is where Quickshell's LeftSide.qml puts it.
local power_button = button {
    width = 70,
    height = 24,
    background = theme.SURFACE,
    radius = 6,
    on_click = function(rect, mouse_button)
        if mouse_button ~= "left" then
            return
        end
        ui_state.open_panel(KIND, rect)
    end,
    children = { cell("power", theme.RED) },
}

local body = {
    section_header("session"),
    cell(util.label(oblisk.battery, function(b)
        if not b.present then
            return "on ac power"
        end
        return string.format("battery %d%% %s", b.percent, b.charging and "charging" or "discharging")
    end), theme.DIM, 11),
    panel_action("system-lock-screen", "lock session", function()
        -- Straight to the capability, no confirmation and no countdown. Quickshell's ten-second
        -- countdown guards actions that lose unsaved work; locking loses nothing.
        oblisk.lock:invoke("lock")
    end, theme.MAUVE),
    panel_action("preferences-system", "settings", function()
        ui_state.settings_open:set(true)
    end),
    section_header("brightness"),
    -- The bar has no brightness module -- Quickshell's does not either -- so § 3.2's one command
    -- with an argument in it is driven from here (Phase 25 item 2). Two rows rather than one
    -- button reading left-up/right-down, because a panel row that means something different on
    -- each mouse button is a control nobody can guess at.
    cell(util.label(oblisk.brightness, function(b)
        return string.format("sun %d%%", b.percent)
    end), theme.DIM, 11),
    panel_action("list-add", "brighter", function()
        step_brightness(BRIGHTNESS_STEP)
    end),
    panel_action("list-remove", "dimmer", function()
        step_brightness(-BRIGHTNESS_STEP)
    end),
}

return { kind = KIND, button = power_button, body = body }
