-- One modal card and the motion `OModal.qml` gives it, for `modules/global/modal_host.lua` to stack
-- under one scrim. The card fades, scales from 0.97, rises by `spacingMd`, and uses OutCubic
-- opening and InCubic closing (ADR-0146, ADR-0149). The wrapper lingers through exit. A modal
-- switch cross-fades the old card against the new one.
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
        -- Screen-sized, so the card keeps its own `margin` or centre alignment, and scale pivots on
        -- the screen's centre. Stacking, not a column: columns control child placement, which would
        -- drop a card's own `align_v` and hang every card from the top. Hidden after exit, subtrees
        -- freeze and their fields cannot receive keyboard input.
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
