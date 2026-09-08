-- Mirrors PrivacyIndicator.qml: red circles appear only while their device is in use, and the group
-- disappears when none is.
--
-- `PrivacyState` began with `camera_users`; drawing two alerts that could never fire was worse than
-- drawing one. ADR-0137 added `microphone_users` and `screencast_users` from the same PipeWire
-- connection.
--
-- "In use" is PipeWire `Running`, not stream existence. A browser tab keeps a capture node open
-- between calls, so stream existence would leave the microphone circle lit.
--
-- Red ground with `text_contrast` picking the glyph colour against it, not a red label. The old
-- "cam:" readout reserved a 96px box even when nothing was recording.
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

-- The microphone is the one alert with two states. `PrivacyIndicator.qml` gives it a `warning`
-- ground and the struck-through glyph while the source is muted, `critical` and the plain glyph
-- while it is live, and shows it on `microphoneActive || microphoneMuted` -- so a muted microphone
-- keeps a circle to unmute from even when nothing is capturing.
local mic_muted = oblisk.audio:map(function(a)
    return a ~= nil and a.source_muted == true
end)
local mic_shown = computed({ oblisk.privacy, mic_muted }, function(p, muted)
    return users_of("microphone_users")(p) or muted
end)

-- The camera and the screencast are readouts: there is no "stop using my camera" to hang off a
-- click, and the mirror's own circles do nothing either. The microphone is the one the mirror
-- makes a control, and `audio:toggle_source_mute` is the command behind it -- muting the source
-- does not end the capture, so the circle stays up, which is the honest result.
return row {
    align_v = "Center",
    spacing = theme.spacing.sm,
    -- Invisible children leave the layout entirely, but this row would still earn a spacing gap.
    -- Hide the group too, or `left_side.lua` still gives the empty row that gap.
    -- `mic_shown` rather than `microphone_users`: the muted microphone is the one child that
    -- appears without a user, and a row hidden under it would take the child down with it.
    visible = computed({ oblisk.privacy, mic_shown }, function(p, mic)
        return mic or users_of("camera_users")(p) or users_of("screencast_users")(p)
    end),
    children = {
        alert(icons.camera, "camera_users"),
        icon_button(mic_muted:map(function(muted)
            return muted and icons.mic_off or icons.mic_on
        end), function()
            oblisk.audio:invoke("toggle_source_mute")
        end, {
            background = mic_muted:map(function(muted)
                return muted and theme.PEACH or theme.RED
            end),
            visible = mic_shown,
        }),
        alert(icons.screenshare, "screencast_users"),
    },
}
