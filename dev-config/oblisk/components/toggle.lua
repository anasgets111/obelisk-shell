-- On/off switch for the boolean writes in § 3.2, including `bluetooth:set_enabled`,
-- `network:set_wifi_enabled`, and `audio:set_muted`. The next capability snapshot is the only
-- readback.
-- Takes the raw signal plus `read`, as `components/meter.lua` does, because `oblisk.bluetooth`
-- pushes a table, not a bool; only the caller knows the field. `on_change` receives the flipped
-- value so the
-- caller can route it through `capability:invoke(...)` or local `state()`.
-- Move the thumb with signal-bound `align_h` (`"Start"`/`"End"`), like `components/volume.lua`'s
-- icon name, not a pixel `margin` offset. § 5.1 and `layout/node/mod.rs`'s `resolve_properties`
-- prove Signals for base non-structural properties, but not for Signals nested in `margin` fields.
local theme = require("config.theme")

local TRACK_WIDTH = theme.s(34, 28)
local TRACK_HEIGHT = theme.control.xs
local THUMB = TRACK_HEIGHT - theme.s(4, 3)

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
        children = { rect {
            width = "Fill",
            height = "Fill",
            radius = TRACK_HEIGHT / 2,
            padding = { top = theme.s(2, 1), right = theme.s(2, 1), bottom = theme.s(2, 1), left = theme.s(2, 1) },
            background = on:map(function(v)
                return v and theme.GREEN or theme.SURFACE
            end),
            children = { rect {
                width = THUMB,
                height = THUMB,
                radius = THUMB / 2,
                background = theme.FG,
                align_v = "Center",
                align_h = on:map(function(v)
                    return v and "End" or "Start"
                end),
            } },
        } },
    }
end
