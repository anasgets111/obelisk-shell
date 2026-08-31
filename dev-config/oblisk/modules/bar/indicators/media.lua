-- Mirrors MediaIndicator.qml's readable half: the transport state as a glyph and the track beside
-- it. The mirror draws a live cava spectrum behind that, which needs a fragment shader and a
-- per-frame push; neither exists here and neither is worth an ADR to add for a bar widget.
--
-- No pill. This sits in the centre zone, where the mirror paints on the bar itself.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")

-- A codepoint budget, not a box, and `lib/util.lua`'s `truncate` says why this one module gets to
-- count characters. The centre zone is content-sized between two `Fill` sides, so its midpoint is
-- the bar's midpoint only while its content is its own width; a fixed box put the glyph at the box
-- edge and the title in the box middle, with a hand of empty space between them.
--
-- 44 is about 300px at `font.sm`, which the zone test's own snapshots feed it past.
local TITLE_LIMIT = 44

local function player_of(m)
    return m and (m.players or {})[1]
end

return row {
    height = theme.item_height,
    align_v = "Center",
    spacing = theme.spacing.sm,
    children = {
        cell(oblisk.mpris:map(function(m)
            local player = player_of(m)
            return (player ~= nil and player.play_state == "Playing") and icons.play or icons.pause
        end), theme.ACCENT, theme.icon.md, { align_v = "Center" }),
        cell(computed({ oblisk.mpris, oblisk.audio }, function(m, a)
            local player = player_of(m)
            if not player then
                return "no media"
            end
            local title = player.title or player.identity or "?"
            if player.artist and player.artist ~= "" then
                title = title .. " -- " .. player.artist
            end
            if a and a.muted then
                return title .. " (muted)"
            end
            return util.truncate(title, TITLE_LIMIT)
        end), theme.FG, theme.font.sm, { align_v = "Center" }),
    },
}
