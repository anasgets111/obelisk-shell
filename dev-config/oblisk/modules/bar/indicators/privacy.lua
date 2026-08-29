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
        return "cam: " .. ((users[1] or {}).app_name or "?")
    end), theme.RED) }, "#45253aff") },
}
