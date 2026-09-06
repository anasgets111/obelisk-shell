-- Mirrors CenterSide.qml: media while playing, the focused window title otherwise.
--
-- One at a time. QML uses a `Loader` active only while `MediaService.playbackAvailable`; here
-- `visible` has the same layout effect because invisible children contribute no size
-- (`resolve_sizes` in scene.rs).
--
-- Content-sized between two `Fill` sides keeps its midpoint at the bar's midpoint. The retired
-- arithmetic is recorded in `modules/bar/init.lua`.
local theme = require("config.theme")
local media = require("modules.bar.indicators.media")
local window_title_module = require("modules.bar.indicators.active_window")

-- `nil` is the pre-push state, not "nothing is playing"; both mean no player to show. The title
-- takes the zone until `mpris` pushes its first snapshot, which makes the bar look right for the
-- second before that snapshot lands.
local function has_player(m)
    return m ~= nil and (m.players or {})[1] ~= nil
end

local media_slot = row {
    height = "Fill",
    align_v = "Center",
    visible = oblisk.mpris:map(has_player),
    children = { media },
}

local title_slot = row {
    height = "Fill",
    align_v = "Center",
    visible = oblisk.mpris:map(function(m)
        return not has_player(m)
    end),
    children = { window_title_module },
}

return row {
    height = "Fill",
    align_h = "Center",
    align_v = "Center",
    spacing = theme.spacing.sm,
    children = { media_slot, title_slot },
}
