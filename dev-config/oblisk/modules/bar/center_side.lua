-- Mirrors `CenterSide.qml`, which layers rather than swaps: `ActiveWindow` is anchored
-- unconditionally and the media `Loader` fills the same box on top of it, so the spectrum plays
-- over the window title instead of replacing it.
--
-- This used to show one or the other, which meant the title vanished for as long as anything was
-- playing and the caption the centre zone exists for was the first thing lost.
--
-- A `rect` is the stacking parent -- `modules/global/modal_host.lua` uses the same shape for its
-- scrim and click catcher. The zone stays content-sized because invisible children contribute no
-- size (`resolve_sizes` in scene.rs), so with nothing playing it is exactly the title's width, and
-- with the overlay up it is `theme.center_zone_width`, the mirror's `parent.width / 3`.
local media = require("modules.bar.indicators.media")
local window_title_module = require("modules.bar.indicators.active_window")

-- `playbackAvailable: !!active && active.playbackState !== Stopped`. A stopped player is still on
-- the bus with its metadata intact, so testing only for a player's existence left the spectrum up
-- over a track nothing was going to play.
local playback_available = oblisk.mpris:map(function(m)
    local player = ((m and m.players) or {})[1]
    return player ~= nil and player.play_state ~= "Stopped"
end)

-- The zone itself stays a `row` with one child. `modules/bar/init.lua` lays the three zones out
-- side by side and `socket.rs`'s overflow test measures a zone by summing its children, so a
-- stacking parent used directly as the zone reads as two modules laid end to end.
return row {
    height = "Fill",
    align_h = "Center",
    align_v = "Center",
    children = { rect {
        height = "Fill",
        align_v = "Center",
        children = {
            row {
                height = "Fill",
                align_h = "Center",
                align_v = "Center",
                children = { window_title_module },
            },
            row {
                height = "Fill",
                align_h = "Center",
                align_v = "Center",
                visible = playback_available,
                children = { media },
            },
        },
    } },
}
