-- Small pure helpers with no node in them, which is what keeps them out of `components/`. The
-- Quickshell config this layout mirrors draws the same line: `Components/` holds widgets,
-- `Services/Utils/` holds functions.
local util = {}

-- Every capability signal reads `nil` until the Supervisor's first snapshot for it arrives, and a
-- payload can be malformed in ways a config should not crash the whole evaluation over. This fixes
-- both once: `nil` renders as "--", a raising reader renders as "!", and each module is left as the
-- one line that reads its own payload.
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

-- `oblisk.battery.state`'s seven UPower names as words a person reads. Three files show this line
-- (the pill's tooltip, the power menu, the lock screen) and the wording has to be the same in all
-- three, which is the whole reason it is not written out at each of them.
--
-- `PendingCharge` is the one worth having: a laptop with `charge_control_end_threshold` set sits
-- there whenever it is plugged in and at the limit, and under the old `charging` boolean it read as
-- "discharging" -- the opposite of what the cable was doing. `PendingDischarge` is its mirror, the
-- battery draining down to a limit that was lowered under it.
local BATTERY_PHRASES = {
    Charging = "charging",
    Discharging = "discharging",
    Empty = "empty",
    FullyCharged = "full",
    PendingCharge = "charge limit reached",
    PendingDischarge = "draining to limit",
    Unknown = "state unknown",
}

function util.battery_phrase(state)
    return BATTERY_PHRASES[state] or "state unknown"
end

-- Whether a battery is actually running down, which is the only time a low reading is worth
-- colouring. `Discharging` on mains does not exist; `PendingDischarge` is on mains by definition and
-- stops at the limit, so it is not the same thing and does not warn.
-- `", 2h 14m left"` or `""`. UPower estimates one of the two durations at a time and neither while
-- it is still learning the rate, so the empty string is the common case for the first minute after
-- a plug or a boot rather than an error.
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

function util.count(list)
    return list and #list or 0
end

-- An `app_id` to its `.desktop` entry, through `oblisk.applications`'s own `by_app_id` map
-- (ADR-0061). Three callers want this and each holds a differently-spelled id:
-- `modules/global/launcher.lua` has a real desktop file id, `modules/bar/indicators/active_window.lua`
-- has whatever the compositor reports as a toplevel's `app_id`, and
-- `modules/bar/indicators/sys_tray.lua` has a StatusNotifierItem's self-declared `Id`.
--
-- The lowercase retry is here rather than in the capability because the map already carries a
-- case-folded key for every entry: this only has to fold the *caller's* spelling to reach it, and
-- doing that in Rust would mean the capability guessing which of its keys a caller meant.
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

-- The icon-name mapping `modules/bar/indicators/volume.lua` and `modules/osd/popup.lua` both need:
-- pulled out once a second real call site made it a duplicate rather than a one-off
-- (`components/pill.lua`'s own bar for a shared file). Takes the raw `oblisk.audio` payload, not a
-- signal, so a caller decides for itself whether `nil` gets its own branch or an empty icon name.
function util.volume_icon_name(a)
    if a == nil then
        return ""
    end
    if a.muted then
        return "audio-volume-muted"
    end
    local percent = (a.volume or 0) * 100
    if percent < 34 then
        return "audio-volume-low"
    elseif percent < 67 then
        return "audio-volume-medium"
    end
    return "audio-volume-high"
end

-- A module that has nothing to say should not be a pill containing "--". `visible` is an ordinary
-- base property (§ 5.1) and takes a signal like any other, so a module can hide itself on the same
-- pass that resolves its text, and a hidden child is skipped by the row's own positioning rather
-- than laid out at zero width.
-- A codepoint budget, and the one place `components/cell.lua`'s argument against character counts
-- does not apply. That component is right that a box is the better unit: it elides against pixels
-- and the caller never guesses. But eliding needs a bounded box, and the two modules in the bar's
-- centre zone need the opposite -- a node exactly as wide as its content, so the content-sized zone
-- between two `Fill` sides puts its midpoint on the bar's midpoint. Bound the box and a short title
-- floats somewhere inside a fixed reservation instead, which is what "(1) WhatsApp" sitting a
-- hundred pixels left of centre was.
--
-- ponytail: the ceiling is that "WWWW" and "iiii" are the same four codepoints and twice different
-- widths, so this cuts to a ragged pixel width. The upgrade is a `max_width` on `text` that lets
-- the engine measure and elide while still reporting the string's own width when it fits, which is
-- a layout change rather than a config one.
function util.truncate(value, limit)
    local s = tostring(value or "")
    local count = utf8.len(s)
    if count == nil or count <= limit then
        return s
    end
    return s:sub(1, utf8.offset(s, limit + 1) - 1) .. "..."
end

-- `notification.body` is a span array, not a string: the freedesktop body is markup, and the
-- Supervisor parses it once so no config has to (§ 2.7, ADR-0033). `text.content` takes an array of
-- runs of the same shape (ADR-0104), so this is a near pass-through: a text span becomes a run, and
-- a link becomes an underlined run in `link_color`, which is where "what does a link look like"
-- gets decided -- the engine draws runs and knows nothing about hrefs.
--
-- Image spans are left out here and drawn by `util.notification_images`: a picture inside a line of
-- text has nowhere to go, and `text` refuses a run with no `text` for exactly that reason.
function util.notification_body(spans, link_color)
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
            }
        end
    end
    return runs
end

-- How many characters a run array holds, for the "is there enough here to be worth an expander"
-- guess `components/notification_card.lua` makes before the engine has measured anything.
function util.runs_length(runs)
    local total = 0
    for _, run in ipairs(runs or {}) do
        total = total + utf8.len(run.text or "")
    end
    return total
end

-- The distinct link targets in a body, in first-seen order. A body that links the same page twice
-- gets one button for it.
function util.notification_links(spans)
    local links, seen = {}, {}
    for _, span in ipairs(spans or {}) do
        local href = span.kind == "text" and span.href or nil
        if href and href ~= "" and not seen[href] then
            seen[href] = true
            links[#links + 1] = href
        end
    end
    return links
end

-- The pictures a body carried inline (`<img src>`), already validated against the trusted roots by
-- the Supervisor. Drawn under the text rather than in it, see `util.notification_body`.
function util.notification_images(spans)
    local paths = {}
    for _, span in ipairs(spans or {}) do
        if span.kind == "image" and span.image_path then
            paths[#paths + 1] = span.image_path
        end
    end
    return paths
end

-- What a link button says: the host for a web address, the address for `mailto:`, and the URL
-- itself for anything else. A full URL on a button is unreadable at any width that fits a card.
function util.link_label(href)
    local rest = href:match("^[%a][%w+.-]*://(.*)$")
    if rest then
        return (rest:match("^[^/?#]+") or rest):gsub("^www%.", "")
    end
    return href:match("^mailto:(.+)$") or href
end

-- A notification's age as the two or three characters a card has room for: "now", "5m", "3h",
-- "2d". `notification.timestamp` and `oblisk.system.time` are both Unix epoch seconds on the same
-- clock (ADR-0093), so this is a subtraction and not a reconciliation.
--
-- Coarse on purpose, and coarser the older it gets. A card shows this beside a summary it is
-- already competing with for width, and nobody reading a notification list needs to know an entry
-- is 2h14m old rather than 2h.
function util.relative_time(now, timestamp)
    local age = (now or 0) - (timestamp or 0)
    -- A clock that stepped backwards, or a push that raced the second boundary. "now" is the
    -- honest answer for both and is what the next tick will say anyway.
    if age < 60 then
        return "now"
    elseif age < 3600 then
        return string.format("%dm", age // 60)
    elseif age < 86400 then
        return string.format("%dh", age // 3600)
    end
    return string.format("%dd", age // 86400)
end

-- What identifies one notification's *content*, for the config's own bookkeeping about what it has
-- already shown. Not the id on its own: `replaces_id` deliberately reuses an id to put new content
-- at it, so an id-keyed note would suppress the replacement as though it were the thing it
-- replaced. `timestamp` moves on every `Notify` and stays put otherwise (ADR-0093), which is
-- exactly the distinction wanted.
function util.notification_key(notification)
    return string.format("%d:%d", notification.id or 0, notification.timestamp or 0)
end

-- The feed as one entry per sending application rather than one per notification, which is what
-- turns eight messages from one chat app into one card instead of eight (`NotificationCard.qml`'s
-- `group`).
--
-- Keyed on `desktop_entry` where the sender set one (ADR-0101) and on `app_name` where it did
-- not. The desktop id is the better key -- two apps can share a display name and one can change
-- its own -- and it is also the key into `applications.by_app_id` (ADR-0061), so a group named
-- by its desktop file gets the installed application's own `Name=` and `Icon=` rather than the
-- sender's description of itself. `applications` is the `oblisk.applications` payload, or `nil`
-- before its first push, in which case the sender's own name and icon stand in.
--
-- Ordered the way the mirror's `_compareGroups` orders: critical groups first, then by each
-- group's *newest* notification, since the feed arrives newest-first and an app that just spoke
-- should not sit below one that spoke an hour ago. The key is the tiebreak, so two groups with the
-- same second do not swap places from one pass to the next.
--
-- `opts.skip_transient` leaves out notifications the sender marked `transient` (ADR-0100): the
-- history never shows them, the popup does.
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

-- The history's groups with a heading before each run of them: "urgent", "today", "yesterday",
-- "earlier" (`NotificationService.qml`'s `bucketOrder`). One flat array, because a `list` draws one
-- array and a heading is an item in it; `kind = "header"` is how the item function tells the two
-- apart, and a heading's key cannot collide with a group's since no desktop id holds a colon.
-- `now` is `oblisk.system.time`, and "today" starts at the local midnight before it.
function util.notification_sections(groups, now)
    local today = os.date("*t", now)
    local today_start = os.time({ year = today.year, month = today.month, day = today.day, hour = 0 })
    local buckets = {
        { label = "urgent", items = {} },
        { label = "today", items = {} },
        { label = "yesterday", items = {} },
        { label = "earlier", items = {} },
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

-- A notification's arrival as a clock reading, "Wed 14:32", for the history, where "3h" is less
-- useful than when. `%a` rather than a date: the sections above already say which day.
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

return util
