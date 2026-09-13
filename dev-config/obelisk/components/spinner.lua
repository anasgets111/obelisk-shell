-- `Components/OSpinner.qml`. A theme icon, not the refresh glyph: femtovg fills a rotated glyph's
-- outline solid, while an icon rotates as a texture. `animate` follows `visible`, so a hidden
-- spinner stops asking for a frame every frame (ADR-0152).
local theme = require("config.theme")

local TURN = { rotate = { duration = 1000, easing = "Linear", keyframes = { 0, 360 }, loops = "Infinite" } }

---@param visible Signal
---@param size integer
return function(visible, size)
    return icon {
        name = "view-refresh-symbolic",
        size = size,
        foreground = theme.DIM,
        align_h = "Center",
        visible = visible,
        animate = visible:map(function(on)
            return on and TURN or {}
        end),
    }
end
