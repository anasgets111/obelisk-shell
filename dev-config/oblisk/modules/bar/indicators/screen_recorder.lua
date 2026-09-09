-- Mirrors ScreenRecorder.qml: one circle with three states, and three mouse buttons that do
-- different things.
--
-- Left starts a region and stops a running capture, middle records the focused output, right opens
-- the panel. Three buttons on one indicator is unusual here -- every other one is left-click only,
-- and `components/icon_button.lua` guards that deliberately -- but it is the mirror's own design
-- and it is the right one: the two captures differ only in extent, so making the common pair one
-- click each beats a panel round trip, while the panel keeps every choice reachable without
-- remembering which button is which.
--
-- The ground says recording, the glyph says what a click will do. `activeColor` while recording and
-- the plain control ground while paused, matching the mirror: a paused capture is not consuming
-- anything, so it should not keep the bar lit.
local theme = require("config.theme")
local icons = require("config.icons")
local cell = require("components.cell")
local icon_button = require("components.icon_button")
local tooltip = require("components.tooltip")
local ui_state = require("lib.ui_state")
local recorder = require("lib.screen_recording")
local screen_recorder_panel = require("modules.bar.panels.screen_recorder_panel")

local SLOT = "screen_recorder"

local state_of = computed({ recorder.recording, recorder.paused }, function(up, held)
    if not up then
        return "idle"
    end
    return held and "paused" or "recording"
end)

local screen_recorder_module = icon_button(state_of:map(function(current)
    if current == "recording" then
        return icons.record_stop
    end
    return current == "paused" and icons.record_paused or icons.record_start
end), nil, {
    slot = SLOT,
    selected = ui_state.panel_showing(screen_recorder_panel.kind),
    background = state_of:map(function(current)
        return current == "recording" and theme.ACCENT or theme.GLASS_CONTROL
    end),
    -- `on_button` rather than `on_activate`: this is one of the three indicators whose extra mouse
    -- buttons are part of the design, so it takes the raw name.
    on_button = function(rect, mouse_button)
        if mouse_button == "right" then
            ui_state.toggle_panel(screen_recorder_panel.kind, rect)
            return
        end
        if mouse_button == "left" then
            if recorder.recording:get() then
                recorder.stop()
            else
                recorder.start("selection")
            end
            return
        end
        if mouse_button == "middle" and not recorder.recording:get() then
            recorder.start()
        end
    end,
})

-- Two lines rather than the mirror's one string: its tooltip is a sentence listing three buttons,
-- which at `font.xs` is either 400px wide or wrapped into something nobody reads. The first line is
-- the state, the second is what the buttons do -- the same facts, sorted.
local screen_recorder_tooltip = tooltip({
    id = "screen_recorder_tooltip",
    slot = SLOT,
    children = {
        cell(computed({ state_of, recorder.elapsed_text }, function(current, elapsed)
            if current == "idle" then
                return "not recording"
            end
            return string.format("%s %s", current == "paused" and "paused at" or "recording", elapsed)
        end), theme.FG, theme.font.sm),
        cell(recorder.recording:map(function(up)
            if up then
                return "left stop · right options"
            end
            return "left region · middle screen · right options"
        end), theme.DIM, theme.font.xs),
    },
})

return { indicator = screen_recorder_module, tooltip = screen_recorder_tooltip }
