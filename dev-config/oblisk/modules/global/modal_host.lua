-- The mirror's `MainScreen`: one surface holds the scrim, outside-click catcher, and every modal;
-- `ShellUiState.activeModal` chooses the card. One surface, not one per modal: two cross-fading
-- surfaces each brought a scrim, nearly doubling darkness on frames never in step. A separate scrim
-- above the modal also took outside clicks, since same-layer order is the compositor's to decide.
local theme = require("config.theme")
local util = require("lib.util")
local ui_state = require("lib.ui_state")

local modals = {
    require("modules.global.launcher"),
    require("modules.global.wallpaper_picker"),
    require("modules.global.idle_settings"),
}

local any_modal = ui_state.active_modal:map(function(kind)
    return kind ~= ""
end)

local cards = {}
for _, modal in ipairs(modals) do
    table.insert(cards, modal.node)
end

return panel {
    id = "modal_host",
    namespace = "oblisk-modal-host",
    layer = "Top",
    anchor = { top = true, bottom = true, left = true, right = true },
    exclusive = false,
    width = "Fill",
    height = "Fill",
    -- Mapped through the last card's exit fade (ADR-0146).
    visible = util.linger(any_modal, theme.animation_ms),
    -- Exclusive only while a modal that wants it shows, not while it exits: a field must be typable
    -- without a click, and a fading surface must hold nothing.
    keyboard_interactivity = ui_state.active_modal:map(function(kind)
        for _, modal in ipairs(modals) do
            if modal.kind == kind and modal.keyboard then
                return "Exclusive"
            end
        end
        return "None"
    end),
    child = rect {
        width = "Fill",
        height = "Fill",
        children = {
            -- `OModal`'s scrim: its opacity follows the open progress, OutCubic in and InCubic out.
            rect {
                width = "Fill",
                height = "Fill",
                background = theme.SCRIM,
                -- Dims, not blur. Full-screen blur made the desktop illegible; a modal that needs
                -- attention does not need the rest destroyed. Cards request blur themselves
                -- (ADR-0195), so only the glass blurs and the scrim dims a sharp desktop.
                --
                -- Blur cannot fade: `set_blur_region` carries only a region, so the step is a step.
                -- Keeping it inside the card's box makes it unnoticeable; the full-screen version
                -- had to be timed against the dim to hide it.
                opacity = any_modal:map(function(open)
                    return open and 1 or 0
                end),
                animate = any_modal:map(function(open)
                    return {
                        opacity = { duration = theme.animation_ms, easing = open and "OutCubic" or "InCubic", from = 0 },
                    }
                end),
            },
            -- Outside catcher and cards' parent in one screen-sized node: `hit::descend` stops at
            -- the first child containing the point, so the catcher must contain cards, not sit
            -- under them. Card clicks stop at the card.
            button {
                width = "Fill",
                height = "Fill",
                cursor = "default",
                on_click = function()
                    ui_state.close_modal(ui_state.active_modal:get())
                end,
                children = cards,
            },
        },
    },
}
