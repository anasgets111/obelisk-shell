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

-- Two lines say what holds it and what happens next if nothing does. The mirror puts reasons in a
-- tooltip too; this config's second line makes the circle worth hovering when nothing is held.
local idle_tooltip = tooltip({
    id = "idle_tooltip",
    slot = SLOT,
    children = {
        cell(computed({ idle.reasons, idle.inhibited }, idle.held_text), theme.FG, theme.font.sm),
        cell(
            computed({ idle.schedule, idle.arming, idle.manual, idle.enabled }, function(plan, arming, manual, on)
                if not on or plan.total == 0 then
                    return "click to hold · right-click for settings"
                end
                -- `manual`, not `inhibited`: offering to drop a hold a camera took does nothing.
                if manual then
                    return "click to drop the manual hold"
                end
                -- The armed stage's countdown, matching the modal masthead.
                for _, entry in ipairs(plan.list) do
                    if entry.key == arming.key then
                        return string.format("%s in %s", entry.title,
                            idle.clock(math.max(0, entry.delay - arming.elapsed)))
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
