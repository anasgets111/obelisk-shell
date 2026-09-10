-- Mirrors PrivacyIndicator.qml: red circles appear only while their device is in use, and the group
-- disappears when none is.
--
-- `PrivacyState` began with `camera_users`; ADR-0137 added `microphone_users` and
-- `screencast_users` from the same PipeWire connection. Alerts for fields that never fired
-- were worse than one.
--
-- "In use" is PipeWire `Running`, not stream existence. A browser tab can keep a capture node open
-- between calls, so existence would leave the microphone circle lit.
--
-- Red ground uses `text_contrast` for the glyph colour, not a red label. The old "cam:" readout
-- reserved a 96px box even when nothing was recording.
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

-- The microphone has two states. `PrivacyIndicator.qml` uses `warning` ground and a struck-through
-- glyph when muted, `critical` and the plain glyph when live, and shows it on
-- `microphoneActive || microphoneMuted`. A muted microphone therefore stays available to unmute.
local mic_muted = oblisk.audio:map(function(a)
    return a ~= nil and a.source_muted == true
end)
local mic_shown = computed({ oblisk.privacy, mic_muted }, function(p, muted)
    return users_of("microphone_users")(p) or muted
end)

-- Camera and screencast are readouts; their circles do nothing. The microphone is the control.
-- It uses `audio:toggle_source_mute`; muting does not end capture, so the circle stays up.
return row {
    align_v = "Center",
    spacing = theme.spacing.sm,
    -- Invisible children leave layout, but the row still contributes spacing. Hide the group, or
    -- `left_side.lua` still gives the empty row that gap.
    -- Use `mic_shown`, not `microphone_users`: a muted microphone appears without a user.
    -- Hiding the row would hide it too.
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
