-- A bar graphic, which is what a percentage actually wants to look like. The trick is that `width`
-- accepts a "NN%" string and a signal resolves before the property is parsed (ADR-0044), so a
-- signal that maps to "45%" is a live-width rect with no engine support for progress bars at all.
local theme = require("config.theme")

return function(signal, read, color, width, height)
    return row {
        width = width or theme.s(40, 30),
        height = height or theme.s(6, 4),
        align_v = "Center",
        background = theme.SURFACE,
        radius = theme.s(3, 2),
        children = { rect {
            width = signal:map(function(value)
                if value == nil then
                    return "0%"
                end
                local ok, pct = pcall(read, value)
                if not ok or pct == nil then
                    return "0%"
                end
                -- `math.floor`, and it is load-bearing rather than tidy. Lua 5.4's `%d` raises on a
                -- float with no integer representation, and `audio.volume * 100` is 45.00027 on a
                -- real session. A raise inside a signal getter fails the whole re-resolve, and the
                -- engine then rolls the scene back and keeps the last good frame (ADR-0044), so one
                -- bad number here freezes every module on the bar, not just this meter.
                return string.format("%d%%", math.floor(math.max(0, math.min(100, pct)) + 0.5))
            end),
            height = "Fill",
            background = color,
            radius = theme.s(3, 2),
        } },
    }
end
