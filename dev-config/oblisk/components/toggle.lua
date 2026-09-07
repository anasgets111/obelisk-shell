-- On/off switch for the boolean writes in § 3.2, including `bluetooth:set_enabled`,
-- `network:set_wifi_enabled`, and `audio:set_muted`. The next capability snapshot is the only
-- readback.
-- Takes the raw signal plus `read`, as `components/meter.lua` does, because `oblisk.bluetooth`
-- pushes a table, not a bool; only the caller knows the field. `on_change` receives the flipped
-- value so the
-- caller can route it through `capability:invoke(...)` or local `state()`.
-- The thumb slides like `OToggle.qml`'s `Behavior on x`: the track is a `row` whose first child is
-- a spacer with a signal-bound, eased `width` (ADR-0145). `align_h` would snap and `margin`'s edge
-- table cannot carry a tween; a bare-number spacer width can.
local theme = require("config.theme")

local TRACK_WIDTH = theme.s(34, 28)
local TRACK_HEIGHT = theme.control.xs
local PAD = theme.s(2, 1)
local THUMB = TRACK_HEIGHT - 2 * PAD
local TRAVEL = TRACK_WIDTH - 2 * PAD - THUMB

local function read_bool(value, read)
    if value == nil then
        return false
    end
    local ok, result = pcall(read, value)
    return ok and result == true
end

return function(signal, read, on_change)
    local on = signal:map(function(value)
        return read_bool(value, read)
    end)
    return button {
        width = TRACK_WIDTH,
        height = TRACK_HEIGHT,
        on_click = function(_, mouse_button)
            if mouse_button ~= "left" then
                return
            end
            on_change(not read_bool(signal:get(), read))
        end,
        children = { row {
            width = "Fill",
            height = "Fill",
            radius = TRACK_HEIGHT / 2,
            padding = { top = PAD, right = PAD, bottom = PAD, left = PAD },
            background = on:map(function(v)
                return v and theme.GREEN or theme.SURFACE
            end),
            animate = { background = { duration = theme.animation_ms, easing = "OutCubic" } },
            children = {
                rect {
                    width = on:map(function(v)
                        return v and TRAVEL or 0
                    end),
                    height = "Fill",
                    animate = { width = { duration = theme.animation_ms, easing = "OutQuad" } },
                },
                rect {
                    width = THUMB,
                    height = THUMB,
                    radius = THUMB / 2,
                    background = theme.FG,
                    align_v = "Center",
                },
            },
        } },
    }
end
