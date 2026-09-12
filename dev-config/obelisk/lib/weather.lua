-- Mirrors `Services/SystemInfo/WeatherService.qml` and `WeatherCodes.qml`: one hourly reading of
-- open-meteo.
--
-- Coordinates come from the timezone, not the mirror's `ipapi.co`, which answers 429 on its free
-- tier. It is the timezone's city, not yours; write `weather_location` to pin one instead.
--
-- `process.run("curl", ...)` replaces `XMLHttpRequest`, decoded in `exit_cb` because only that
-- callback knows the body is complete. `docs/roadmap.md` routes weather through an HTTP CLI rather
-- than a capability, and open-meteo needs no key, so the whole service is two requests and a cache.
--
-- ## One deadline instead of two timers
--
-- The mirror runs an hourly `updateTimer` and a separate `retryTimer` at 2s then 4s. There is no
-- config-owned timer here, so both collapse into `next_attempt`, a wall-clock second compared on
-- `obelisk.system`'s 1 Hz push. Success schedules the next hour; the first two failures schedule
-- seconds away, and the third gives up until the hour, which is the mirror's own `_retryCount >= 2`
-- rule. `modules/global/launcher/currency.lua` refreshes the same way.
--
-- Nothing is owed until `obelisk.storage` has pushed, because the stored timestamp is what says
-- whether a launch owes a request at all, and its `0` default would spend one every time. That is a
-- level test on the signal rather than a handler watching for its first push: a push is an edge,
-- and an in-place reload installs the new handler after the capability has already pushed, so an
-- edge gate would arm on a cold start and never again.
local icons = require("config.icons")
local store = require("lib.store")
local util = require("lib.util")

local weather = {}

-- `refreshInterval: 60 * 60 * 1000`, and `_handleRequestError`'s `2000 * Math.pow(2, n - 1)`.
local REFRESH_SECONDS = 3600
local RETRY_SECONDS = { 2, 4 }
-- `refresh()` ignores a click inside 30s of the last reading.
local MANUAL_FLOOR_SECONDS = 30

local GEOCODING_URL = "https://geocoding-api.open-meteo.com/v1/search?count=5&name="

-- `WeatherCodes.qml`, verbatim. WMO codes are sparse, so this is a lookup rather than a range test,
-- and an unlisted one is the mirror's own "Unknown" rather than a nil a caller has to test for. The
-- forecast cards draw the icon; `modules/bar/indicators/date_time.lua` draws both halves.
local CODES = {
    [0] = { icon = "☀️", desc = "Clear sky" },
    [1] = { icon = "🌤️", desc = "Mainly clear" },
    [2] = { icon = "⛅", desc = "Partly cloudy" },
    [3] = { icon = "☁️", desc = "Overcast" },
    [45] = { icon = "🌫️", desc = "Fog" },
    [48] = { icon = "🌫️", desc = "Depositing rime fog" },
    [51] = { icon = "🌦️", desc = "Drizzle: Light" },
    [53] = { icon = "🌦️", desc = "Drizzle: Moderate" },
    [55] = { icon = "🌧️", desc = "Drizzle: Dense" },
    [56] = { icon = "🌧️❄️", desc = "Freezing Drizzle: Light" },
    [57] = { icon = "🌧️❄️", desc = "Freezing Drizzle: Dense" },
    [61] = { icon = "🌦️", desc = "Rain: Slight" },
    [63] = { icon = "🌧️", desc = "Rain: Moderate" },
    [65] = { icon = "🌧️", desc = "Rain: Heavy" },
    [66] = { icon = "🌧️❄️", desc = "Freezing Rain: Light" },
    [67] = { icon = "🌧️❄️", desc = "Freezing Rain: Heavy" },
    [71] = { icon = "🌨️", desc = "Snow fall: Slight" },
    [73] = { icon = "🌨️", desc = "Snow fall: Moderate" },
    [75] = { icon = "❄️", desc = "Snow fall: Heavy" },
    [77] = { icon = "❄️", desc = "Snow grains" },
    [80] = { icon = "🌦️", desc = "Rain showers: Slight" },
    [81] = { icon = "🌧️", desc = "Rain showers: Moderate" },
    [82] = { icon = "⛈️", desc = "Rain showers: Violent" },
    [85] = { icon = "🌨️", desc = "Snow showers: Slight" },
    [86] = { icon = "❄️", desc = "Snow showers: Heavy" },
    [95] = { icon = "⛈️", desc = "Thunderstorm: Slight or moderate" },
    [96] = { icon = "⛈️🧊", desc = "Thunderstorm with slight hail" },
    [99] = { icon = "⛈️🧊", desc = "Thunderstorm with heavy hail" },
}

local UNKNOWN = { icon = "❓", desc = "Unknown" }

---`WeatherCodes.get`.
---@param code integer|nil
---@return { icon: string, desc: string }
function weather.info(code)
    return CODES[code or -1] or UNKNOWN
end

-- `LockContent.qml`'s `weatherIcon`: the lock screen has one line for weather, so the same codes
-- collapse into six glyphs rather than twenty-eight pictures.
local GLYPH_BUCKETS = {
    [icons.weather_sunny] = { 0, 1 },
    [icons.weather_fog] = { 45, 48 },
    [icons.weather_rain] = { 51, 53, 55, 61, 63, 65, 80, 81, 82 },
    [icons.weather_snow] = { 56, 57, 66, 67, 71, 73, 75, 77, 85, 86 },
    [icons.weather_storm] = { 95, 96, 99 },
}

local GLYPHS = {}
for glyph, codes in pairs(GLYPH_BUCKETS) do
    for _, code in ipairs(codes) do
        GLYPHS[code] = glyph
    end
end

---@param code integer|nil
---@return string
function weather.glyph(code)
    return GLYPHS[code or -1] or icons.weather_cloud
end

-- The reading, straight off `lib/store.lua` so a restart inside the hour draws before any request.
-- `Settings.state.weather` holds the same fields; `weather_location` is the mirror's
-- `Settings.data.weatherLocation`, written once by the IP lookup and read back for its place name.
weather.code = store.weather_code
weather.temperature = store.weather_temperature
weather.daily = store.weather_daily
weather.updated_at = store.weather_updated_at
weather.location = store.weather_location

-- `hasError` is not cached: it describes this session's last attempt, and a stale one read back at
-- launch would show "Weather Unavailable" over a forecast that is on screen.
weather.failed = state("weather_failed", false)

-- `state`, not module locals: a local is rebuilt by every in-place reload, while the `process.run`
-- child it is guarding is not. An unrelated config save would clear the guard under a live request
-- and let the next tick start a second one. `state` has exactly the child's lifetime, surviving a
-- reload with an unchanged seed (ADR-0044 decision 5) and dying with the generation that gets the
-- child reaped anyway. `0` in `next_attempt` means nothing is scheduled yet.
local in_flight = state("weather_fetching", false)
local retries = state("weather_retries", 0)
local next_attempt = state("weather_next_attempt", 0)

-- Monotonic, not wall: a retry is a duration this session owns, and setting the clock must not
-- park the next attempt an hour out (ADR-0202). The stored-freshness deadline below is the other
-- kind and stays wall, so the two are never compared against the same reading.
local function schedule(seconds)
    next_attempt:set(((obelisk.system:get() or {}).monotonic or 0) + seconds)
end

local function failed()
    weather.failed:set(true)
    local attempt = retries:get() + 1
    local backoff = RETRY_SECONDS[attempt]
    -- Out of short retries: wait for the hour, and reset so that hour's cycle gets its own two,
    -- which is what `_startRefreshCycle` does before every scheduled check.
    retries:set(backoff and attempt or 0)
    schedule(backoff or REFRESH_SECONDS)
end

---@param url string
---@param apply fun(data: table)
local function http_get(url, apply)
    in_flight:set(true)
    local body = {}
    process.run("curl", { "-fsS", "--max-time", "5", url }, function(line, stream)
        if stream == "stdout" then
            body[#body + 1] = line
        end
    end, function(code)
        in_flight:set(false)
        local data = code == 0 and json.decode(table.concat(body)) or nil
        -- `apply` raising is the mirror's `throw new Error("No weather data")`, caught into the same
        -- retry as a dead socket. A half-applied reading cannot result: both writers write last.
        if type(data) ~= "table" or not pcall(apply, data) then
            failed()
        end
    end)
end

local function fetch_weather(latitude, longitude)
    http_get(string.format(
        "https://api.open-meteo.com/v1/forecast?latitude=%s&longitude=%s&current_weather=true"
        .. "&timezone=auto&forecast_days=10&past_days=1"
        .. "&daily=temperature_2m_max,temperature_2m_min,weathercode",
        latitude, longitude), function(data)
        local current = data.current_weather
        assert(type(current) == "table", "no current_weather")
        store:set("weather_code", math.floor(current.weathercode or -1))
        store:set("weather_temperature", math.floor((current.temperature or 0) + 0.5))
        store:set("weather_daily", data.daily)
        store:set("weather_updated_at", os.time())
        weather.failed:set(false)
        retries:set(0)
        schedule(REFRESH_SECONDS)
    end)
end

-- `_fetchGeoLocation`. The zone's last segment is its city.
local function fetch_location(zone)
    local city = zone:match("([^/]+)$"):gsub("_", "%%20")
    http_get(GEOCODING_URL .. city, function(data)
        for _, place in ipairs(data.results or {}) do
            -- Load-bearing: unmatched, `Europe/Kiev` returns a Kiev in the Urals.
            if place.timezone == zone then
                store:set("weather_location", {
                    latitude = place.latitude,
                    longitude = place.longitude,
                    place_name = place.country and (place.name .. ", " .. place.country) or place.name,
                    timezone = zone,
                })
                fetch_weather(place.latitude, place.longitude)
                return
            end
        end
        error("no place in " .. zone)
    end)
end

-- Re-read each cycle so a machine that travels re-resolves; geocode only when the zone changed.
local function fetch()
    in_flight:set(true)
    local zone = ""
    process.run("timedatectl", { "show", "-p", "Timezone", "--value" }, function(line, stream)
        if stream == "stdout" then
            zone = util.trim(line)
        end
    end, function(code)
        if code ~= 0 or zone == "" then
            in_flight:set(false)
            failed()
            return
        end
        local known = store.weather_location:get()
        if known and known.timezone == zone and known.latitude and known.longitude then
            fetch_weather(known.latitude, known.longitude)
        else
            fetch_location(zone)
        end
    end)
end

---`refresh()`: the widget's button. A reading younger than 30s is left alone, as the mirror does.
function weather.refresh()
    if in_flight:get() then
        return
    end
    if os.time() - (store.weather_updated_at:get() or 0) < MANUAL_FLOOR_SECONDS then
        return
    end
    retries:set(0)
    fetch()
end

obelisk.system:on_change(function(system)
    if not system or in_flight:get() or obelisk.storage:get() == nil then
        return
    end
    local due = next_attempt:get()
    if due == 0 then
        -- Nothing scheduled yet, so the deadline is the stored reading's own hour. That stamp
        -- outlived the session, so it is wall time and only `time` can be compared with it.
        local stale_at = (store.weather_updated_at:get() or 0) + REFRESH_SECONDS
        if system.time < stale_at then
            return
        end
    elseif system.monotonic < due then
        return
    end
    fetch()
end)

---`timeAgo`.
---@param at integer|nil
---@param now integer|nil
---@return string
function weather.time_ago(at, now)
    if not at or at == 0 or not now then
        return ""
    end
    local seconds = math.max(0, now - at)
    for _, step in ipairs({ { 86400, "%dd ago" }, { 3600, "%dh ago" }, { 60, "%dm ago" } }) do
        if seconds >= step[1] then
            return string.format(step[2], seconds // step[1])
        end
    end
    return "just now"
end

---The weekday an ISO `YYYY-MM-DD` from `daily.time` names. `os.date` needs seconds, and the
---engine's clock is the only other date source a config has.
---@param iso string|nil
---@return string
function weather.weekday(iso)
    local year, month, day = tostring(iso or ""):match("^(%d%d%d%d)-(%d%d)-(%d%d)$")
    if not year then
        return ""
    end
    -- Noon, so a timezone shift cannot move the date across midnight and name the wrong weekday.
    local named = os.date("%a",
        os.time({ year = tonumber(year), month = tonumber(month), day = tonumber(day), hour = 12 }))
    ---@cast named string
    return named
end

return weather
