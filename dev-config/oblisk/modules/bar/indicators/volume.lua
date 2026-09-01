-- Mirrors Volume.qml: a circle showing one glyph, which grows to show the percentage while the
-- pointer is on it.
--
-- The expansion is two properties, not an animation. `hover` is a signal the engine writes
-- (ADR-0062), so the button's `width` reads it and the readout's `visible` reads it, and the
-- control is wide exactly while it is hovered. The mirror tweens the width over 147ms; this snaps,
-- because nothing in the engine interpolates a property between two resolves.
--
-- Click mutes. The mirror puts mute on the middle button and a drag on the left, and a drag needs
-- pointer motion tracked into a value, which § 5.1's press-carries-a-rect model does not do.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local ui_state = require("lib.ui_state")

local SLOT = "volume"
local hovered = hover(SLOT)

local function volume_glyph(a)
    if a == nil then
        return icons.vol_muted
    end
    if a.muted then
        return icons.vol_muted
    end
    local level = (a.volume or 0) * 100
    if level < 1 then
        return icons.vol_zero
    elseif level < 33 then
        return icons.vol_low
    elseif level < 66 then
        return icons.vol_mid
    end
    return icons.vol_high
end

-- Muted sits on the content ground rather than the control ground, which is the mirror's own way of
-- saying "this is off" without changing the glyph's colour as well as its shape.
local ground = oblisk.audio:map(function(a)
    return (a ~= nil and a.muted) and theme.GLASS_CONTENT or theme.GLASS_CONTROL
end)

return button {
    width = hovered:map(function(is_hovered)
        return is_hovered and theme.volume_expanded_width or theme.item_width
    end),
    height = theme.item_height,
    align_v = "Center",
    hover = hovered,
    radius = theme.item_radius,
    background = computed({ oblisk.audio, hovered }, function(a, is_hovered)
        if is_hovered then
            return theme.GLASS_CONTROL_HOVER
        end
        return (a ~= nil and a.muted) and theme.GLASS_CONTENT or theme.GLASS_CONTROL
    end),
    border_width = theme.border_width,
    border_color = hovered:map(function(is_hovered)
        return is_hovered and theme.GLASS_BORDER_HOVER or theme.GLASS_BORDER
    end),
    on_click = function(_, mouse_button)
        if mouse_button ~= "left" then
            return
        end
        oblisk.audio:invoke("toggle_mute")
        ui_state.arm_osd("volume")
    end,
    children = { row {
        width = "Fill",
        height = "Fill",
        align_h = "Center",
        align_v = "Center",
        spacing = theme.spacing.xs,
        children = {
            cell(oblisk.audio:map(volume_glyph), ground:map(theme.text_contrast), theme.icon.lg, { align_v = "Center" }),
            cell(util.label(oblisk.audio, function(a)
                if a.muted then
                    return "muted"
                end
                return string.format("%d%%", math.floor(a.volume * 100 + 0.5))
            -- The percentage exists only while the control is wide enough for it. This was declared
            -- on the row instead, which held the glyph too, so the collapsed control was an empty
            -- circle and the volume was the one indicator on the bar showing nothing at all. A
            -- hidden child costs no width and no spacing either: `layout::scene`'s row arm sums
            -- footprints over the visible children and multiplies spacing by that count.
            end), theme.text_contrast(theme.GLASS_CONTROL_HOVER), theme.font.sm, { align_v = "Center", visible = hovered }),
        },
    } },
}
