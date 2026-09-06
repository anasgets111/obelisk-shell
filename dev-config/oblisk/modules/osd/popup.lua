-- `OSDCard.qml` without animation: bottom-centered glass card, switching its two layouts on whether
-- the entry has a level. `modules/osd/service.lua` decides what/when; this draws the entry.
--
-- One `panel` with two `visible`-switched rows, not two panels. § 6 gives each surface its own
-- compositor identity; otherwise a volume change during a toggle would overlap at one position.
local theme = require("config.theme")
local cell = require("components.cell")
local meter = require("components.meter")
local osd = require("modules.osd.service")

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
        cell(read("glyph"), theme.ACCENT, theme.font.xxl, { align_v = "Center" }),
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
                children = { cell(read("glyph"), theme.ACCENT, theme.font.xl, { align_v = "Center" }) },
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
    margin = { bottom = theme.s(132, 90) },
    width = theme.osd_width,
    height = theme.osd_height,
    visible = osd.visible,
    child = column {
        width = "Fill",
        height = "Fill",
        background = theme.GLASS,
        radius = theme.radius.md,
        border_width = theme.border_width,
        border_color = theme.BORDER,
        children = { level_row, fact_row },
    },
}
