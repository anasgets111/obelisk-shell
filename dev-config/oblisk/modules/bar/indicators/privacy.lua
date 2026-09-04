-- Mirrors PrivacyIndicator.qml: three red circles, each present only while the thing it names is
-- actually in use, and the whole group absent when nothing is.
--
-- All three exist now. `PrivacyState` was one field, `camera_users`, until ADR-0137 added
-- `microphone_users` and `screencast_users` off the same PipeWire connection, which is what this
-- file was waiting for -- drawing two alerts that could never fire was worse than drawing one.
--
-- "In use" is PipeWire's own `Running`, not the existence of a stream: a browser tab holds a
-- capture node open between calls, and a microphone circle lit by that would never go out.
--
-- Red ground rather than a red word. `text_contrast` picks the glyph colour against it, so the
-- alert reads at a glance and does not need "cam:" spelled out beside it -- which is what this
-- module used to do, in a 96px box, permanently reserved whether anything was recording or not.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local icon_button = require("components.icon_button")

local function users_of(field)
    return function(p)
        return #((p or {})[field] or {}) > 0
    end
end

local function alert(glyph, field, on_activate)
    return icon_button(glyph, on_activate, {
        background = theme.RED,
        visible = util.shown_when(oblisk.privacy, users_of(field)),
    })
end

-- The camera and the screencast are readouts: there is no "stop using my camera" to hang off a
-- click, and the mirror's own circles do nothing either. The microphone is the one the mirror
-- makes a control, and `audio:toggle_source_mute` is the command behind it -- muting the source
-- does not end the capture, so the circle stays up, which is the honest result.
return row {
    align_v = "Center",
    spacing = theme.spacing.sm,
    -- On the group as well as on each circle: an invisible child leaves the layout entirely, but
    -- this row itself would still earn a spacing gap from `left_side.lua` while holding nothing.
    visible = util.shown_when(oblisk.privacy, function(p)
        return users_of("camera_users")(p) or users_of("microphone_users")(p) or users_of("screencast_users")(p)
    end),
    children = {
        alert(icons.camera, "camera_users"),
        alert(icons.mic_on, "microphone_users", function()
            oblisk.audio:invoke("toggle_source_mute")
        end),
        alert(icons.screenshare, "screencast_users"),
    },
}
