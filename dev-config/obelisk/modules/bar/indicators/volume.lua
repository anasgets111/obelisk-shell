-- Mirrors Volume.qml: a glyph circle expands to a slider on hover. Drag and wheel set volume;
-- middle-click mutes, and right-click opens the audio panel.
--
-- `hover` is an engine signal (ADR-0062); `width` and `visible` follow it or a held drag. Width and
-- ground ease over the mirror's 147ms through `animate` (ADR-0145); percentage appears at once
-- because `visible` is not a property a tween carries.
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
local SPLIT = 1
local hovered = hover(SLOT)
local dragging = state("volume_dragging", false)
-- The slider's held value, `-1` when none; the readout follows a drag before PipeWire answers.
local held = state("volume_pending", -1)
local expanded = computed({ hovered, dragging }, function(h, d)
    return h or d
end)
local panel_open = ui_state.panel_showing("audio")

local function muted(a)
    return a ~= nil and a.muted
end

local function volume(a)
    return a and a.volume
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
local headroom = obelisk.audio:map(function(a)
    return muted(a) and theme.INACTIVE or theme.RED
end)

local level = computed({ obelisk.audio, held }, function(a, h)
    return h >= 0 and h or volume(a) or 0
end)

-- `Volume.qml`'s `foregroundAt`: contrast against the fill once it reaches the glyph or percentage,
-- otherwise against the ground. Their thresholds are the first and last quarters of the expanded
-- control.
local function foreground_past(threshold)
    return computed({ level, expanded, ground, fill, headroom }, function(v, open, ground_color, fill_color, over)
        if open and v / util.MAX_VOLUME >= threshold then
            return theme.text_contrast(threshold * util.MAX_VOLUME > SPLIT and over or fill_color)
        end
        return theme.text_contrast(ground_color)
    end)
end

return slider {
    name = "volume_pending",
    signal = obelisk.audio,
    read = volume,
    on_commit = function(value)
        obelisk.audio:invoke("set_volume", value)
    end,
    max = util.MAX_VOLUME,
    split_at = SPLIT,
    pending = held,
    headroom_color = headroom,
    width = expanded:map(function(is_expanded)
        return is_expanded and theme.volume_expanded_width or theme.item_width
    end),
    height = theme.item_height,
    align_v = "Center",
    hover = hovered,
    dragging = dragging,
    radius = theme.item_radius,
    background = ground,
    color = fill,
    fill_visible = expanded,
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
            cell(computed({ obelisk.audio, level }, function(a, v)
                if volume(a) == nil then
                    return "--"
                elseif a.muted then
                    return "muted"
                end
                return string.format("%d%%", math.floor(v * 100 + 0.5))
            end), foreground_past(0.75), theme.font.sm, { align_v = "Center", visible = expanded }),
        },
    } },
}
