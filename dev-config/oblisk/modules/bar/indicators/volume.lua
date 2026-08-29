-- Mirrors Volume.qml.
--
-- Volume, real as of docs/adr/0053 decision 3. `audio.volume` is the cube root of PipeWire's
-- `channelVolumes`, which is the number `wpctl` and `pactl` show and the one a user recognises as
-- "the volume"; the raw linear value would read 3% where this reads 30%.
-- § 5.2 item 5's own worked example (`"audio-volume-high"`), which makes this the natural place to
-- prove `icon` resolves a theme name and re-resolves it when the signal pushes. `:map` rather than
-- `label`: a nil `audio` should resolve to no icon at all, and `label`'s "--" placeholder would go
-- to the theme lookup as if it were a name.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local pill = require("components.pill")
local meter = require("components.meter")

local volume_icon = oblisk.audio:map(function(a)
    if a == nil then
        return ""
    end
    if a.muted then
        return "audio-volume-muted"
    end
    local percent = (a.volume or 0) * 100
    if percent < 34 then
        return "audio-volume-low"
    elseif percent < 67 then
        return "audio-volume-medium"
    end
    return "audio-volume-high"
end)

-- The icon and the readout are one `button`: clicking mutes, clicking again unmutes. This is the
-- § 3.2 audio write path's live proof, and `toggle_mute` is the one action of the seven that takes
-- no arguments, so it is also the one a click can express without inventing a gesture this engine
-- does not have. Volume steps and a device picker want a scroll wheel and a popup list, and the
-- config vocabulary for both is a separate slice.
--
-- Nothing here updates the pill optimistically. The new state arrives back through PipeWire's own
-- `Props` event, which is what makes a mute from this bar and a mute from `wpctl` look identical.
local volume_module = pill({
    icon { name = volume_icon, size = 14 },
    -- The readout is the button and the icon beside it is not, which is the shape
    -- `brightness_module` already uses: a `button` is a stacking container (`scene.rs` positions
    -- its children independently rather than in a line), so it holds one child. Wrapping a `row`
    -- in it to get both drew the icon's box and none of its pixels.
    button {
        height = 18,
        align_v = "Center",
        on_click = function()
            oblisk.audio:invoke("toggle_mute")
        end,
        children = { cell(util.label(oblisk.audio, function(a)
            if a.muted then
                return "muted"
            end
            return string.format("vol %d%%", math.floor(a.volume * 100 + 0.5))
        end), theme.FG) },
    },
    meter(oblisk.audio, function(a)
        return a.muted and 0 or a.volume * 100
    end, theme.MAUVE),
})

return volume_module
