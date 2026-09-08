-- Mirrors `MediaIndicator.qml`, which is a spectrum and nothing else: a `ShaderEffect` running
-- `Shaders/frag/cava_bars.frag` over `CavaService.values`, plus a pointer target that opens the
-- media panel. It carries no glyph and no track text -- `CenterSide.qml` draws the window title
-- underneath it, and the track itself belongs to the panel.
--
-- This drew a play glyph and "title -- artist" instead, which put the track caption where the
-- mirror puts the spectrum and hid the window title for as long as anything was playing.
--
-- ## Why the bars are flat
--
-- Levels need cava's 30fps frames, and the engine has no shader or canvas node: nine node types,
-- none of which draws a waveform. Bars are `rect`s, which can carry a level, but a per-frame push
-- driving them is the first per-frame path this config would have and the bar already logs
-- `exceeded the 5ms CPU budget` failures. Not attempted here; see the ADR.
--
-- Flat is not a placeholder shape, though: `cava_bars.frag`'s `h = max(minHeightPx, level)` draws
-- exactly this row when every level is zero, which is what the mirror shows while cava has no data.
local theme = require("config.theme")
local ui_state = require("lib.ui_state")
local media_panel = require("modules.bar.panels.media_panel")

-- `barCount` is cava's own configured 256. Fewer here because each is a real node rather than a
-- shader lane: at rest the row reads as the same fine rule either way, and 256 static children on
-- the bar buys nothing until they carry levels.
local BARS = 48

-- `gapPx: borderWidthThin` and `minHeightPx: borderWidthMedium`.
local GAP = theme.border_width
local BAR_HEIGHT = theme.border_width_medium

-- `barColor: playing ? activeMedium : activeSubtle`, eased by the mirror's `ColorTransition`.
local tint = oblisk.mpris:map(function(m)
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

-- The mirror opens the panel on hover and closes it on an `animationSlow` timer once the pointer
-- has left both the trigger and the card. Click instead: every other indicator here toggles its
-- panel on a click, and `Accessible.onPressAction` is the mirror's own keyboard equivalent of it.
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
