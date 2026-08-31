-- Mirrors PrivacyIndicator.qml, one circle of its three: a red glyph while something has the
-- camera open, absent otherwise.
--
-- The mirror also draws a microphone and a screen-share circle. `PrivacyState` in
-- `supervisor/src/privacy/controller.rs` is one field, `camera_users`, so there is nothing behind
-- the other two and drawing them would mean drawing two alerts that can never fire.
--
-- Red ground rather than a red word. `text_contrast` picks the glyph colour against it, so the
-- alert reads at a glance and does not need "cam:" spelled out beside it -- which is what this
-- module used to do, in a 96px box, permanently reserved whether anything was recording or not.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local icon_button = require("components.icon_button")

-- A read-only alert: `on_activate` is nil, so this draws as a `row` rather than a `button`. There
-- is no command to hang off a click -- the mirror's microphone circle toggles the source mute, and
-- § 4.1's audio row has neither the command nor a field to read the result back from.
return icon_button(icons.camera, nil, {
    background = theme.RED,
    visible = util.shown_when(oblisk.privacy, function(p)
        return #((p or {}).camera_users or {}) > 0
    end),
})
