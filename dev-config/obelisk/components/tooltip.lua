-- Hover tooltip: a `popup` following one hover slot (ADR-0062). Two bindings are enough: `visible`
-- takes the hover boolean and `anchor_rect` takes the engine-written rect.
--
-- `grab = false` is required. A grabbing popup takes the pointer off its source node and flickers;
-- hover also has no click to carry the required armed input serial.
--
-- The 4px offset opens below the anchor, keeping a pointer resting on the pill out of the tooltip
-- so their hover states do not fight. Moving down closes it when leaving the bar turns hover off.
-- A hover-open panel needs its own hover region, OR-ed with the bar's, so the pointer can enter it.
--
-- Nothing parented to the bar opens while a panel is up. `DateTimeDisplay.qml` gates its loader with
-- (`requested: mouseArea.containsMouse && !panelOpen`) because the panel card hangs under every
-- slot. Gate on any panel: its position collides, not its subject.
--
-- ## Sizing
-- `Tooltip.qml` is `Math.max(controlWidthLg, content.implicitWidth + hPadding * 2)` on both axes:
-- the window is whatever its words need. Omitting `width`/`height` leaves the `popup` axis at
-- `Content`, measured from the resolved tree when it opens
-- (`layout::node::toplevel::parse_popup_extent`).
--
-- Before the engine could measure one, hand-guessed widths were wrong: "69% charge limit reached,
-- 1h 20m left" wants 253px but had 180, "SteelSeries Arctis Nova Pro Wireless" wants 241 but had
-- 220, and an idle hold naming three programs wants 266 but had 240. Content sizing kept the
-- natural width inside the smaller surface, which clipped it; `text` elides only with a width to
-- elide into, so there was no ellipsis. Heights had the opposite error: 44 to 64 declared for 36.
--
-- No floor. The mirror keeps one so a two-word tooltip still looks like one; no tooltip here
-- approaches it, so adding an unreachable number would only add maintenance.
local theme = require("config.theme")
local panel_card = require("components.panel_card")
local ui_state = require("lib.ui_state")

return function(opts)
    return popup {
        id = opts.id,
        parent = opts.parent or "bar",
        anchor_rect = hover_rect(opts.slot),
        visible = computed({ hover(opts.slot), ui_state.panel_open }, function(is_hovered, panel_open)
            return is_hovered and ((opts.parent or "bar") ~= "bar" or not panel_open)
        end),
        -- The surface is the card's own box. `date_time.lua` is the one caller that still declares
        -- width and height because its rows fill the card rather than sizing it.
        width = opts.width,
        height = opts.height,
        -- `grab` defaults to `true`, but hover cannot produce the required input serial.
        -- Without this, `visible = true` resolves and is refused on every re-resolve.
        grab = false,
        -- Centre under the indicator, matching `Tooltip.qml`'s
        -- `anchor.rect: Qt.rect(target.width / 2, ...)` with `edges` and `gravity` both `Bottom`.
        -- `BottomLeft`/`BottomRight` hung it from the slot's left edge and let it run rightwards,
        -- which put a 250px tip on a 24px icon almost entirely to one side of what it describes --
        -- and pushed the rightmost indicators' tips off the screen for `SlideX` to drag back.
        -- Content-sized tips must stay centred as their text changes, or they grow in one direction
        -- and walk away from the anchor.
        anchor = "Bottom",
        gravity = "Bottom",
        constraint_adjustment = { "FlipY", "SlideX" },
        offset = { x = 0, y = theme.panel_gap },
        child = panel_card(opts.children, {
            background = theme.GLASS,
            blur = true,
            border_width = theme.border_width,
            border_color = theme.BORDER,
            -- `padding_v` is per-tip: `xs` suits a label's inset and `md` suits a month grid's
            -- card. The surface follows the requested value.
            padding = {
                top = opts.padding_v or theme.spacing.xs,
                right = theme.spacing.sm,
                bottom = opts.padding_v or theme.spacing.xs,
                left = theme.spacing.sm,
            },
        }),
    }
end
