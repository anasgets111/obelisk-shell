-- One modal's card and the motion `OModal.qml` gives it, for `modules/global/modal_host.lua` to
-- stack with the others under one scrim. The card fades, scales from 0.97 and rises by
-- `spacingMd`, OutCubic opening and InCubic closing (ADR-0146, ADR-0149). The wrapper lingers
-- visible through the exit, so switching modals cross-fades the old card out against the new one
-- coming in.
local theme = require("config.theme")
local util = require("lib.util")
local ui_state = require("lib.ui_state")

-- `Theme.qml`'s `modalClosedScale`.
local CLOSED_SCALE = 0.97

---@class ModalOpts
---@field kind string The `modal` state value that shows this one, e.g. `"launcher"`.
---@field card table The card node, positioned by its own `margin` or aligns within the screen.
---@field keyboard? boolean Take the keyboard exclusively while showing; a field inside needs it.

---@class Modal
---@field kind string
---@field keyboard boolean
---@field node table The screen-sized wrapper carrying the card and its motion.

---@param opts ModalOpts
---@return Modal
return function(opts)
    local showing = ui_state.modal_showing(opts.kind)
    -- The easing follows the direction, so the table is a signal; `from` is the entry.
    local animate = showing:map(function(open)
        local easing = open and "OutCubic" or "InCubic"
        return {
            opacity = { duration = theme.animation_ms, easing = easing, from = 0 },
            scale = { duration = theme.animation_ms, easing = easing, from = CLOSED_SCALE },
            translate = { duration = theme.animation_ms, easing = easing, from = { y = -theme.spacing.md } },
        }
    end)
    return {
        kind = opts.kind,
        keyboard = opts.keyboard or false,
        -- Screen-sized, so the card keeps its own placement (a computed `margin`, or centre
        -- aligns) inside it, and the scale pivots on the screen's centre, where the card sits.
        -- Stacking, not a column: a column governs its children's vertical placement itself, so a
        -- card's own `align_v` would be dropped and every card would hang from the top.
        -- Hidden once the exit has run: a hidden subtree is frozen and its fields are out of the
        -- keyboard's reach.
        node = rect {
            width = "Fill",
            height = "Fill",
            visible = util.linger(showing, theme.animation_ms),
            scale = showing:map(function(open)
                return open and 1 or CLOSED_SCALE
            end),
            translate = showing:map(function(open)
                return { y = open and 0 or -theme.spacing.md }
            end),
            opacity = showing:map(function(open)
                return open and 1 or 0
            end),
            animate = animate,
            children = { opts.card },
        },
    }
end
