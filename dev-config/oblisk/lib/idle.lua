-- What the shell does about an idle seat, minus the doing: the settings, the three stages, and the
-- one question everything else asks -- is something holding this session awake, and what.
--
-- `modules/global/idle.lua` is the other half and runs the clock. Split so the bar can read this
-- without pulling in a file whose whole purpose is side effects, the same one-way shape
-- `lib/media.lua` has: `lib/` is required by `modules/`, never the reverse.
--
-- ## One threshold, not three
--
-- `IdleService.qml` is three `IdleMonitor`s, one per stage, each with its own `timeout` and a chain
-- of `enabled` bindings that keep them in order. This registers exactly one threshold, at one
-- second, and then counts on `oblisk.system.time`.
--
-- Not a stylistic preference. `oblisk.idle:register_threshold` has no counterpart that removes one
-- (§ 3.2), so a panel that changes the lock timeout from five minutes to ten would leave both
-- registered and lock at five anyway. With one registration the timeouts are plain Lua numbers a
-- panel can edit, and the stages are a list this file walks rather than a lattice of `enabled`
-- conditions each waiting on the others.
--
-- ## A stage is armed by the one before it finishing, not by a running total
--
-- `IdleService.qml` is three `IdleMonitor`s whose `enabled` is a gate on the stage before them --
-- `_lockDone` is `!lockActionEnabled || LockService.locked`, and the DPMS stage names it. A
-- monitor's timer starts when `enabled` flips true, so the chain is a sequence of relative delays
-- and, more importantly, it *unwinds*: unlocking makes `_lockDone` false again, the pending DPMS
-- monitor is torn down, and the whole sequence starts over. That is the behaviour, and it falls out
-- of the gates rather than being handled anywhere.
--
-- So a stage here carries a `done` predicate, and `idle.eligible` is the same condition
-- generalised to any `order`: a stage is armed once every enabled stage before it reports done.
-- `modules/global/idle.lua` stamps the moment a stage arms and fires it that stage's own delay
-- later, which is what makes the delays relative, and clears the stamp the moment it stops being
-- armed, which is what makes an unlock undo the rest of the sequence.
--
-- Counting from one zero was the first two attempts and was wrong twice over: the two timeouts
-- interfered (lowering the blank silently shortened the gap before the lock), and nothing could
-- undo a stage that had already fired, so unlocking left the screen due to blank a minute later
-- whatever you did next.
--
-- What it costs: stages fire on a one-second clock that the bar's own readouts already run on, so
-- the resolution is a second and the cost is nothing new. `oblisk.system` pushing is load-bearing;
-- if that timer ever stops, the stages stop with it.
--
-- ponytail: the seat is "idle" from the moment the one-second threshold reports it, which is a
-- second after the last input. `idle_since` subtracts that second back out, so the elapsed count is
-- right; nothing here can do better, because `ext-idle-notifier-v1` has no "how long has this seat
-- been idle" call.
--
-- ## Holding it awake is an inhibitor, not a flag
--
-- The mirror's `armed` is `idleEnabled && !inhibited`, checked in every stage's `enabled` binding,
-- because a Quickshell `IdleInhibitor` is a Wayland surface inhibitor its own `IdleMonitor`s ignore.
-- Here `oblisk.idle:inhibit(reason)` takes a logind hold and the Supervisor holds every threshold
-- event for as long as *anything* is holding one, ours included (ADR-0139). So a manual hold, a
-- video playing, and `systemd-inhibit --what=idle` from a terminal all stop the stages by the same
-- route, `idle_since` goes back to zero on the way in, and no stage below needs a guard.
local store = require("lib.store")
local media = require("lib.media")
local icons = require("config.icons")

local idle = {}

-- The one registration. A second is `IdleMonitor { timeout: 1 }` in the mirror, which exists there
-- to wake the displays on any input; it does that here too, and the counting as well.
idle.TICK = 1

-- The three stages, in the order the panel lists them, which is not the order they run in -- that
-- is whatever the timeouts say. `options` is `IdleSettingsPanel.qml`'s `timeoutOptionsMin` in
-- seconds; the value stored need not be one of them, so a `state.json` edited by hand still reads.
idle.STAGES = {
    {
        key = "dpms",
        title = "turn off displays",
        detail = "until input comes back",
        icon = icons.display,
        options = { 30, 60, 120, 300, 600, 900 },
        -- `_dpmsDone`. The stage after this one waits for it.
        done = function()
            return idle.blanked:get()
        end,
    },
    {
        key = "lock",
        title = "lock screen",
        detail = "needs your password to come back",
        icon = icons.lock,
        options = { 30, 60, 120, 300, 600, 900, 1800 },
        -- `_lockDone`, and the reason this whole mechanism is predicates rather than a running
        -- total: unlocking makes this false again, which disarms every stage waiting behind it.
        done = function()
            local l = oblisk.lock:get()
            return l ~= nil and l.active
        end,
    },
    {
        key = "suspend",
        title = "suspend",
        detail = "sleeps the machine",
        icon = icons.sleep,
        options = { 300, 600, 900, 1800, 3600, 7200 },
        -- Terminal: nothing waits behind it, and a suspended machine is not idle. A stage with no
        -- `done` never satisfies a successor, which is the safe answer for any stage added later.
    },
}

--- One stage by key, or `nil`. What validates an `order` entry read back off disk.
--- @param key string
--- @return table?
function idle.stage(key)
    for _, stage in ipairs(idle.STAGES) do
        if stage.key == key then
            return stage
        end
    end
    return nil
end

local ORDER = { "dpms", "lock", "suspend" }

-- Annotated because the table mixes booleans with profile tables, and without it inference calls
-- every value a `boolean|table` and then refuses both halves of the fold in `idle.read`.
---@type table<string, any>
local DEFAULTS = {
    enabled = false,
    video_auto_inhibit = true,
    ac = { dpms_on = true, dpms_sec = 300, lock_on = true, lock_sec = 600, suspend_on = false, suspend_sec = 1800 },
    battery = { dpms_on = true, dpms_sec = 120, lock_on = true, lock_sec = 180, suspend_on = true, suspend_sec = 600 },
}

-- Every stage exactly once, in the stored sequence: unknown names dropped, missing ones appended in
-- declaration order. A hand-edited `state.json` that names a stage twice, or one written before a
-- stage existed, would otherwise leave a stage that can never run and no way to find out why.
local function resolve_order(stored)
    local seen, out = {}, {}
    for _, key in ipairs(type(stored) == "table" and stored or {}) do
        if type(key) == "string" and not seen[key] and idle.stage(key) then
            seen[key] = true
            out[#out + 1] = key
        end
    end
    for _, key in ipairs(ORDER) do
        if not seen[key] then
            out[#out + 1] = key
        end
    end
    return out
end

--- Every key filled in, whatever the stored table is missing. `persistent_table`'s own `defaults`
--- seed the top-level `idle` key once and never look inside it again, so a `state.json` written
--- before a stage existed would otherwise read `nil` for its timeout and divide by it.
--- @param stored table? `store.idle`'s payload
--- @return table
function idle.read(stored)
    stored = type(stored) == "table" and stored or {}
    ---@type table<string, any>
    local out = {
        enabled = stored.enabled == true,
        video_auto_inhibit = stored.video_auto_inhibit ~= false,
        order = resolve_order(stored.order),
    }
    for _, name in ipairs({ "ac", "battery" }) do
        local fallback = DEFAULTS[name]
        local held = type(stored[name]) == "table" and stored[name] or {}
        local profile = {}
        for key, default in pairs(fallback) do
            local value = held[key]
            if type(value) == type(default) then
                profile[key] = value
            else
                profile[key] = default
            end
        end
        out[name] = profile
    end
    return out
end

-- Copy-on-write, `lib/ui_state.lua`'s `toggle_key`: `set` compares a table by identity, so a fresh
-- one is both what makes the write land and what keeps the value under an unfinished resolve from
-- being mutated.
local function with(source, key, value)
    local next_table = {}
    for k, v in pairs(source or {}) do
        next_table[k] = v
    end
    next_table[key] = value
    return next_table
end

--- Writes one setting back to `lib/store.lua`. `profile` is `"ac"`, `"battery"`, or `nil` for the
--- two that are not per-profile.
--- @param profile string?
--- @param key string
--- @param value any
function idle.write(profile, key, value)
    local current = idle.read(store.idle:get())
    if profile == nil then
        store:set("idle", with(current, key, value))
        return
    end
    store:set("idle", with(current, profile, with(current[profile], key, value)))
end

--- The next value up `stage.options` from `sec`, wrapping. `step` of `-1` goes back down. Wrapping
--- rather than clamping is `modules/bar/panels/power_menu.lua`'s brightness rule: a control that
--- stops dead at one end reads as broken, and there is no harm at either end of this list.
--- @param stage table one entry of `idle.STAGES`
--- @param sec integer
--- @param step integer
--- @return integer
function idle.cycle(stage, sec, step)
    local options = stage.options
    -- The nearest option at or above the stored value, so a hand-edited 45s steps to 60s rather
    -- than back to the start of the list.
    local index = #options
    for position, value in ipairs(options) do
        if value >= sec then
            index = position
            break
        end
    end
    if options[index] ~= sec then
        -- Land on that neighbour first; a stored value off the list is one press from a listed one.
        return step > 0 and options[index] or options[math.max(1, index - 1)]
    end
    return options[(index - 1 + step) % #options + 1]
end

--- A timeout as words: `"off"`, `"45s"`, `"5m"`, `"1m 30s"`.
--- @param sec integer?
--- @return string
function idle.format(sec)
    if sec == nil or sec <= 0 then
        return "off"
    end
    if sec < 60 then
        return string.format("%ds", sec)
    end
    if sec % 60 == 0 then
        return string.format("%dm", sec // 60)
    end
    return string.format("%dm %ds", sec // 60, sec % 60)
end

--- A duration as a clock, for the two counters that tick: `"0:42"`, `"14:03"`.
--- @param sec integer
--- @return string
function idle.clock(sec)
    return string.format("%d:%02d", math.max(0, sec) // 60, math.max(0, sec) % 60)
end

-- ## State
--
-- `state()` rather than locals, and the names matter: a signal registry entry keeps its value
-- across a config reload (ADR-0044 decision 5), which is what stops an edit to this file from
-- forgetting a manual hold or leaking the logind inhibitor behind it.

--- The `oblisk.system.time` the seat went idle, or `0` while it is awake.
idle.since = state("idle_since", 0)

--- When each stage armed, in `oblisk.system.time`, keyed by `stage.key`. A stage absent from this
--- is not armed, which is `IdleMonitor { enabled: false }`: no timer is running for it. The stamp
--- is what makes a delay relative, and clearing it is what makes an unlock undo the sequence.
idle.armed_at = state("idle_armed_at", {})

--- Whether the displays are off because `modules/global/idle.lua` turned them off.
idle.blanked = state("idle_blanked", false)

--- `IdleService.manualInhibit`: the bar button's own hold.
idle.manual = state("idle_manual", false)

--- Whether a logind inhibitor is out in our name right now. Not derived from [`idle.reasons`]:
--- `inhibit`/`release_inhibit` are counted (§ 3.2), so this is the count, and a wrong one leaks.
idle.holding = state("idle_holding", false)

--- @param power table? `oblisk.power`'s payload
--- @return string `"ac"` or `"battery"`
function idle.profile_of(power)
    return (power ~= nil and power.on_battery == true) and "battery" or "ac"
end

--- Which profile's numbers are in force. `on_battery` is `nil` on a host with no UPower, which is
--- the AC answer: a machine that cannot tell you it is on battery is plugged in.
idle.active_profile = oblisk.power:map(idle.profile_of)

--- Why the session is being held awake, in words fit to draw, or an empty list when it is not.
--- Pure, and it takes the four payloads rather than reading them, so `modules/global/idle.lua` can
--- call it with what an `on_change` handed it instead of trusting a `computed` to be current
--- inside a callback.
--- @param privacy table? `oblisk.privacy`'s payload
--- @param mpris table? `oblisk.mpris`'s payload
--- @param settings table the result of [`idle.read`]
--- @param manual boolean
--- @return string[]
function idle.reasons_from(privacy, mpris, settings, manual)
    local reasons = {}
    if manual then
        reasons[#reasons + 1] = "manual"
    end
    -- `automaticInhibitorActive`: a video playing, or anything reading a camera, a microphone or
    -- the screen. Named one by one rather than as "media", because "why is my laptop not sleeping"
    -- deserves the actual answer.
    if settings.video_auto_inhibit then
        if media.is_playing_video(mpris) then
            reasons[#reasons + 1] = "video"
        end
        privacy = privacy or {}
        if #(privacy.camera_users or {}) > 0 then
            reasons[#reasons + 1] = "camera"
        end
        if #(privacy.microphone_users or {}) > 0 then
            reasons[#reasons + 1] = "microphone"
        end
        if #(privacy.screencast_users or {}) > 0 then
            reasons[#reasons + 1] = "screen capture"
        end
    end
    -- TODO: `fullscreenInhibitorActive`. `oblisk.workspaces.active_client` carries no fullscreen
    -- flag on niri yet, so a film in a fullscreen player is caught by `video` above or not at all.
    return reasons
end

--- [`idle.reasons_from`] over the live payloads, for anything that draws them.
idle.reasons = computed({ oblisk.privacy, oblisk.mpris, store.idle, idle.manual }, function(p, m, stored, manual)
    return idle.reasons_from(p, m, idle.read(stored), manual)
end)

--- Whether anything is holding the session awake.
idle.inhibited = idle.reasons:map(function(reasons)
    return #reasons > 0
end)

--- Takes or drops the logind hold so it matches [`idle.reasons`], and does nothing when it already
--- does. Every caller that can change the answer calls this; it is the one writer.
---
--- A capability call in `lib/` rather than in a module, which `lib/ui_state.lua`'s `close_panel`
--- already does and for the same reason: the alternative is three callers each remembering to
--- take, drop and count, and one of them eventually not.
function idle.sync_inhibit()
    local reasons = idle.reasons_from(
        oblisk.privacy:get(),
        oblisk.mpris:get(),
        idle.read(store.idle:get()),
        idle.manual:get()
    )
    local want = #reasons > 0
    if want == idle.holding:get() then
        return
    end
    idle.holding:set(want)
    if want then
        oblisk.idle:inhibit(table.concat(reasons, " + "))
    else
        oblisk.idle:release_inhibit()
    end
end

--- Flips the bar button's hold and settles the inhibitor behind it.
--- @param on boolean
function idle.set_manual(on)
    idle.manual:set(on)
    idle.sync_inhibit()
end

--- The stages that will run, in order, with the delay each one waits once it is armed. `from` and
--- `at` are the running total the delays add up to, for anything that wants to say "and then, and
--- then"; nothing fires on them, because a stage's clock starts when it arms and not before.
--- @param settings table the result of [`idle.read`]
--- @param profile string `"ac"` or `"battery"`
--- @return { list: table[], total: integer }
function idle.plan(settings, profile)
    local numbers = settings[profile]
    local list = {}
    local from = 0
    for _, key in ipairs(settings.order) do
        local stage = idle.stage(key)
        local delay = stage and numbers[key .. "_sec"] or 0
        if stage and numbers[key .. "_on"] and delay > 0 then
            list[#list + 1] = {
                key = key,
                icon = stage.icon,
                title = stage.title,
                from = from,
                at = from + delay,
                delay = delay,
            }
            from = from + delay
        end
    end
    return { list = list, total = from }
end

--- Which stage of `plan` is armed right now, or `nil` when none is.
---
--- `IdleStage`'s `enabled` generalised to any order: the first stage whose predecessors have all
--- reported [`done`](idle.STAGES), skipping any that has already reported done itself. A stage with
--- no `done` never satisfies a successor, so a terminal stage ends the chain rather than letting
--- whatever follows it fire immediately.
---
--- Called every tick rather than latched, which is the point: `oblisk.lock`'s `active` going false
--- makes the lock stage undone, and the stage behind it stops being the armed one on the very next
--- tick. Nothing has to notice the unlock.
--- @param plan table the result of [`idle.plan`]
--- @return table? one entry of `plan.list`
function idle.armed(plan)
    for _, entry in ipairs(plan.list) do
        local stage = idle.stage(entry.key)
        local done = stage and stage.done ~= nil and stage.done() == true
        if not done then
            return entry
        end
    end
    return nil
end

--- Moves one stage `step` places through the order and stores the result. Out of range is a no-op,
--- which is what lets the modal wire the two chevrons unconditionally and hide rather than guard.
--- @param key string
--- @param step integer `-1` earlier, `1` later
function idle.move(key, step)
    local order = idle.read(store.idle:get()).order
    local at
    for index, name in ipairs(order) do
        if name == key then
            at = index
        end
    end
    local to = at and at + step
    if to == nil or to < 1 or to > #order then
        return
    end
    order[at], order[to] = order[to], order[at]
    idle.write(nil, "order", order)
end

--- The plan in force right now.
idle.schedule = computed({ store.idle, idle.active_profile }, function(stored, profile)
    return idle.plan(idle.read(stored), profile)
end)

--- Which stage is armed and how long it has been, straight off the stamp
--- `modules/global/idle.lua` writes. `key` is `""` and `elapsed` `0` when none is.
idle.arming = computed({ oblisk.system, idle.armed_at }, function(s, stamps)
    for key, at in pairs(stamps or {}) do
        return { key = key, elapsed = math.max(0, ((s and s.time) or 0) - at) }
    end
    return { key = "", elapsed = 0 }
end)

--- How long the seat has been idle, in seconds, or `0` while it is not.
idle.elapsed = computed({ oblisk.system, idle.since }, function(s, since)
    if since == 0 then
        return 0
    end
    return math.max(0, ((s and s.time) or 0) - since)
end)

return idle
