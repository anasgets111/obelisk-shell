-- Mirrors Volume.qml: a circle showing one glyph, which grows into a slider while the pointer is
-- on it. Drag along it to set the volume, roll the wheel over it to step, middle-click to mute,
-- right-click for the audio panel.
--
-- The expansion is two properties, not an animation. `hover` is a signal the engine writes
-- (ADR-0062), so the control's `width` reads it and the readout's `visible` reads it, and the
-- control is wide exactly while it is hovered. The mirror tweens the width over 147ms; this snaps,
-- because nothing in the engine interpolates a property between two resolves.
--
-- The whole control is a `components/slider.lua`, as the mirror's whole control is a `Slider`: the
-- accent fill runs under the glyph and the percentage, and shows only while expanded, since a
-- collapsed circle has no length to fill.
local theme = require("config.theme")
local util = require("lib.util")
local ui_state = require("lib.ui_state")
local cell = require("components.cell")
local slider = require("components.slider")

local SLOT = "volume"
local hovered = hover(SLOT)
local panel_open = ui_state.panel_showing("audio")

local function muted(a)
    return a ~= nil and a.muted
end

local function volume(a)
    return (a and a.volume) or 0
end

-- Muted sits on the content ground rather than the control ground, which is the mirror's own way of
-- saying "this is off" without changing the glyph's colour as well as its shape.
local ground = computed({ oblisk.audio, hovered }, function(a, is_hovered)
    if is_hovered then
        return theme.GLASS_CONTROL_HOVER
    end
    return muted(a) and theme.GLASS_CONTENT or theme.GLASS_CONTROL
end)

-- The mirror's `trackColor`: the fill goes inactive when muted, so a muted control at 60% reads as
-- a grey bar rather than a purple one saying "loud".
local fill = oblisk.audio:map(function(a)
    return muted(a) and theme.INACTIVE or theme.ACCENT
end)

-- `Volume.qml`'s `foregroundAt`: a glyph is read against whatever is behind its centre, which is
-- the fill once the fill has reached it and the ground before that. The glyph sits in the first
-- quarter of the expanded control and the percentage in the last, so those are the two
-- thresholds.
local function foreground_past(threshold)
    return computed({ oblisk.audio, hovered, ground, fill }, function(a, is_hovered, ground_color, fill_color)
        if is_hovered and volume(a) >= threshold then
            return theme.text_contrast(fill_color)
        end
        return theme.text_contrast(ground_color)
    end)
end

return slider {
    name = "volume_pending",
    signal = oblisk.audio,
    read = volume,
    on_commit = function(fraction)
        oblisk.audio:invoke("set_volume", fraction)
    end,
    width = hovered:map(function(is_hovered)
        return is_hovered and theme.volume_expanded_width or theme.item_width
    end),
    height = theme.item_height,
    align_v = "Center",
    hover = hovered,
    radius = theme.item_radius,
    background = ground,
    color = fill,
    fill_visible = hovered,
    border_width = theme.border_width,
    border_color = computed({ hovered, panel_open }, function(is_hovered, open)
        if open then
            return theme.ACCENT
        end
        return is_hovered and theme.GLASS_BORDER_HOVER or theme.GLASS_BORDER
    end),
    on_click = function(rect, mouse_button)
        if mouse_button == "middle" then
            oblisk.audio:invoke("toggle_mute")
        elseif mouse_button == "right" then
            ui_state.toggle_panel("audio", rect)
        end
    end,
    children = { row {
        width = "Fill",
        height = "Fill",
        align_h = "Center",
        align_v = "Center",
        spacing = theme.spacing.xs,
        children = {
            cell(oblisk.audio:map(util.volume_glyph), foreground_past(0.25), theme.icon.lg, { align_v = "Center" }),
            -- The percentage exists only while the control is wide enough for it. A hidden child
            -- costs no width and no spacing either: `layout::scene`'s row arm sums footprints over
            -- the visible children and multiplies spacing by that count.
            cell(util.label(oblisk.audio, function(a)
                if a.muted then
                    return "muted"
                end
                return string.format("%d%%", math.floor(a.volume * 100 + 0.5))
            end), foreground_past(0.75), theme.font.sm, { align_v = "Center", visible = hovered }),
        },
    } },
}
