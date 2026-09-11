-- Mirrors `Launcher/CurrencyProvider.qml`: "50 usd to egp" becomes one row whose Enter copies the
-- converted amount.
--
-- The mirror's `XMLHttpRequest` is `process.run("curl", ...)`, decoded in `exit_cb` because only
-- that callback knows the body is complete. `docs/roadmap.md` routes currency through an HTTP CLI
-- rather than a capability, and the engine has no HTTP to offer either way.
--
-- Rates live in `lib/store.lua`, as the mirror keeps them in `Settings.state.currency`, so a
-- restart inside the day reuses them.
--
-- No config-owned timer exists, so staleness is checked on `obelisk.system`'s 1 Hz push: one
-- integer compare a second. `modules/bar/indicators/updates.lua` seeds from storage's first push;
-- this needs no seed branch, because the tick a second later does the same work.
--
-- Dropped: `ratesLive` and its "FX-STATIC" badge. `rates` starts empty, and a query whose target is
-- missing claims nothing, so no row reaches the eye before the first fetch lands.
local util = require("lib.util")
local store = require("lib.store")

local M = {}

-- `refreshInterval: 86400000`, in seconds.
local REFRESH_SECONDS = 86400
local URL = "https://cdn.jsdelivr.net/npm/@fawazahmed0/currency-api@latest/v1/currencies/usd.json"

local SYMBOLS = {
    ["$"] = "usd",
    ["€"] = "eur",
    ["£"] = "gbp",
    ["¥"] = "jpy",
    ["₹"] = "inr",
    ["₿"] = "btc",
}

-- `_getFlag`'s `specials`: three countries whose code is not their first two letters, and the
-- cryptocurrencies that have a sign instead of a flag.
local FLAGS = {
    eur = "eu", gbp = "gb", usd = "us",
    btc = "₿", eth = "Ξ", ltc = "Ł", doge = "Ð", xrp = "✕",
    ada = "₳", sol = "₴", dot = "●", usdt = "₮", usdc = "₵",
}

-- `(?:to|in|->|=>|=)`. Matched against a lowercased query, so the word forms need no case class.
-- The earliest match wins, which is what the mirror's lazy `(.+?)` picks.
local SEPARATORS = { "%s+to%s*", "%s+in%s*", "%s*%->%s*", "%s*=>%s*", "%s*=%s*" }

---A currency code is three to five letters, or one of the symbols.
---@param token string
---@return string|nil
local function currency(token)
    if SYMBOLS[token] then
        return SYMBOLS[token]
    end
    if token:match("^%a+$") and #token >= 3 and #token <= 5 then
        return token:lower()
    end
    return nil
end

---@return string|nil source, string|nil target
local function split(text)
    local first, last
    for _, separator in ipairs(SEPARATORS) do
        local from, to = text:find(separator)
        if from and (not first or from < first) then
            first, last = from, to
        end
    end
    if not first then
        return nil, nil
    end
    return text:sub(1, first - 1), text:sub(last + 1)
end

---`_parseSource`: "50 usd", "$50", "50$", a bare "$", and a bare "usd" once a separator precedes it.
---@return number|nil amount, string|nil code
local function parse_source(text, allow_implicit_amount)
    text = util.trim(text)
    local amount, token = text:match("^(%d+%.?%d*)%s*(.+)$")
    if amount then
        local code = currency(token)
        return code and tonumber(amount) or nil, code
    end
    for symbol, code in pairs(SYMBOLS) do
        if text:sub(1, #symbol) == symbol then
            local rest = util.trim(text:sub(#symbol + 1))
            if rest == "" then
                return 1, code
            end
            return rest:match("^%d+%.?%d*$") and tonumber(rest) or nil, code
        end
    end
    if allow_implicit_amount then
        local code = currency(text)
        if code then
            return 1, code
        end
    end
    return nil, nil
end

---Unicode regional indicators start 127397 code points after ASCII letters.
local function flag(code)
    local country = FLAGS[code] or code:sub(1, 2)
    if not country:match("^%a%a$") then
        return country:upper()
    end
    local first, second = country:upper():byte(1, 2)
    return utf8.char(0x1F1E6 + first - 65, 0x1F1E6 + second - 65)
end

---`_formatLastUpdated`, against `date_time.lua`'s fixed twelve-hour clock.
local function updated_text(at, now)
    if not at or at == 0 then
        return ""
    end
    local same_day = os.date("%Y%j", at) == os.date("%Y%j", now)
    return "Updated " .. os.date(same_day and "%I:%M %p" or "%b %d, %I:%M %p", at)
end

-- `_requesting`: one request in flight, and a failure leaves the stored rates alone.
local requesting = false

local function fetch()
    if requesting then
        return
    end
    requesting = true
    local body = {}
    process.run("curl", { "-fsS", "--max-time", "5", URL }, function(line, stream)
        if stream == "stdout" then
            body[#body + 1] = line
        end
    end, function(code)
        requesting = false
        if code ~= 0 then
            return
        end
        local decoded = json.decode(table.concat(body))
        local rates = decoded and decoded.usd
        if type(rates) ~= "table" then
            return
        end
        -- `data.usd["usd"] = 1.0`: the base is absent from its own table.
        rates.usd = 1.0
        store:set("currency_rates", rates)
        store:set("currency_updated_at", os.time())
    end)
end

-- Until storage pushes, `currency_updated_at` reads its `0` default, and fetching on that would
-- spend a request the stored rates were about to answer.
local storage_ready = false

obelisk.storage:on_change(function()
    storage_ready = true
end)

obelisk.system:on_change(function(system)
    local now = system and system.time
    if not (storage_ready and now) then
        return
    end
    if now - (store.currency_updated_at:get() or 0) >= REFRESH_SECONDS then
        fetch()
    end
end)

---@param query string
---@param rates table<string, number>|nil
---@param updated_at integer|nil
---@return LauncherRow|nil
function M.claims(query, rates, updated_at)
    local text = util.trim(query):lower()
    local source, target = split(text)
    local amount, from = parse_source(source or text, source ~= nil)
    if not (amount and from) then
        return nil
    end
    local to
    if target and target ~= "" then
        to = currency(target)
        if not to then
            return nil
        end
    else
        -- `parsed.f === "egp" ? "usd" : "egp"`.
        to = from == "egp" and "usd" or "egp"
    end
    if from == to then
        return nil
    end
    local from_rate = tonumber((rates or {})[from])
    local to_rate = tonumber((rates or {})[to])
    if not from_rate or not to_rate or from_rate <= 0 or to_rate <= 0 then
        return nil
    end
    local converted = (amount / from_rate) * to_rate
    if converted ~= converted or converted == math.huge then
        return nil
    end
    local result
    if math.abs(converted) < 0.01 and converted ~= 0 then
        result = string.format("%.6g", converted)
    else
        local decimals = converted >= 100 and 2 or (converted >= 1 and 4 or 6)
        result = util.thousands(string.format("%." .. decimals .. "f", converted))
    end
    return {
        kind = "currency",
        badge = "FX",
        hint = "Enter to copy result",
        icon = flag(from),
        icon_is_text = true,
        title = string.format("%s %s → %s %s", string.format("%.14g", amount), from:upper(), result, to:upper()),
        subtitle = updated_text(updated_at, os.time()),
        payload = result,
    }
end

return M
