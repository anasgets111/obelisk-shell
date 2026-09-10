-- Rust deliberately does not answer this (ADR-0137). The Supervisor provides no video flag; config
-- uses `url`, `desktop_entry`, and fallback `identity`; four lists may go stale independently.
-- Mirrors `MediaService.qml`'s `_isVideo`; order is load-bearing:
--   1. A known video application is video whatever it plays.
--   2. A non-browser is not video; a music player publishing `.mp4` remains a music player.
--   3. For browsers, check music sites first: `music.youtube.com/watch?v=` contains
--      `youtube.com/watch`, so video-first would classify every album as a film.
--   4. Then check video sites, then the extension.
-- Prefer `desktop_entry` to `identity`: the former is the stable `.desktop` basename; the latter
-- is a display string that may be localised or decorated.
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
    mp4 = true,
    mkv = true,
    webm = true,
    avi = true,
    mov = true,
    m4v = true,
    mpeg = true,
    mpg = true,
    wmv = true,
    flv = true,
}

-- `find(..., true)` keeps entries literal; otherwise the `.` in `youtu.be` matches any character.
local function matches_any(haystack, needles)
    for _, needle in ipairs(needles) do
        if haystack:find(needle, 1, true) then
            return true
        end
    end
    return false
end

--- Video classification for one `PlayerState`.
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

--- Video classification from one `oblisk.mpris` payload. Pure and payload-based so `lib/idle.lua`
--- can use the value from `on_change` instead of a possibly stale `computed` in its callback.
--- @param m table? `oblisk.mpris`'s payload
--- @return boolean
function media.is_playing_video(m)
    for _, player in ipairs((m or {}).players or {}) do
        if player.play_state == "Playing" and media.is_video(player) then
            return true
        end
    end
    return false
end

media.video_playing = oblisk.mpris:map(media.is_playing_video)

return media
