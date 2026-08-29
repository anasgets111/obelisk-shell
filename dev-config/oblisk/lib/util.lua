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

function util.count(list)
    return list and #list or 0
end

-- A module that has nothing to say should not be a pill containing "--". `visible` is an ordinary
-- base property (§ 5.1) and takes a signal like any other, so a module can hide itself on the same
-- pass that resolves its text, and a hidden child is skipped by the row's own positioning rather
-- than laid out at zero width.
function util.shown_when(signal, predicate)
    return signal:map(function(value)
        if value == nil then
            return false
        end
        local ok, shown = pcall(predicate, value)
        return ok and shown or false
    end)
end

-- `text` has no truncation, no ellipsis and no max width (it wraps to its parent and that is all),
-- so a 200-character track title would push every module to its right off the bar. Truncating in
-- Lua is the only lever a config has today.
--
-- `utf8.offset` rather than `string.sub`, because `string.sub` counts bytes: cutting a track title
-- at byte 28 lands mid-scalar on any non-ASCII text and produces a string the shaper cannot render.
function util.truncate(s, limit)
    if utf8.len(s) == nil or utf8.len(s) <= limit then
        return s
    end
    return string.sub(s, 1, utf8.offset(s, limit + 1) - 1) .. "..."
end

return util
