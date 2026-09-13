-- `OSDCard.qml`: bottom-centered glass card with two layouts selected by the entry's level.
-- `modules/osd/service.lua` supplies the entry. It follows the mirror's `Behavior on opacity`/`y`
-- and lingers through exit (ADR-0146), because the mirror unmaps its window before its fade-out.
--
-- One `panel` with two `visible`-switched rows, not two panels. Each surface has its own
-- compositor identity; otherwise a volume change during a toggle would overlap at one position.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local glyph = require("components.glyph")
local meter = require("components.meter")
local osd = require("modules.osd.service")

-- Card travel and entry/exit durations.
--
-- The mirror's `osdAnimationOffset` is 60px over `animationDuration * 1.5`. This card acknowledges
-- a key already pressed, so a 60px swoop several dozen times a day spends time not reading it.
-- Its short settle says "here is the readout"; arrival motion belongs to the notification stack
-- (`components/notification_card.lua`).
local SLIDE = theme.s(12, 8)
-- Arrival uses the shell's beat; exit is quicker because a card whose two seconds are up is not
-- news.
local RISE_MS = theme.animation_ms
local FALL_MS = theme.animation_fast_ms

-- Both layouts inset their content by the same amount.
local PADDING = theme.spacing.xl

-- `OSDCard.qml` sizes itself `isSlider ? osdSliderWidth : Math.max(osdToggleMinWidth,
-- _toggleWidth)`, with `_toggleWidth` measured from `labelText.implicitWidth`. The toggle row now
-- sizes to its content; `geometry` no longer supplies the width, so the card, surface and
-- `set_size` follow it without a settling pass.
--
-- The mirror counts `spacingLg` twice where this layout has one gap. Add the second half on each
-- side so a text-tight card does not look cramped without moving its contents off centre.
local SLACK = theme.spacing.lg / 2

local function read(field)
    return osd.entry:map(function(e)
        return e[field]
    end)
end

local function bold(field)
    return osd.entry:map(function(e)
        return { { text = e[field] or "", bold = true } }
    end)
end

-- Slider layout: accent glyph, filling track, bold readout.
local level_row = row {
    -- `osdSliderWidth`: a track has no intrinsic width, so this layout states one. The toggle row
    -- measures instead; only one is visible, and an invisible child takes no space.
    width = theme.osd_width,
    height = "Fill",
    align_v = "Center",
    spacing = theme.spacing.lg,
    padding = { left = PADDING, right = PADDING },
    visible = osd.entry:map(function(e)
        return e.level ~= nil
    end),
    children = {
        glyph(read("glyph"), theme.ACCENT, theme.font.xxl, { align_v = "Center" }),
        -- A repeated volume or brightness key moves the target every few frames. Eased retargeting
        -- restarts from a standstill (ADR-0145), so the fill trails the percentage; a spring keeps
        -- its velocity (ADR-0154) and arrives with the number.
        meter(osd.entry, function(e)
            return e.level or 0
        end, osd.entry:map(function(e)
            return e.color or theme.ACCENT
        end), "Fill", theme.osd_track, { motion = theme.spring_tracking }),
        text {
            content = bold("text"),
            foreground = theme.FG,
            font_size = theme.font.lg,
            width = theme.s(52, 40),
            text_align = "End",
            align_v = "Center",
        },
    },
}

-- Toggle layout: glyph in an accent-tinted tile and bold text beside it, centered.
local fact_row = row {
    -- No `width`: the card is these words. `osdToggleMinWidth` is their floor; `align_h` centres
    -- the pair on a card sized by that floor rather than by the text.
    min_width = theme.osd_toggle_min,
    height = "Fill",
    align_h = "Center",
    align_v = "Center",
    spacing = theme.spacing.lg,
    -- The mirror counts this inset in `_toggleWidth`; without it, the words reach the card edge and
    -- can run past it.
    padding = { left = PADDING + SLACK, right = PADDING + SLACK },
    visible = osd.entry:map(function(e)
        return e.level == nil
    end),
    children = {
        -- `align_*` places the box, not its child; a filling row centers the glyph inside the tile.
        rect {
            width = theme.osd_tile,
            height = theme.osd_tile,
            align_v = "Center",
            background = theme.ACCENT_LIGHT,
            border_width = theme.border_width,
            border_color = theme.ACCENT_MEDIUM,
            radius = theme.radius.md,
            children = { row {
                width = "Fill",
                height = "Fill",
                align_h = "Center",
                align_v = "Center",
                children = { glyph(read("glyph"), theme.ACCENT, theme.font.xl, { align_v = "Center" }) },
            } },
        },
        -- `labelText`. No `width`, so it sizes to its own words and everything above measures it.
        text {
            content = bold("text"),
            foreground = theme.FG,
            font_size = theme.font.lg,
            align_v = "Center",
        },
    },
}

return panel {
    id = "osd",
    layer = "Overlay",
    -- No `left`/`right`: anchors map directly to `zwlr_layer_surface_v1`; the
    -- `renderer/src/wayland/layer.rs` `anchor_for` is a bare bitflag map. The protocol centres an
    -- axis with neither edge anchored, leaving its width measurable; two anchored edges span it.
    anchor = { bottom = true },
    -- The surface is `SLIDE` taller and sits that much lower, so the card can rise from below
    -- without leaving the surface. `translate` is painted, not laid out, but the surface still
    -- clips it.
    margin = { bottom = theme.s(132, 90) - SLIDE },
    -- No `width`: the surface is the card, and the card is its content.
    height = theme.osd_height + SLIDE,
    -- Map until exit completes at `FALL_MS`; holding it for the entry's beat left an idle overlay.
    visible = util.linger(osd.visible, FALL_MS),
    child = column {
        height = theme.osd_height,
        -- `translate`, matching `components/modal.lua` and the notification cards, is paint-only
        -- (ADR-0149). The card is solved once; easing `margin` re-ran the solver every frame.
        translate = osd.visible:map(function(shown)
            return { y = shown and 0 or SLIDE }
        end),
        opacity = osd.visible:map(function(shown)
            return shown and 1 or 0
        end),
        -- A signal lets entry decelerate and exit accelerate with separate curves.
        animate = osd.visible:map(function(shown)
            local duration = shown and RISE_MS or FALL_MS
            return {
                opacity = { duration = duration, from = 0 },
                translate = {
                    duration = duration,
                    easing = shown and "OutCubic" or "InQuad",
                    from = { y = SLIDE },
                },
            }
        end),
        background = theme.GLASS,
        blur = true,
        radius = theme.radius.md,
        border_width = theme.border_width,
        border_color = theme.BORDER,
        children = { level_row, fact_row },
    },
}
