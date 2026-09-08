-- Who is logged in, as `MainService` exposes it over D-Bus.
--
-- A config has `os.getenv` and `process.run` (ADR-0048), and the environment carries neither the
-- full name nor the host, so each is read once from the tool that owns it: GECOS out of
-- `getent passwd`, the node name out of `uname -n`. Both are decoration -- callers draw `$USER` and
-- "localhost" until they answer, and keep them if they never do.
--
-- Extracted from `modules/global/lock.lua` when the notifications panel wanted the same name for
-- its greeting. Two readers is the extraction rule, and it matters more than usual here: the two
-- processes must run once for the session, not once per module that asks.
--
-- Kept in `state` rather than a module local because a reload re-runs this file: the guard below is
-- what stops a save spawning two more processes, since the value outlives the evaluation that set
-- it and a table `initial` never re-seeds.
local identity = state("lock_identity", { name = "", host = "" })

local USER = os.getenv("USER") or "user"

local function remember(field, value)
    value = value:match("^%s*(.-)%s*$")
    if value == "" then
        return
    end
    local current = identity:get()
    -- Field at a time: the two processes finish in either order.
    identity:set({
        name = field == "name" and value or current.name,
        host = field == "host" and value or current.host,
    })
end

if identity:get().name == "" then
    -- `anas:x:1000:1000:Anas Khalifa:/home/anas:/usr/bin/fish`. Field five is GECOS, whose first
    -- comma-separated part is the full name; the rest is office and phone numbers nobody fills in.
    process.run("getent", { "passwd", USER }, function(line)
        local fields = {}
        for field in (line .. ":"):gmatch("([^:]*):") do
            fields[#fields + 1] = field
        end
        remember("name", (fields[5] or ""):match("^[^,]*") or "")
    end, function() end)
end

if identity:get().host == "" then
    process.run("uname", { "-n" }, function(line)
        remember("host", line)
    end, function() end)
end

return {
    user = USER,
    full_name = identity:map(function(i)
        return i.name ~= "" and i.name or USER
    end),
    account = identity:map(function(i)
        return string.format("%s@%s", USER, i.host ~= "" and i.host or "localhost")
    end),
    -- `userInitials`: the first letter of each of the first two words, so "Anas Khalifa" is "AK"
    -- and a single-word name is one letter.
    initials = identity:map(function(i)
        local name = i.name ~= "" and i.name or USER
        local letters = ""
        for word in name:gmatch("%S+") do
            letters = letters .. word:sub(1, 1):upper()
            if #letters == 2 then
                break
            end
        end
        return letters ~= "" and letters or "U"
    end),
}
