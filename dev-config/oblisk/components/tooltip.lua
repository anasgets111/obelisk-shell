-- Hover tooltip: a `popup` following one hover slot (ADR-0062).
-- Two bindings, no state machine: hover is a signal. `visible` takes its boolean and `anchor_rect`
-- takes the rect the engine wrote beside it, leaving nothing to reset when re-resolve replaces the
-- node under the pointer.
-- `grab = false` is required. A grabbing popup takes the pointer off the node whose hover opened
-- it, causing endless flicker. It also avoids the armed input serial required by § 6, which hover
-- has no click to carry.
-- The 4px offset opens below the anchor, keeping a pointer resting on the pill out of the tooltip
-- so their hover states do not fight. Moving down onto it closes it when leaving the bar turns
-- hover off, which is right for a tooltip. A hover-open panel needs its own hover region, OR-ed
-- with the bar's,
-- so the pointer can travel into it.
local theme = require("config.theme")
local panel_card = require("components.panel_card")

return function(opts)
    return popup {
        id = opts.id,
        parent = opts.parent or "bar",
        anchor_rect = hover_rect(opts.slot),
        visible = hover(opts.slot),
        width = opts.width,
        height = opts.height,
        -- § 6 defaults `grab` to `true`, but hover cannot produce the required input serial.
        -- Without
        -- this, `visible = true` resolves then gets refused on every re-resolve, as the first live
        -- run did.
        grab = false,
        anchor = "BottomLeft",
        gravity = "BottomRight",
        constraint_adjustment = { "FlipY", "SlideX" },
        offset = { x = 0, y = theme.panel_gap },
        child = panel_card(opts.children, {
            background = theme.GLASS,
            border_width = theme.border_width,
            border_color = theme.BORDER,
        }),
    }
end
