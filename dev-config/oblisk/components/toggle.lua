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

-- `OToggle.qml` derives the whole switch from one number: the track is
-- `controlHeightFor(size) * scaleSmall` tall, which `control.xs` now equals, and
-- `round(_trackHeight * 2.3)` wide, with `_thumbPadding: max(3, round(_trackHeight * 0.12))`.
-- Deriving the width keeps the thumb's travel proportional when the scale moves; the fixed 34 it
-- replaced left a 24px-tall track with almost no room for the thumb to slide in.
local TRACK_HEIGHT = theme.control.xs
local TRACK_WIDTH = math.floor(TRACK_HEIGHT * 2.3 + 0.5)
local PAD = math.max(3, math.floor(TRACK_HEIGHT * 0.12 + 0.5))
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
            -- `OToggle.qml`'s `_trackOn`/`_trackOff`: `activeFull` and `glassControlColor`. The
            -- track was green over flat surface, which read as a status light rather than a switch
            -- and put a second accent in a shell whose every other lit control is mauve.
            background = on:map(function(v)
                return v and theme.with_opacity(theme.ACCENT, theme.opacity.full) or theme.GLASS_CONTROL
            end),
            -- `border.color: glassBorderColor`, the same hairline every other glass control carries.
            border_width = theme.border_width,
            border_color = theme.GLASS_BORDER,
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
                    -- `borderMedium`, which separates the thumb from a lit track it nearly matches.
                    border_width = theme.border_width,
                    border_color = theme.BORDER_SUBTLE,
                    align_v = "Center",
                },
            },
        } },
    }
end
