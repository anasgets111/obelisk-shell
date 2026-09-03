-- Mirrors Bar/Panels/PowerMenu.qml, and replaces the `menu N` click counter that used to sit on
-- the bar. That button existed to prove Phase 21's input path and nothing else: it counted its own
-- clicks, opened the settings window and the popup at once, and had no counterpart in any real bar.
--
-- Quickshell's PowerMenu offers log out, restart and power off. This one offers lock, because lock
-- is the only session command a capability exposes (ADR-0052 decision 1) and the other three have
-- nowhere to land -- there is no logind capability behind them. Drawing three dead buttons to
-- match the mirror exactly would be worse than being one action short of it.
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

-- Left zone, first module, which is where Quickshell's LeftSide.qml puts it. A glass circle like
-- every other control, with a red glyph.
--
-- The red was the ground until now, and a solid #f38ba8 disc was the loudest thing on a bar whose
-- whole point is that nothing on it is opaque. One rule everywhere on this bar: a filled ground
-- means "this is on", a coloured glyph means "this is what it does". Ending the session is the
-- second kind. `rescue` and `privacy` keep their filled grounds, because both are alerts that are
-- absent until they are not.
local power_button = icon_button(icons.shutdown, function(rect)
    ui_state.toggle_panel(KIND, rect)
end, {
    slot = "power",
    selected = ui_state.panel_showing(KIND),
    foreground = theme.RED,
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
