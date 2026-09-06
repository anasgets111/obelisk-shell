-- Mirrors MediaIndicator.qml's readable half: transport glyph and track. Its live cava spectrum
-- needs a fragment shader and per-frame pushes, neither available or worth an ADR for a bar widget.
--
-- No pill, matching the mirror's centre-zone treatment.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local glyph = require("components.glyph")

-- A codepoint budget, not a box. `lib/util.lua`'s `truncate` is appropriate because the centre zone
-- is content-sized between two `Fill` sides; a fixed box would put the glyph at its edge and the
-- title at its middle, leaving empty space between them.
--
-- 44 is about 300px at `font.sm`, which the zone test's snapshots exceed.
local TITLE_LIMIT = 44

local function player_of(m)
    return m and (m.players or {})[1]
end

return row {
    height = theme.item_height,
    align_v = "Center",
    spacing = theme.spacing.sm,
    children = {
        glyph(oblisk.mpris:map(function(m)
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
