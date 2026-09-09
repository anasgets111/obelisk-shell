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
--
-- ## Sizing
-- `Tooltip.qml` is `Math.max(controlWidthLg, content.implicitWidth + hPadding * 2)` on both axes:
-- the window is whatever its words need. Omitting `width`/`height` says the same thing here -- a
-- `popup` axis left off is `Content`, measured off the resolved tree on the pass that opens it
-- (`layout::node::toplevel::parse_popup_extent`).
--
-- Every tooltip carried a hand-guessed pair of numbers before the engine could measure one, and
-- they were wrong wherever the text was not the sentence the number was guessed against: a battery
-- reading "69% charge limit reached, 1h 20m left" wants 253px and had 180, a headset named
-- "SteelSeries Arctis Nova Pro Wireless" wants 241 and had 220, an idle hold naming three programs
-- wants 266 and had 240. The card is content-sized, so it kept its natural width inside the smaller
-- surface and the surface cut it -- no ellipsis, because `text` elides only when it is given a
-- width to elide into. The heights were wrong the other way: 44 to 64 declared for 36 of content.
--
-- No floor. The mirror's exists because a two-word tooltip should still look like one; nothing here
-- comes near it, and a number no tooltip reaches is a number that only has to be maintained.
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
        -- Omitted on purpose: the surface is the card's own box. `date_time.lua` is the one
        -- caller that still declares them, because its rows fill the card rather than sizing it.
        width = opts.width,
        height = opts.height,
        -- § 6 defaults `grab` to `true`, but hover cannot produce the required input serial.
        -- Without
        -- this, `visible = true` resolves then gets refused on every re-resolve, as the first live
        -- run did.
        grab = false,
        -- Centred under the indicator, which is `Tooltip.qml`'s
        -- `anchor.rect: Qt.rect(target.width / 2, ...)` with `edges` and `gravity` both `Bottom`.
        -- `BottomLeft`/`BottomRight` hung it from the slot's left edge and let it run rightwards,
        -- which put a 250px tip on a 24px icon almost entirely to one side of what it describes --
        -- and pushed the rightmost indicators' tips off the screen for `SlideX` to drag back.
        -- Worth restating now that the width is the words' own: a tip that changes width would
        -- otherwise grow in one direction only, walking away from its anchor as its text changed.
        anchor = "Bottom",
        gravity = "Bottom",
        constraint_adjustment = { "FlipY", "SlideX" },
        offset = { x = 0, y = theme.panel_gap },
        child = panel_card(opts.children, {
            background = theme.GLASS,
            blur = true,
            border_width = theme.border_width,
            border_color = theme.BORDER,
            -- `padding_v` is per-tip because a two-line tip and a month grid do not want the same
            -- air above them: `xs` reads as a label's inset, `md` as a card's. The surface now
            -- follows whichever is asked for instead of having to be told about it.
            padding = {
                top = opts.padding_v or theme.spacing.xs,
                right = theme.spacing.sm,
                bottom = opts.padding_v or theme.spacing.xs,
                left = theme.spacing.sm,
            },
        }),
    }
end
