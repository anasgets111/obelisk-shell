-- Pure helpers with no nodes, kept out of `components/`; the mirrored config keeps widgets in
-- `Components/` and functions in `Services/Utils/`.
local util = {}

-- Capability signals are `nil` until the first Supervisor snapshot, and payload readers may raise.
-- Map `nil` to "--" and reader errors to "!", leaving each module one line for its payload.
function util.label(signal, read)
    return signal:map(function(value)
        if value == nil then
            return "--"
        end
        local ok, text = pcall(read, value)
        if not ok then
            return "!"
        end
        return text or "--"
    end)
end

-- Human-readable words for `obelisk.battery.state`'s seven UPower names, shared by the pill tooltip,
-- power menu, and lock screen.
-- `PendingCharge` matters when a laptop with `charge_control_end_threshold` set sits plugged in at
-- the limit; the old `charging` boolean called it "discharging", opposite to the cable state.
-- Neither pending state *means* the limit, though. UPower says only "plugged in and not charging",
-- and this machine reports it for a few seconds at every plug-in while the asus driver still reads
-- `Not charging`; read as a limit it claimed one of 70 had been hit at 37%. The number is not
-- available to say otherwise: UPower 1.91 carries `ChargeEndThreshold` but reports 80 with
-- `ChargeThresholdEnabled` false here, against sysfs's 70, because asusd writes it behind UPower.
-- So both phrases say what UPower observed and leave the cause to whoever set the limit.
local BATTERY_PHRASES = {
    Charging = "charging",
    Discharging = "discharging",
    Empty = "empty",
    FullyCharged = "full",
    PendingCharge = "waiting to charge",
    PendingDischarge = "waiting to discharge",
    Unknown = "state unknown",
}

function util.battery_phrase(state)
    return BATTERY_PHRASES[state] or "state unknown"
end

-- Battery display helpers: warnings use `Discharging` and `Empty`. `PendingDischarge` does not warn
-- because it is not draining -- by its name the discharge is pending, and UPower defines it no
-- further. `battery_eta` returns `", 2h 14m left"` or `""`. UPower
-- UPower estimates one duration at a time and neither while learning the rate, so the empty string
-- is common during the first minute after a plug or a boot, not an error.
function util.battery_eta(b)
    local seconds, suffix
    if b.time_to_empty then
        seconds, suffix = b.time_to_empty, "left"
    elseif b.time_to_full then
        seconds, suffix = b.time_to_full, "to full"
    else
        return ""
    end
    local hours = math.floor(seconds / 3600)
    local minutes = math.floor((seconds % 3600) / 60)
    if hours > 0 then
        return string.format(", %dh %02dm %s", hours, minutes, suffix)
    end
    return string.format(", %dm %s", minutes, suffix)
end

function util.battery_is_draining(state)
    return state == "Discharging" or state == "Empty"
end

-- `BatteryService.qml`'s `lowThreshold`/`criticalThreshold`/`suspendThreshold` as whole numbers;
-- one table keeps the pill, two notifications, and automatic suspend in agreement.
util.battery_thresholds = { low = 20, critical = 10, suspend = 8 }

-- Whether `b` drains at or under `percent`. Every threshold uses this gate, so 14% with the charger
-- in cannot turn red, matching the reference's `isOnBattery` check.
function util.battery_at_most(b, percent)
    return b ~= nil and b.present and util.battery_is_draining(b.state) and b.percent <= percent
end

-- Five-level glyph plus the two cable states, shared by `modules/bar/indicators/battery.lua` and
-- the lock card's status row. It takes the raw payload so a caller with a `nil` battery still gets
-- the AC glyph rather than a branch of its own.
-- `PendingCharge` gets the bolt; `Charging` and `FullyCharged` get the plug. That is the mirror's
-- order and not the obvious one, explained at the branch below. This line described the reverse
-- until now, having survived the commit that swapped the two.
function util.battery_glyph(b)
    local icons = require("config.icons")
    if b == nil or not b.present then
        return icons.battery_ac
    end
    -- `BatteryIndicator.qml`'s order, which is not the obvious one: `isPendingCharge` is tested
    -- *first* and gets the charging bolt, and everything else on mains gets the plug -- so a
    -- battery that is actually charging draws the plug, and only a stopped one draws the bolt. That
    -- reads correctly on a machine with a limit set, where charging is the ordinary state and
    -- stopped is the one worth a distinct glyph. Ours had the two swapped, which is why this laptop
    -- showed a plug where the mirror showed a bolt.
    if b.state == "PendingCharge" then
        return icons.battery_pending
    end
    if b.state == "Charging" or b.state == "FullyCharged" then
        return icons.battery_ac
    end
    -- Five buckets over 0..100. Lua's 1-based indexing makes 100% bucket 5, not an out-of-range 6.
    local bucket = math.floor((b.percent or 0) / 20) + 1
    return icons.battery_levels[math.max(1, math.min(5, bucket))]
end

-- Resolve an `app_id` through `obelisk.applications.by_app_id` (ADR-0061). Callers supply different
-- spellings: a desktop file id (`modules/global/launcher.lua`), compositor toplevel `app_id`
-- (`modules/bar/indicators/active_window.lua`), or StatusNotifierItem `Id`
-- (`modules/bar/indicators/sys_tray.lua`).
-- Fold only the caller's spelling here; the map already carries case-folded keys. Doing it in Rust
-- would make the capability guess which caller key was intended.
function util.app_entry(applications, app_id)
    if applications == nil or app_id == nil or app_id == "" then
        return nil
    end
    local by_app_id = applications.by_app_id
    if by_app_id == nil then
        return nil
    end
    return by_app_id[app_id] or by_app_id[string.lower(app_id)]
end

-- The engine's `set_volume` clamp.
util.MAX_VOLUME = 1.5

-- Shared icon mapping for `modules/bar/indicators/volume.lua` and `modules/osd/popup.lua`. It
-- takes raw `obelisk.audio`, not a signal, so callers choose their `nil` behavior. Muted, then four
-- steps by level, as Nerd Font glyphs rather than themed icon names: the OSD accent-tints them and
-- themed icons cannot be tinted.
function util.volume_glyph(a)
    local icons = require("config.icons")
    if a == nil or a.muted then
        return icons.vol_muted
    end
    local percent = (a.volume or 0) * 100
    if percent < 1 then
        return icons.vol_zero
    elseif percent < 33 then
        return icons.vol_low
    elseif percent < 66 then
        return icons.vol_mid
    end
    return icons.vol_high
end

-- Four strength buckets, matching `NetworkService.getWifiIcon`'s 0..100 tiering, shared by the
-- bar indicator and the lock card's status row.
function util.network_glyph(n)
    local icons = require("config.icons")
    if n == nil then
        return icons.wifi_none
    end
    if n.ssid == "Ethernet" then
        return icons.ethernet
    end
    -- A dead radio and a live one joined to nothing are different pictures. `wifi_enabled` was
    -- added to `NetworkState` so the first can be drawn.
    if not n.networking_enabled or not n.wifi_enabled then
        return icons.wifi_off
    end
    if n.ssid == nil then
        return icons.wifi_none
    end
    return icons.wifi[util.signal_tier(n.strength)]
end

-- `signalTier`: >= 95 ? 3 : >= 80 ? 2 : >= 50 ? 1 : 0, 1-indexed here. The bars in the bar, the
-- bars in the list, and the list's order all read it.
function util.signal_tier(strength)
    local percent = strength or 0
    return percent >= 95 and 4 or percent >= 80 and 3 or percent >= 50 and 2 or 1
end

-- Bluetooth devices by shown name, then MAC. The Supervisor builds both lists from a `HashMap`, so
-- their order can change on any rebuild and rows would swap under the pointer. `"\0"` sorts below
-- every name character, so a name that prefixes another still sorts first.
function util.sorted_devices(devices)
    local function key(device)
        return (device.name ~= "" and device.name:lower() or device.mac) .. "\0" .. device.mac
    end
    local out = table.move(devices or {}, 1, #(devices or {}), 1, {})
    table.sort(out, function(left, right)
        return key(left) < key(right)
    end)
    return out
end

-- `Theme.networkBandColor` plus the short label the panel draws beside the bars: "6G", "5G", "2.4".
-- Shared because the bar tints its glyph by the associated band and the panel tints every row; two
-- copies had already drifted apart, with 2.4 GHz yellow in one and `warning` in the mirror. Like
-- `volume_glyph`, it takes raw payload with no signal, and the caller decides about `nil`. A `nil`
-- label with `FG` is the honest answer for ethernet and for a band nothing reported.
---@param ap AccessPointInfo?
---@return string? # Short band label, or `nil` when there is no band to name.
---@return Color # The band's colour, or `FG`.
function util.band_of(ap)
    local theme = require("config.theme")
    local number = ap and ap.band and ap.band:match("^[%d%.]+")
    if number == "6" then
        return "6G", theme.GREEN
    elseif number == "5" then
        return "5G", theme.ACCENT
    elseif number == "2.4" then
        return "2.4", theme.PEACH
    end
    return nil, theme.FG
end

-- The `available_networks` entry the link is actually on. `NetworkState` carries `ssid` and
-- `strength` for the association but not its band, so anything band-shaped has to come back through
-- the AP list. A wired link has no entry here, which is why callers need no separate ethernet case.
---@param n NetworkState?
---@return AccessPointInfo? # The associated access point, or `nil`.
function util.active_access_point(n)
    for _, ap in ipairs((n and n.available_networks) or {}) do
        if ap.active then
            return ap
        end
    end
    return nil
end

-- Hide a module with no content instead of showing a "--" pill. `visible` is a signal-bound base
-- property, so hidden children are skipped by row positioning rather than laid out at zero
-- width. Use a codepoint budget here, the exception to `components/cell.lua`'s pixel-box rule.
-- Centre-zone modules need content-sized nodes between two `Fill` sides; bounding them made short
-- "(1) WhatsApp" sit a hundred pixels left of centre. ponytail: "WWWW" and "iiii" share four
-- codepoints but differ in width, so this cuts to a ragged pixel width. Upgrade with
-- `text.max_width`, letting the engine measure/elide while reporting the string's own width when it
-- fits; that requires a layout change, not config.
function util.truncate(value, limit)
    local s = tostring(value or "")
    local count = utf8.len(s)
    if count == nil or count <= limit then
        return s
    end
    return s:sub(1, utf8.offset(s, limit + 1) - 1) .. "..."
end

-- `notification.body` is a parsed freedesktop markup span array (ADR-0033). The Supervisor
-- parses it once, and `text.content` accepts the same run shape (ADR-0104): links become underlined
-- `link_color` runs with `href`, while the engine draws/reports pressed runs but knows no URLs.
-- Image spans go to `util.notification_images` because `text` refuses runs without `text`. Like
-- `NotificationText`'s `linkify`, scan only unlinked text so pasted web/file addresses become
-- pressable while `<a href>` targets survive. Strip trailing sentence punctuation, which is almost
-- never part of a URL.
local URL_PATTERNS = { "%f[%S]https?://[^%s<>'\"]+", "%f[%S]file://[^%s<>'\"]+" }

local function linkified(spans)
    local out = {}
    for _, span in ipairs(spans or {}) do
        local text = span.kind == "text" and (span.href == nil or span.href == "") and span.text or nil
        if not text then
            out[#out + 1] = span
        else
            local at = 1
            while at <= #text do
                local first, last
                for _, pattern in ipairs(URL_PATTERNS) do
                    local from, to = text:find(pattern, at)
                    if from and (not first or from < first) then
                        first, last = from, to
                    end
                end
                if not first then
                    break
                end
                local href = text:sub(first, last):gsub("[.,;:!?]+$", "")
                last = first + #href - 1
                if first > at then
                    local plain = {}
                    for key, value in pairs(span) do
                        plain[key] = value
                    end
                    plain.text = text:sub(at, first - 1)
                    out[#out + 1] = plain
                end
                local link = {}
                for key, value in pairs(span) do
                    link[key] = value
                end
                link.text, link.href = href, href
                out[#out + 1] = link
                at = last + 1
            end
            if at == 1 then
                out[#out + 1] = span
            elseif at <= #text then
                local rest = {}
                for key, value in pairs(span) do
                    rest[key] = value
                end
                rest.text = text:sub(at)
                out[#out + 1] = rest
            end
        end
    end
    return out
end

function util.notification_body(spans, link_color)
    spans = linkified(spans)
    local runs = {}
    for _, span in ipairs(spans or {}) do
        if span.kind == "text" and span.text and span.text ~= "" then
            local is_link = span.href ~= nil and span.href ~= ""
            runs[#runs + 1] = {
                text = span.text,
                bold = span.bold or false,
                italic = span.italic or false,
                underline = span.underline or is_link,
                color = is_link and link_color or nil,
                -- Carries `href` to `on_link` (ADR-0106). The engine never opens it; the card does.
                href = is_link and span.href or nil,
            }
        end
    end
    return runs
end

-- Run character count for `components/notification_card.lua`'s pre-measurement expander guess.
function util.runs_length(runs)
    local total = 0
    for _, run in ipairs(runs or {}) do
        total = total + utf8.len(run.text or "")
    end
    return total
end

-- Distinct body link targets in first-seen order; repeated pages get one button.
function util.notification_links(spans)
    local links, seen = {}, {}
    for _, span in ipairs(linkified(spans)) do
        local href = span.kind == "text" and span.href or nil
        if href and href ~= "" and not seen[href] then
            seen[href] = true
            links[#links + 1] = href
        end
    end
    return links
end

-- Inline body pictures (`<img src>`), trusted-root validated by the Supervisor; draw under text, as
-- `util.notification_body` does.
function util.notification_images(spans)
    local paths = {}
    for _, span in ipairs(spans or {}) do
        if span.kind == "image" and span.image_path then
            paths[#paths + 1] = span.image_path
        end
    end
    return paths
end

-- Link label: web host, `mailto:` address, or the full URL otherwise. Full URLs do not fit a card
-- button.
function util.link_label(href)
    local rest = href:match("^[%a][%w+.-]*://(.*)$")
    if rest then
        return (rest:match("^[^/?#]+") or rest):gsub("^www%.", "")
    end
    return href:match("^mailto:(.+)$") or href
end

-- Content identity for popup bookkeeping. Include `timestamp`, not only id: `replaces_id` reuses an
-- id for new content, while timestamp changes on every `Notify` and otherwise stays put (ADR-0093).
function util.notification_key(notification)
    return string.format("%d:%d", notification.id or 0, notification.timestamp or 0)
end

-- Group the feed by sending application, turning eight chat messages into one card
-- (`NotificationCard.qml`'s `group`). Key by sender `desktop_entry` (ADR-0101), or `app_name` when
-- absent. Desktop ids avoid shared or changing display names and key `applications.by_app_id`
-- (ADR-0061), supplying installed `Name=`/ `Icon=`; before the first `obelisk.applications` push
-- (`nil`), use the sender's name and icon. Match `_compareGroups`: critical first, then newest
-- notification. The newest-first feed keeps a recently speaking app above one silent for an hour;
-- key breaks equal-second ties between passes. `opts.skip_transient` omits sender-marked
-- `transient` notifications (ADR-0100): history omits them; popup does not.
function util.group_notifications(feed, applications, opts)
    opts = opts or {}
    local groups, by_key = {}, {}
    for _, notification in ipairs(feed or {}) do
        if not (opts.skip_transient and notification.transient) then
            local entry_id = notification.desktop_entry
            local key = entry_id and string.lower(entry_id) or (notification.app_name or "?")
            local group = by_key[key]
            if group == nil then
                local entry = util.app_entry(applications, entry_id)
                group = {
                    key = key,
                    app_name = (entry and entry.name) or notification.app_name or "?",
                    app_icon = (entry and entry.icon) or notification.app_icon,
                    urgency = notification.urgency or "normal",
                    latest = notification.timestamp or 0,
                    items = {},
                }
                by_key[key] = group
                groups[#groups + 1] = group
            end
            group.items[#group.items + 1] = notification
        end
    end
    table.sort(groups, function(a, b)
        local a_critical, b_critical = a.urgency == "critical", b.urgency == "critical"
        if a_critical ~= b_critical then
            return a_critical
        end
        if a.latest ~= b.latest then
            return a.latest > b.latest
        end
        return a.key < b.key
    end)
    return groups
end

-- History groups with `NotificationService.qml`'s `bucketOrder`: "urgent", "today", "yesterday",
-- "earlier". Flatten for `list`; headers are `kind = "header"`, and colon keys cannot collide with
-- desktop ids, which contain no colon. `now` is `obelisk.system.time`; today starts at local
-- midnight.
function util.notification_sections(groups, now)
    local today = os.date("*t", now)
    local today_start = os.time({ year = today.year, month = today.month, day = today.day, hour = 0 })
    local buckets = {
        { label = "urgent",    items = {} },
        { label = "today",     items = {} },
        { label = "yesterday", items = {} },
        { label = "earlier",   items = {} },
    }
    for _, group in ipairs(groups or {}) do
        local index = 4
        if group.urgency == "critical" then
            index = 1
        elseif group.latest >= today_start then
            index = 2
        elseif group.latest >= today_start - 86400 then
            index = 3
        end
        local items = buckets[index].items
        items[#items + 1] = group
    end
    local sections = {}
    for _, bucket in ipairs(buckets) do
        if #bucket.items > 0 then
            sections[#sections + 1] = { kind = "header", key = "header:" .. bucket.label, label = bucket.label }
            for _, group in ipairs(bucket.items) do
                sections[#sections + 1] = group
            end
        end
    end
    return sections
end

-- History arrival as "Wed 14:32"; `%a` is enough because sections already name the day.
function util.absolute_time(timestamp)
    return os.date("%a %H:%M", timestamp or 0)
end

function util.shown_when(signal, predicate)
    return signal:map(function(value)
        if value == nil then
            return false
        end
        local ok, shown = pcall(predicate, value)
        return ok and shown or false
    end)
end

-- `signal` or its value from up to `ms` ago: true while the source is true and for `ms` after it
-- drops. The close-hold `PanelHost.qml` builds from a `Timer` and six `retained*` copies; here the
-- hidden subtree keeps its content (ADR-0124) and `delay` keeps the surface mapped while the exit
-- tween runs (ADR-0146).
function util.linger(signal, ms)
    return computed({ signal, delay(signal, ms) }, function(now, was)
        -- `== true` rather than `now or was`, which returned whatever `delay` held. `delay` answers
        -- the source's older value, and before the first change settles that is the property's
        -- identity, `0` -- a number Lua calls truthy and the engine refuses, so `modal_host`'s
        -- `visible` got `Integer(0)` and the whole re-resolve was dropped. Every caller here feeds
        -- a `visible`, so the boolean is the helper's job to guarantee.
        return now == true or was == true
    end)
end

-- Whitespace off both ends. Parenthesised because `gsub` also returns its count, and a caller
-- writing `return util.trim(x)` would otherwise return two values.
function util.trim(text)
    return (tostring(text or ""):gsub("^%s+", ""):gsub("%s+$", ""))
end

-- Thousands separators into an already-formatted number: the half of JS `toLocaleString` the
-- launcher's calculator and currency rows need. Reverse, group, reverse, because grouping from the
-- right is what makes "1234" read as "1,234" rather than "123,4".
function util.thousands(formatted)
    local sign, digits, rest = formatted:match("^(%-?)(%d+)(.*)$")
    if not digits then
        return formatted
    end
    local grouped = digits:reverse():gsub("(%d%d%d)", "%1,"):reverse():gsub("^,", "")
    return sign .. grouped .. rest
end

return util
