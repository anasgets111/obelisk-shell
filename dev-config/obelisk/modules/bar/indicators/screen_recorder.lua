-- Mirrors ScreenRecorder.qml: one circle with three states, and three mouse buttons that do
-- different things.
--
-- Left starts a region or stops a capture; middle records focused output; right opens the panel.
-- Three buttons on one indicator are unusual here: every other one is left-click only, and
-- `components/icon_button.lua` deliberately guards that. Mirror makes each capture one click
-- because they differ only in extent; that beats a panel round trip.
-- The panel keeps every choice reachable without remembering which button is which.
--
-- Ground says recording; glyph says click action. `activeColor` while recording;
-- plain control ground while paused matches mirror: a paused capture consumes nothing,
-- so the bar need not stay lit.
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
            recorder.toggle()
            return
        end
        if mouse_button == "middle" and not recorder.recording:get() then
            recorder.start()
        end
    end,
})

-- Two lines rather than mirror's one tooltip: three buttons. At `font.xs`, it is 400px wide or
-- wraps into something nobody reads; first line is state, second is actions, facts stay sorted.
local screen_recorder_tooltip = tooltip({
    id = "screen_recorder_tooltip",
    slot = SLOT,
    children = {
        cell(computed({ state_of, recorder.elapsed_text }, function(current, elapsed)
            if current == "idle" then
                return "Not recording"
            end
            return string.format("%s %s", current == "paused" and "Paused at" or "Recording", elapsed)
        end), theme.FG, theme.font.sm),
        cell(recorder.recording:map(function(up)
            if up then
                return "Left stop · right options"
            end
            return "Left region · middle screen · right options"
        end), theme.DIM, theme.font.xs),
    },
})

return { indicator = screen_recorder_module, tooltip = screen_recorder_tooltip }
