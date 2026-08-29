-- Mirrors MediaIndicator.qml.
--
-- The one module that reads two capabilities at once, and the reason `computed` exists: a media
-- widget wants the player from `mpris` and the output volume from `audio`, and neither signal can
-- see the other.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local pill = require("components.pill")

return pill({ cell(computed({ oblisk.mpris, oblisk.audio }, function(m, a)
    local player = m and (m.players or {})[1]
    if not player then
        return "no media"
    end
    local mark = player.play_state == "Playing" and ">" or "||"
    local title = util.truncate(player.title or player.identity or "?", 16)
    if player.artist and player.artist ~= "" then
        title = title .. " -- " .. util.truncate(player.artist, 10)
    end
    if a and a.muted then
        return mark .. " " .. title .. " (muted)"
    end
    return mark .. " " .. title
end), theme.ACCENT) })
