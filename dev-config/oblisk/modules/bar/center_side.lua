-- Mirrors CenterSide.qml, which shows the media widget while something is playing and the focused
-- window's title otherwise.
--
-- One at a time, and that is a width constraint rather than a style choice. This zone is 20% of the
-- bar, about 380px on a 1920px output, and the two modules together are wider than that. QML gets
-- the same effect from a `Loader` that is only `active` while `MediaService.playbackAvailable`;
-- here it is `visible`, which is the same thing to the layout because an invisible child
-- contributes nothing to its parent's size (`resolve_sizes` in scene.rs).
local media = require("modules.bar.indicators.media")
local window_title_module = require("modules.bar.indicators.active_window")

-- `nil` is the pre-push state, not "nothing is playing", and both answer the same way here: no
-- player to show. The title takes the zone until `mpris` says otherwise, which is also what makes
-- the bar look right for the second before the first snapshot lands.
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
    width = "13%",
    height = "Fill",
    align_h = "Center",
    align_v = "Center",
    spacing = 8,
    children = { media_slot, title_slot },
}
