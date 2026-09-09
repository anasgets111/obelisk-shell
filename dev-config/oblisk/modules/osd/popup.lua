-- `OSDCard.qml`: bottom-centered glass card, switching its two layouts on whether the entry has a
-- level. `modules/osd/service.lua` decides what/when; this draws the entry. It fades and rises
-- like the mirror's `Behavior on opacity`/`y`, and the surface lingers mapped through the exit
-- (ADR-0146): the mirror unmaps its window at once, so its fade-out is never seen.
--
-- One `panel` with two `visible`-switched rows, not two panels. § 6 gives each surface its own
-- compositor identity; otherwise a volume change during a toggle would overlap at one position.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local glyph = require("components.glyph")
local meter = require("components.meter")
local osd = require("modules.osd.service")

-- How far the card travels on its way in, and how long each direction takes.
--
-- The mirror's `osdAnimationOffset` is 60px over `animationDuration * 1.5`, and that is a swoop.
-- This card is an acknowledgement of a key the user just pressed: they already know what happened,
-- so the card's whole job is to be legible by the time their eye reaches the bottom of the screen.
-- Travel is time spent not reading it, and a 60px swoop several dozen times a day is noise. A
-- short settle says "here is the readout" without saying "something has arrived" -- which is the
-- notification stack's line, and belongs to it alone (`components/notification_card.lua`).
local SLIDE = theme.s(12, 8)
-- Arriving decelerates over the same beat as the rest of the shell; leaving is quicker, because a
-- card whose two seconds are up is not news. The same split the notification cards use.
local RISE_MS = theme.animation_ms
local FALL_MS = theme.animation_fast_ms

-- Both layouts inset their content by the same amount.
local PADDING = theme.spacing.xl

-- `OSDCard.qml` sizes itself `isSlider ? osdSliderWidth : Math.max(osdToggleMinWidth,
-- _toggleWidth)`, where the second is measured off `labelText.implicitWidth`. Ours read that box
-- back through `geometry` and redid the arithmetic in Lua, because a `panel` could not be sized
-- from what it held. It can now, so the toggle row is content-sized and the card, the surface and
-- `set_size` follow it, with nothing settling a pass behind the words.
--
-- The mirror counts `spacingLg` twice where this layout has one gap. It reads as slack rather than
-- arithmetic, and a card cut exactly to its text looks tight, so the second one is asked for here
-- instead of falling out of a sum: half each side, where it does not move the contents off centre.
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
    -- `osdSliderWidth`. A track has no width of its own to measure, so this layout states one and
    -- the card takes it; the toggle layout beside it measures instead. Only one of the two is ever
    -- visible, and an invisible child takes no space, so the card is whichever is showing.
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
        -- The one thing on this card that is actually watched. A volume or brightness key on
        -- repeat moves the target every few frames, and an eased fill restarts from a standstill
        -- each time (ADR-0145's retarget), so it crawls along behind the percentage beside it. A
        -- spring keeps its velocity across the retarget (ADR-0154) and arrives with the number.
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
    -- No `width`: the card is these words. `osdToggleMinWidth` is the floor under them, and
    -- `align_h` is what centres the pair on a card the floor decided rather than the text.
    min_width = theme.osd_toggle_min,
    height = "Fill",
    align_h = "Center",
    align_v = "Center",
    spacing = theme.spacing.lg,
    -- The mirror counts this inset in `_toggleWidth` and this row never had it, so the words ran
    -- to the card's edge on the way to running past it.
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
    -- No `left`/`right`: § 6's anchors map directly to `zwlr_layer_surface_v1`
    -- (`renderer/src/wayland/layer.rs`'s `anchor_for` is a bare bitflag map), and the protocol
    -- centers an axis with neither edge anchored. That is also what leaves the width worth
    -- measuring: an axis with both its edges anchored is spanned whatever it asks for.
    anchor = { bottom = true },
    -- The surface is `SLIDE` taller than the card, and sits that much lower, so the card can rise
    -- into place from below its resting spot without leaving the surface. `translate` is painted,
    -- not laid out, but the surface still clips it, so the room is still needed.
    margin = { bottom = theme.s(132, 90) - SLIDE },
    -- No `width`: the surface is the card, and the card is its content.
    height = theme.osd_height + SLIDE,
    -- Mapped until the exit has played, and no longer: the card leaves at `FALL_MS`, so holding
    -- the surface for the entry's beat left an overlay on the compositor doing nothing.
    visible = util.linger(osd.visible, FALL_MS),
    child = column {
        height = theme.osd_height,
        -- `translate`, matching `components/modal.lua` and the notification cards: the travel is
        -- paint-only (ADR-0149), so the card is solved once and the rise costs no layout. Easing
        -- `margin` instead re-ran the solver over the card on every frame of every volume key.
        translate = osd.visible:map(function(shown)
            return { y = shown and 0 or SLIDE }
        end),
        opacity = osd.visible:map(function(shown)
            return shown and 1 or 0
        end),
        -- A signal, so the direction picks its own pace and curve: in decelerates, out accelerates.
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
