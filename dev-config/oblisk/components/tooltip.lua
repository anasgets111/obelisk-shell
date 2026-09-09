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
--
-- Nothing opens while a panel is up. `DateTimeDisplay.qml` gates its own loader that way
-- (`requested: mouseArea.containsMouse && !panelOpen`), and the reason generalises to every slot on
-- the bar: the panel card hangs directly under the bar, so a tooltip opening into the same space is
-- a second sheet over the one the user just asked for. Gated on any panel, not this indicator's
-- own, because it is the card's position that collides, not its subject.
local theme = require("config.theme")
local panel_card = require("components.panel_card")
local ui_state = require("lib.ui_state")

return function(opts)
    return popup {
        id = opts.id,
        parent = opts.parent or "bar",
        anchor_rect = hover_rect(opts.slot),
        visible = computed({ hover(opts.slot), ui_state.panel_open }, function(is_hovered, panel_open)
            return is_hovered and not panel_open
        end),
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
            blur = true,
            border_width = theme.border_width,
            border_color = theme.BORDER,
            -- `padding_v` is per-tip because the surface is a fixed size: the one- and two-line
            -- tips are sized with `xs` already counted in, and widening it for all of them would
            -- squeeze their text rather than give it room. A tall body asks for its own.
            padding = {
                top = opts.padding_v or theme.spacing.xs,
                right = theme.spacing.sm,
                bottom = opts.padding_v or theme.spacing.xs,
                left = theme.spacing.sm,
            },
        }),
    }
end
