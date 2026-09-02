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
-- Supervisor parses it once so no config has to (§ 2.7, ADR-0033). Both readers of it want a
-- flat run back -- a `panel_row` subtitle and the OSD's one elided line -- because `text` carries
-- one string and no rich runs, so a span per node would be paragraph layout neither caller has
-- room for. The styling each span carries is dropped with it; drawing bold means a `text` node
-- per span, which is the same change as wrapping.
--
-- Image spans contribute nothing rather than a placeholder: an inline `<img>` in a two-line row
-- has nowhere to go, and "[image]" in the middle of a sentence reads worse than the gap.
function util.notification_body(spans)
    local parts = {}
    for _, span in ipairs(spans or {}) do
        if span.kind == "text" then
            parts[#parts + 1] = span.text
        end
    end
    return table.concat(parts)
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
