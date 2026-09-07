-- Mirrors IdleInhibitor.qml: one circle shows session holds and adds a manual hold on click.
--
-- Two glyphs swap on the *manual* hold: the cup means "I asked for this", crossed-out zeds mean
-- "the shell is watching". The accent ground means any hold, so media can light the circle without
-- changing its glyph; a clicked hold survives media ending.
--
-- Right-click opens `modules/global/idle_settings.lua`, matching
-- `IdleInhibitor.qml`'s `ShellUiState.openModal("idleSettings")`.
local theme = require("config.theme")
local icons = require("config.icons")
local cell = require("components.cell")
local icon_button = require("components.icon_button")
local tooltip = require("components.tooltip")
local ui_state = require("lib.ui_state")
local idle = require("lib.idle")

local SLOT = "idle"

local indicator = icon_button(idle.manual:map(function(manual)
    return manual and icons.awake or icons.idle
end), nil, {
    slot = SLOT,
    selected = ui_state.idle_settings_open,
    background = idle.inhibited:map(function(held)
        return held and theme.ACCENT or theme.GLASS_CONTROL
    end),
    background_hover = idle.inhibited:map(function(held)
        return held and theme.ACCENT_HOVER or theme.GLASS_CONTROL_HOVER
    end),
    on_button = function(_, mouse_button)
        if mouse_button == "right" then
            ui_state.toggle_modal("idle_settings")
        elseif mouse_button == "left" then
            idle.set_manual(not idle.manual:get())
        end
    end,
})

-- Two lines: what is holding it, and what happens next if nothing is. The mirror puts the reasons
-- in a tooltip too; the second line is this config's, and it is the one that makes the circle worth
-- hovering when nothing is held.
local idle_tooltip = tooltip({
    id = "idle_tooltip",
    slot = SLOT,
    width = 240,
    height = 60,
    children = {
        cell(idle.reasons:map(function(reasons)
            if #reasons == 0 then
                return "nothing is holding this awake"
            end
            return "held awake by " .. table.concat(reasons, ", ")
        end), theme.FG, theme.font.sm),
        cell(
            computed({ idle.schedule, idle.arming, idle.inhibited, idle.enabled }, function(plan, arming, held, on)
                if not on or plan.total == 0 then
                    return "click to hold · right-click for settings"
                end
                if held then
                    return "click to drop the manual hold"
                end
                -- The armed stage's countdown, matching the modal masthead.
                for _, entry in ipairs(plan.list) do
                    if entry.key == arming.key then
                        return string.format("%s in %s", entry.title, idle.clock(math.max(0, entry.delay - arming.elapsed)))
                    end
                end
                return "nothing is counting down"
            end),
            theme.DIM,
            theme.font.xs
        ),
    },
})

return { indicator = indicator, tooltip = idle_tooltip }
