-- The mirror's `MainScreen`: one surface holding the scrim, the outside-click catcher and every
-- modal, with `ShellUiState.activeModal` choosing which card shows. One surface, not one per
-- modal: two cross-fading surfaces each brought a scrim, stacking to nearly double darkness on
-- frames never in step, and a separate scrim surface stacked above the modal and took its outside
-- click, since same-layer order is the compositor's to decide.
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
    -- Exclusive only while a modal that wants it is showing, not while one is on its way out: a
    -- field must be typable without a click, and a surface fading out must hold nothing.
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
                -- Dims, and does not blur. Blurring the whole screen here was tried and is the
                -- louder reading: the desktop stops being legible at all, and a modal that only
                -- wants attention does not need the rest of the screen destroyed. The cards ask
                -- for themselves instead (ADR-0195), so what is blurred is the glass, and the
                -- scrim behind it stays a dim over a sharp desktop.
                --
                -- Blur cannot fade either way. `set_blur_region` carries a region and nothing
                -- else, so the step is a step; keeping it inside the card's own box is what makes
                -- that unnoticeable, where the full-screen version had to be timed against the dim
                -- to hide it.
                opacity = any_modal:map(function(open)
                    return open and 1 or 0
                end),
                animate = any_modal:map(function(open)
                    return {
                        opacity = { duration = theme.animation_ms, easing = open and "OutCubic" or "InCubic", from = 0 },
                    }
                end),
            },
            -- Outside catcher and the cards' parent in one screen-sized node: `hit::descend` stops
            -- at the first child containing the point, so the catcher has to be the node the cards
            -- sit in, not a sibling under them. Clicks on a card stop at the card.
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
