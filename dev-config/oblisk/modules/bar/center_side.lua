-- Mirrors CenterSide.qml, which shows the media widget while something is playing and the focused
-- window's title otherwise.
--
-- One at a time. QML gets that from a `Loader` that is only `active` while
-- `MediaService.playbackAvailable`; here it is `visible`, which is the same thing to the layout
-- because an invisible child contributes nothing to its parent's size (`resolve_sizes` in
-- scene.rs).
--
-- Content-sized between two `Fill` sides, so it is exactly as wide as whatever is showing and its
-- midpoint is the bar's midpoint whatever that turns out to be. `modules/bar/init.lua` has the
-- arithmetic that retired.
local theme = require("config.theme")
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
    height = "Fill",
    align_h = "Center",
    align_v = "Center",
    spacing = theme.spacing.sm,
    children = { media_slot, title_slot },
}
