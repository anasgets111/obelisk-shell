-- Mirrors Volume.qml: a glyph circle expands to a slider on hover. Drag and wheel set volume;
-- middle-click mutes, and right-click opens the audio panel.
--
-- `hover` is an engine signal (ADR-0062); `width` and `visible` use it. Width and ground ease over
-- the mirror's 147ms through `animate` (ADR-0145); percentage appears at once because `visible` is
-- not a property a tween carries.
--
-- `components/slider.lua` matches `Slider`; its accent fill runs under glyph and percentage
-- only while expanded.
local theme = require("config.theme")
local util = require("lib.util")
local ui_state = require("lib.ui_state")
local cell = require("components.cell")
local glyph = require("components.glyph")
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

-- Muted uses the content ground, the mirror's way to say "this is off" without changing glyph
-- colour.
local ground = computed({ obelisk.audio, hovered }, function(a, is_hovered)
    if is_hovered then
        return theme.GLASS_CONTROL_HOVER
    end
    return muted(a) and theme.GLASS_CONTENT or theme.GLASS_CONTROL
end)

-- Mirror `trackColor`: muted makes the fill inactive, so 60% reads as a grey bar, not purple
-- "loud".
local fill = obelisk.audio:map(function(a)
    return muted(a) and theme.INACTIVE or theme.ACCENT
end)

-- `Volume.qml`'s `foregroundAt`: contrast against the fill once it reaches the glyph or percentage,
-- otherwise against the ground. Their thresholds are the first and last quarters of the expanded
-- control.
local function foreground_past(threshold)
    return computed({ obelisk.audio, hovered, ground, fill }, function(a, is_hovered, ground_color, fill_color)
        if is_hovered and volume(a) >= threshold then
            return theme.text_contrast(fill_color)
        end
        return theme.text_contrast(ground_color)
    end)
end

return slider {
    name = "volume_pending",
    signal = obelisk.audio,
    read = volume,
    on_commit = function(fraction)
        obelisk.audio:invoke("set_volume", fraction)
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
    animate = { width = theme.animation_ms, background = theme.animation_ms, border_color = theme.animation_ms },
    border_width = theme.border_width,
    border_color = computed({ hovered, panel_open }, function(is_hovered, open)
        if open then
            return theme.ACCENT
        end
        return is_hovered and theme.GLASS_BORDER_HOVER or theme.GLASS_BORDER
    end),
    on_click = function(rect, mouse_button)
        if mouse_button == "middle" then
            obelisk.audio:invoke("toggle_mute")
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
            glyph(obelisk.audio:map(util.volume_glyph), foreground_past(0.25), theme.icon.lg, { align_v = "Center" }),
            -- A hidden percentage costs no width or spacing: `layout::scene` sums visible child
            -- footprints and multiplies spacing by their count.
            cell(util.label(obelisk.audio, function(a)
                if a.muted then
                    return "muted"
                end
                return string.format("%d%%", math.floor(a.volume * 100 + 0.5))
            end), foreground_past(0.75), theme.font.sm, { align_v = "Center", visible = hovered }),
        },
    } },
}
