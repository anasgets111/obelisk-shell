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

-- `Theme.qml`'s `osdAnimationOffset`, and its `animationDuration * 1.5` for the rise.
local SLIDE = theme.s(60, 40)
local RISE_MS = theme.animation_ms * 1.5

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
    width = "Fill",
    height = "Fill",
    align_v = "Center",
    spacing = theme.spacing.lg,
    padding = { left = theme.spacing.xl, right = theme.spacing.xl },
    visible = osd.entry:map(function(e)
        return e.level ~= nil
    end),
    children = {
        glyph(read("glyph"), theme.ACCENT, theme.font.xxl, { align_v = "Center" }),
        meter(osd.entry, function(e)
            return e.level or 0
        end, osd.entry:map(function(e)
            return e.color or theme.ACCENT
        end), "Fill", theme.osd_track),
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
    width = "Fill",
    height = "Fill",
    align_h = "Center",
    align_v = "Center",
    spacing = theme.spacing.lg,
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
    -- centers an axis with neither edge anchored. Explicit `width`/`height` are required because
    -- `bottom` alone anchors neither full axis.
    anchor = { bottom = true },
    -- The surface is `SLIDE` taller than the card, and sits that much lower, so the card can rise
    -- into place from below its resting spot without leaving the surface.
    margin = { bottom = theme.s(132, 90) - SLIDE },
    width = theme.osd_width,
    height = theme.osd_height + SLIDE,
    visible = util.linger(osd.visible, RISE_MS),
    child = column {
        width = "Fill",
        height = theme.osd_height,
        margin = osd.visible:map(function(shown)
            return { top = shown and 0 or SLIDE }
        end),
        opacity = osd.visible:map(function(shown)
            return shown and 1 or 0
        end),
        animate = {
            opacity = { duration = theme.animation_ms, from = 0 },
            margin = { duration = RISE_MS, easing = "OutCubic", from = { top = SLIDE } },
        },
        background = theme.GLASS,
        radius = theme.radius.md,
        border_width = theme.border_width,
        border_color = theme.BORDER,
        children = { level_row, fact_row },
    },
}
