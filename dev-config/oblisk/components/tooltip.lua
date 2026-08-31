-- A hover tooltip: a `popup` that follows one hover slot (docs/adr/0062).
--
-- Two bindings and no state machine, which is the whole reason hover is a signal rather than a
-- callback. `visible` takes the boolean and `anchor_rect` takes the rect the engine wrote beside
-- it, so there is no edge for this file to miss and nothing to reset when a re-resolve replaces the
-- node underneath the pointer.
--
-- `grab = false`, and it is not optional. A grabbing popup takes the pointer, so the first thing it
-- would do on opening is take the pointer off the node whose hover opened it, which reads as a
-- tooltip that flickers forever. It also means this needs no armed input serial (§ 6.3), which is
-- what lets a hover open it at all: there is no click to carry one.
--
-- The 4px offset is doing the same job from the other side. This opens *below* its anchor, so the
-- pointer resting on the pill is not inside the tooltip, and the two never fight over who has it.
-- Moving the pointer down onto the tooltip does close it, because leaving the bar turns the hover
-- off -- correct for a tooltip, and the reason this component is not the one to build a
-- hover-to-open *panel* out of. That wants the pointer to be able to travel into it, which needs
-- the panel to declare a hover region of its own and the two to be OR-ed together.
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
        -- Not a default worth inheriting: § 6.3's `grab` is `true` unless a popup says otherwise,
        -- and a grabbing popup needs an armed input serial, which a hover has no way to produce.
        -- Leaving this line out is a tooltip that resolves `visible = true` and is then refused on
        -- every re-resolve, which is exactly what the first live run did.
        grab = false,
        anchor = "BottomLeft",
        gravity = "BottomRight",
        constraint_adjustment = { "FlipY", "SlideX" },
        offset = { x = 0, y = 4 },
        child = panel_card(opts.children, {
            background = "#181825ee",
            border_width = 1,
            border_color = theme.SURFACE,
        }),
    }
end
