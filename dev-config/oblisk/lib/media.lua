-- Whether a player is showing a video rather than playing a song.
--
-- Nothing in Rust answers this and nothing should (ADR-0137). The Supervisor publishes the two
-- facts a config cannot reach, `url` and `desktop_entry`, and stops there: the four lists below are
-- taste, they go stale on their own schedule, and the right list for one person is wrong for the
-- next. This is where the rest of this config's taste already lives.
--
-- Mirrors `MediaService.qml`'s `_isVideo`, order included, because the order is load-bearing:
--
--   1. A known video application is a video whatever it happens to be playing.
--   2. Anything that is not a browser is not a video. A music player publishing a `.mp4` URL is
--      still a music player.
--   3. A browser is judged by its URL, music sites checked *first*: `music.youtube.com/watch?v=`
--      contains `youtube.com/watch`, so checking the video list first calls every album a film.
--   4. Then the video sites, then the file extension.
--
-- `desktop_entry` before `identity`: the first is the player's `.desktop` basename and is stable,
-- the second is a display string a player may localise or decorate.
local media = {}

local VIDEO_APPS = {
    "mpv", "vlc", "celluloid", "io.github.celluloid_player.celluloid", "org.gnome.totem", "smplayer",
    "mplayer", "haruna", "kodi", "io.github.iwalton3.jellyfin-media-player", "jellyfin", "plex",
    "freetube", "stremio", "clapper", "dragon", "hypnotix",
}

local BROWSERS = { "firefox", "zen", "chrome", "chromium", "brave", "vivaldi", "edge", "opera" }

local AUDIO_SITES = {
    "music.youtube.com", "spotify.com", "soundcloud.com", "music.apple.com", "deezer.com",
    "tidal.com", "bandcamp.com", "pocketcasts.com", "audible.com", "mixcloud.com", "tunein.com",
}

local VIDEO_SITES = {
    "youtube.com/watch", "youtu.be", "netflix.com", "primevideo.com", "vimeo.com", "twitch.tv",
    "hulu.com", "disneyplus.com", "crunchyroll.com", "max.com", "hbomax.com", "udemy.com",
    "coursera.org", "pluralsight.com", "nebula.tv", "odysee.com", "dailymotion.com", "tv.apple.com",
    "tiktok.com", "instagram.com/reel", "meet.google.com", "teams.microsoft.com", "teams.live.com",
    "zoom.us", "discord.com", "meet.jit.si", "whereby.com", "webex.com", "gotomeeting.com",
}

local VIDEO_EXTENSIONS = {
    mp4 = true, mkv = true, webm = true, avi = true, mov = true, m4v = true, mpeg = true,
    mpg = true, wmv = true, flv = true,
}

-- `find(..., true)` for a plain substring search: every entry above is a literal, and a `.` in
-- `youtu.be` would otherwise match any character.
local function matches_any(haystack, needles)
    for _, needle in ipairs(needles) do
        if haystack:find(needle, 1, true) then
            return true
        end
    end
    return false
end

--- Whether one `PlayerState` is showing a video.
--- @param player table? one entry of `oblisk.mpris`'s `players`
--- @return boolean
function media.is_video(player)
    if player == nil then
        return false
    end
    local name = (player.desktop_entry ~= "" and player.desktop_entry or player.identity or ""):lower()
    if matches_any(name, VIDEO_APPS) then
        return true
    end
    if not matches_any(name, BROWSERS) then
        return false
    end
    local url = (player.url or ""):lower()
    if url == "" or matches_any(url, AUDIO_SITES) then
        return false
    end
    if matches_any(url, VIDEO_SITES) then
        return true
    end
    -- The extension of the last path segment, stopping at a query string or a fragment.
    local extension = url:match("%.([%a%d][%a%d][%a%d]?[%a%d]?[%a%d]?)[?#]") or url:match("%.([%a%d]+)$")
    return extension ~= nil and VIDEO_EXTENSIONS[extension] == true
end

--- A signal that is true while any player is playing a video. What an idle module reads to decide
--- whether the screen may blank.
media.video_playing = oblisk.mpris:map(function(m)
    for _, player in ipairs((m or {}).players or {}) do
        if player.play_state == "Playing" and media.is_video(player) then
            return true
        end
    end
    return false
end)

return media
