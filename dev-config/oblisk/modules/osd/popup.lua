-- The OSD card, `OSDCard.qml` minus its animation: a glass card at the bottom centre, with the two
-- layouts that file has, switched by whether the entry carries a level. What to show and when is
-- `modules/osd/service.lua`'s; this file only draws the entry it holds.
--
-- One `panel`, two stacked rows switched by `visible`, rather than two panels: § 6 gives every
-- surface its own compositor identity, and a volume change while a toggle card is still up would
-- otherwise be two overlapping surfaces fighting over one screen position instead of one replacing
-- the other.
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

-- The slider layout: glyph in the accent colour, a track that fills, a bold readout.
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

-- The toggle layout: the glyph in an accent-tinted tile, a bold line beside it, the pair centred.
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
        -- A box's `align_*` place the box in its parent, not its child in it, so the glyph is centred
        -- by a filling row inside the tile, the battery pill's own arrangement.
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
    -- No `left`/`right`: § 6's anchor booleans pass straight through to `zwlr_layer_surface_v1`
    -- (`renderer/src/wayland/layer.rs`'s `anchor_for` is a bare bitflag map, nothing more), and the
    -- protocol centers an axis with neither of its edges anchored. Explicit `width`/`height` are
    -- required because `bottom` alone doesn't anchor both edges of either axis.
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
