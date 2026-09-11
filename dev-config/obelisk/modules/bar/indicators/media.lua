-- Mirrors `MediaIndicator.qml`: `ShaderEffect` runs `Shaders/frag/cava_bars.frag` over
-- `CavaService.values`; a pointer target opens the media panel. It has no glyph or track text;
-- `CenterSide.qml` draws the window title underneath, and the track belongs to the panel.
--
-- A play glyph and "title -- artist" put the track caption where the mirror puts the spectrum and
-- hid the window title while anything was playing.
--
-- ## Why the bars are flat
--
-- Cava needs 30fps frames. The engine has no shader or canvas node; its nine node types draw no
-- waveform.
-- `rect`s can carry levels, but per-frame pushes are the first such path here; the bar logs
-- `exceeded the 5ms CPU budget` failures. Not attempted; see the ADR.
--
-- Flat is not a placeholder shape, though: `cava_bars.frag`'s `h = max(minHeightPx, level)` draws
-- exactly this row when every level is zero, which is what the mirror shows while cava has no data.
local theme = require("config.theme")
local ui_state = require("lib.ui_state")
local media_panel = require("modules.bar.panels.media_panel")

-- `barCount` is cava's configured 256. Fewer here because each is a real node, not a shader lane.
-- At rest it reads as the same fine rule; 256 static children buy nothing until they carry levels.
local BARS = 48

-- `gapPx: borderWidthThin` and `minHeightPx: borderWidthMedium`.
local GAP = theme.border_width
local BAR_HEIGHT = theme.border_width_medium

-- `barColor: playing ? activeMedium : activeSubtle`, eased by the mirror's `ColorTransition`.
local tint = obelisk.mpris:map(function(m)
    for _, player in ipairs((m and m.players) or {}) do
        if player.play_state == "Playing" then
            return theme.ACCENT_MEDIUM
        end
    end
    return theme.ACCENT_SUBTLE
end)

local children = {}
for index = 1, BARS do
    children[index] = rect {
        width = "Fill",
        height = BAR_HEIGHT,
        align_v = "End",
        background = tint,
    }
end

-- The mirror opens on hover and closes on `animationSlow` after the pointer leaves the trigger and
-- card.
-- Here click toggles the panel, matching every other indicator; `Accessible.onPressAction` is the
-- mirror's keyboard equivalent.
return button {
    width = theme.center_zone_width,
    height = "Fill",
    on_click = function(rect_, mouse_button)
        if mouse_button ~= "left" then
            return
        end
        ui_state.toggle_panel(media_panel.kind, rect_)
    end,
    children = { row {
        width = "Fill",
        height = "Fill",
        align_v = "End",
        -- `anchors.margins: spacingXs` inside the zone.
        padding = { top = theme.spacing.xs, right = theme.spacing.xs, bottom = theme.spacing.xs, left = theme.spacing.xs },
        spacing = GAP,
        animate = { background = { duration = theme.animation_ms, easing = "OutCubic" } },
        children = children,
    } },
}
