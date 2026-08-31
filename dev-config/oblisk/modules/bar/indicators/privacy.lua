-- Mirrors PrivacyIndicator.qml.
--
-- Only rendered when something is actually using the camera, which is the whole point: a privacy
-- indicator that is always visible is not an indicator.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local pill = require("components.pill")

return row {
    height = "Fill",
    align_v = "Center",
    visible = util.shown_when(oblisk.privacy, function(p)
        return #(p.camera_users or {}) > 0
    end),
    children = { pill({ cell(util.label(oblisk.privacy, function(p)
        local users = p.camera_users or {}
        -- Truncated like every other readout here. An app name arrives from whichever process
        -- opened the camera and has no length this config controls, and the pill is only ever up
        -- while a camera is live, which is the worst moment for the bar to reflow.
        return "cam: " .. util.truncate((users[1] or {}).app_name or "?", 10)
    end), theme.RED) }, "#45253aff") },
}
