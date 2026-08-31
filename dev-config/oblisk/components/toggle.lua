-- An on/off switch, the shape every boolean write in § 3.2 wants: `bluetooth:set_enabled`,
-- `network:set_wifi_enabled`, `audio:set_muted`, and so on all take one `bool` and nothing reads it
-- back except the capability's own next snapshot.
--
-- Takes the raw signal and a `read` function rather than a bare boolean signal, the same split
-- `components/meter.lua` already makes for the same reason: `oblisk.bluetooth` pushes a table, not
-- a bool, so the component needs to know which field to read and the caller is the only one who
-- knows that. `on_change` takes the flipped value rather than writing it itself, so a caller can
-- route it through `capability:invoke(...)` or a local `state()`, whichever it is.
--
-- The thumb moves via `align_h` on a stacking (non-row) parent -- `"Start"` or `"End"`, chosen by
-- a signal the same way `components/volume.lua`'s icon name is -- rather than a pixel offset in
-- `margin`, because § 5.1's blanket "any property accepts a Signal" is proven for base properties
-- like `align_h` (`layout/node/mod.rs`'s `resolve_properties` resolves every non-structural
-- property uniformly) and unproven for a `Signal` nested inside a `margin` table's own fields.
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
