-- A percentage bar using `width`'s "NN%" strings. Signals resolve before property parsing
-- (ADR-0044), so mapping to "45%" makes a live-width rect without progress-bar engine support.
-- The fill eases between two percents like `FillBar.qml`'s `Behavior on width` (ADR-0145).
--
-- `opts.motion` replaces that ease for a fill whose value moves while the motion is still running:
-- a volume key on repeat retargets every few frames, and an eased tween restarts from a standstill
-- each time, so the fill crawls. `theme.spring_tracking` is the answer those call sites pass.
-- Polled readouts and progress bars keep the ease; nothing retargets them mid-flight.
local theme = require("config.theme")

---@param opts? { motion?: Animation }
return function(signal, read, color, width, height, opts)
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
                -- Keep `math.floor`: Lua 5.4's `%d` raises on non-integral floats, and
                -- `audio.volume * 100` measured 45.00027 in a real session. A getter raise fails
                -- the
                -- re-resolve; the engine rolls back to the last good frame (ADR-0044), freezing the
                -- whole bar rather than just this meter.
                return string.format("%d%%", math.floor(math.max(0, math.min(100, pct)) + 0.5))
            end),
            height = "Fill",
            background = color,
            radius = theme.s(3, 2),
            animate = { width = (opts and opts.motion) or theme.animation_ms },
        } },
    }
end
