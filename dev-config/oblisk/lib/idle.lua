-- Idle-seat policy, not execution: settings, three stages, and reasons holding the session awake.
-- `modules/global/idle.lua` runs the clock. Keep this side-effect-free for bar readers, with the
-- one-way dependency as `lib/media.lua`: `modules/` requires `lib/`, never the reverse.
-- `IdleService.qml` uses three `IdleMonitor`s with per-stage `timeout` and chained `enabled`. This
-- registers one one-second threshold and counts on `oblisk.system.time`.
-- `oblisk.idle:register_threshold` has no removal counterpart (§ 3.2): changing lock from five to
-- ten minutes would leave both thresholds registered and still lock at five. One registration keeps
-- editable Lua-number timeouts and a walked stage list.
-- In `IdleService.qml`, each `enabled` gates on the preceding stage: `_lockDone` is
-- `!lockActionEnabled || LockService.locked`, and DPMS names it. A monitor starts timing when its
-- gate turns true; unlocking makes `_lockDone` false, tears down pending DPMS, and restarts the
-- chain.
-- Here each stage carries `done`; `idle.eligible` generalizes the same rule to any `order`: arm a
-- stage once every enabled predecessor is done. `modules/global/idle.lua` stamps arming, fires
-- after that stage's delay, and clears the stamp when it is no longer armed.
-- Counting from one zero failed twice: the two timeouts interfered, so lowering blank shortened the
-- lock gap; unlocking left the screen due to blank a minute later regardless of what followed.
-- Stages use the bar's existing one-second readout clock, so resolution is one second with no new
-- cadence. `oblisk.system` pushes are load-bearing; if that timer stops, stages stop too.
-- ponytail: the one-second threshold reports idle one second after last input. `idle_since`
-- subtracts it back out, but `ext-idle-notifier-v1` has no "how long idle" call to do better.
-- The mirror checks `idleEnabled && !inhibited` on every stage because its Wayland-surface
-- `IdleInhibitor` is ignored by its own `IdleMonitor`s. Here `oblisk.idle:inhibit(reason)` takes a
-- logind hold, and the Supervisor holds every threshold event while anything holds one, ours
-- included (ADR-0139). Manual hold, video, and `systemd-inhibit --what=idle` therefore stop stages;
-- `idle_since` resets on entry and stages need no individual guard.
local store = require("lib.store")
local media = require("lib.media")
local icons = require("config.icons")

local idle = {}

-- One registration. The mirror's second `IdleMonitor { timeout: 1 }` wakes displays on input; this
-- threshold handles both display wake and counting.
idle.TICK = 1

-- Stages are listed in panel order, not run order, which follows timeouts. `options` mirrors
-- `IdleSettingsPanel.qml`'s `timeoutOptionsMin` in seconds; hand-edited `state.json` values still
-- read.
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
        -- `_lockDone`; unlocking makes it false and disarms every following stage.
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
        -- Terminal: nothing waits behind it, and a suspended machine is not idle. A stage without
        -- `done` never satisfies a successor.
    },
}

--- Stage by key, or `nil`; used to validate an `order` entry read from disk.
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

---@type table<string, any>
local DEFAULTS = {
    enabled = false,
    video_auto_inhibit = true,
    ac = { dpms_on = true, dpms_sec = 300, lock_on = true, lock_sec = 600, suspend_on = false, suspend_sec = 1800 },
    battery = { dpms_on = true, dpms_sec = 120, lock_on = true, lock_sec = 180, suspend_on = true, suspend_sec = 600 },
}

-- Normalize to each stage exactly once: drop unknown/duplicate names, then append missing stages in
-- declaration order, repairing hand-edited `state.json` and files predating a new stage.
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

--- Fill every missing key. `persistent_table` seeds only top-level `idle` once, so an older
--- `state.json` would otherwise yield a missing stage timeout and divide by `nil`.
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

-- Copy-on-write like `lib/ui_state.lua`'s `toggle_key`: table identity makes a fresh table
-- necessary for the write and prevents mutating a value under an unfinished resolve.
local function with(source, key, value)
    local next_table = {}
    for k, v in pairs(source or {}) do
        next_table[k] = v
    end
    next_table[key] = value
    return next_table
end

--- Write one setting to `lib/store.lua`; `profile` is `"ac"`, `"battery"`, or `nil` for shared
--- keys.
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

--- Next `stage.options` value from `sec`, wrapping; `step = -1` goes down. This follows
--- `modules/bar/panels/power_menu.lua`'s brightness rule: stopping at an end reads as broken, and
--- either end is safe.
--- @param stage table one entry of `idle.STAGES`
--- @param sec integer
--- @param step integer
--- @return integer
function idle.cycle(stage, sec, step)
    local options = stage.options
    -- Choose the nearest option at or above the stored value, so hand-edited 45s steps to 60s
    -- rather than the list's start.
    local index = #options
    for position, value in ipairs(options) do
        if value >= sec then
            index = position
            break
        end
    end
    if options[index] ~= sec then
        -- Land on that neighbour first; an off-list value is one press from a listed value.
        return step > 0 and options[index] or options[math.max(1, index - 1)]
    end
    return options[(index - 1 + step) % #options + 1]
end

--- Timeout words: `"off"`, `"45s"`, `"5m"`, `"1m 30s"`.
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

--- Clock duration for the two counters: `"0:42"`, `"14:03"`.
--- @param sec integer
--- @return string
function idle.clock(sec)
    return string.format("%d:%02d", math.max(0, sec) // 60, math.max(0, sec) % 60)
end

-- ## State
-- Use named `state()` signals because registry entries survive config reloads
-- (ADR-0044 decision 5), preventing an edit from forgetting a manual hold or leaking its inhibitor.

--- The `oblisk.system.time` the seat went idle, or `0` while it is awake.
idle.since = state("idle_since", 0)

--- Arming time in `oblisk.system.time`, keyed by `stage.key`. Missing means
--- `IdleMonitor { enabled: false }`;
--- the stamp makes delay relative, and clearing it makes unlock undo the sequence.
idle.armed_at = state("idle_armed_at", {})

--- Whether the displays are off because `modules/global/idle.lua` turned them off.
idle.blanked = state("idle_blanked", false)

--- The arming stamp each stage has already fired for, keyed by `stage.key`. `dpms` and `lock`
--- report `done` once they act, so the walk moves past them. `suspend` is terminal and observes
--- nothing, so without this it re-ran `systemctl suspend` every tick from the moment it came due
--- until something ended the idle period.
idle.fired_at = state("idle_fired_at", {})

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

--- Reasons *this config* would take a logind hold for, or an empty list. Pure and payload-based so
--- `modules/global/idle.lua` can use `on_change`'s value instead of a possibly stale `computed`.
---
--- Holds only, which is why foreign holders are absent. Taking our own inhibitor because another
--- application holds one is a second block for one reason, and nothing releases it. The writers
--- watch privacy, mpris and storage, never `oblisk.idle`; [`idle.reasons`] adds foreign holders
--- back.
--- @param privacy table? `oblisk.privacy`'s payload
--- @param mpris table? `oblisk.mpris`'s payload
--- @param settings table the result of [`idle.read`]
--- @param manual boolean
--- @return string[]
function idle.own_reasons(privacy, mpris, settings, manual)
    local reasons = {}
    if manual then
        reasons[#reasons + 1] = "manual"
    end
    -- `automaticInhibitorActive`: video, camera, microphone and screen capture, named separately so
    -- "why is my laptop not sleeping" gets the actual reason rather than "media".
    -- Gated on the master switch, unlike `manual` above it. These hold off *our* stages, so with
    -- automatic actions off there is nothing to hold. `manual` stays ungated as an explicit press.
    if settings.enabled and settings.video_auto_inhibit then
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
    -- No `fullscreenInhibitorActive`. `active_client.is_fullscreen` exists but is nil under niri,
    -- which reports no such field and does not fabricate `false` (ADR-0056 decision 5); Hyprland
    -- reports it (ADR-0119). A fullscreen film is therefore caught by `video` or not at all.
    return reasons
end

--- Everything holding the session awake, ours and anyone else's, for anything drawing the list.
---
--- Foreign entries are holds this config did not take: `systemd-inhibit --what=idle`, a browser
--- call, or the compositor withholding notifications for a surface inhibitor (ADR-0160, which
--- arrives with an empty `who`). ADR-0139 made the framework honor them; ADR-0141 exposed them.
idle.reasons = computed(
    { oblisk.privacy, oblisk.mpris, store.idle, idle.manual, oblisk.idle },
    function(p, m, stored, manual, foreign)
        local reasons = idle.own_reasons(p, m, idle.read(stored), manual)
        for _, inhibitor in ipairs((foreign or {}).inhibitors or {}) do
            reasons[#reasons + 1] = inhibitor.who ~= "" and inhibitor.who or "another application"
        end
        return reasons
    end
)

--- Whether anything holds the session awake, including unnamed holders. `oblisk.idle`'s `inhibited`
--- is the authoritative `BlockInhibited` gate, so an unreadable `who` still stops the countdown
--- instead of leaving a modal bar that can never fill.
idle.inhibited = computed({ idle.reasons, oblisk.idle }, function(reasons, foreign)
    return #reasons > 0 or (foreign ~= nil and foreign.inhibited == true)
end)

--- Make the logind hold match [`idle.reasons`], with no-op convergence. Every caller that can
--- change the answer calls this one writer.
---
--- Keep the capability call in `lib/`, like `lib/ui_state.lua`'s `close_panel`; three module
--- callers remembering take/drop/count would eventually disagree.
function idle.sync_inhibit()
    -- Our hold is excluded from `oblisk.idle.inhibitors` by `foreign_idle_inhibitors`, so readback
    -- cannot make this function think it already holds one and skip acquiring it.
    local reasons = idle.own_reasons(
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

--- Flip the bar button's hold and settle its inhibitor.
--- @param on boolean
function idle.set_manual(on)
    idle.manual:set(on)
    idle.sync_inhibit()
end

--- Runnable stages in order and their post-arming delays. `at` is a display total only; no stage
--- fires from it because its clock starts when it arms.
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
                at = from + delay,
                delay = delay,
            }
            from = from + delay
        end
    end
    return { list = list, total = from }
end

--- Currently armed stage from `plan`, or `nil`.
---
--- Generalizes `IdleStage.enabled`: choose the first stage whose predecessors report
--- [`done`](idle.STAGES), skipping stages already done. A stage without `done` satisfies no
--- successor, so it terminates the chain.
---
--- Recomputed every tick, not latched: when `oblisk.lock.active` goes false, lock becomes undone
--- and the next stage stops being armed on the next tick without an unlock listener.
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

--- Move one stage `step` places and store it. Out-of-range is a no-op, so the modal can wire both
--- chevrons unconditionally and hide them rather than guard.
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

--- The master switch on its own, for anything drawing "is automation running". A missing name in a
--- table construction dropped a trailing `nil`: `modules/bar/indicators/idle_inhibitor.lua` asked
--- for four dependencies, got three, and its countdown line became unreachable.
--- four dependencies, got three, and its countdown line became unreachable.
idle.enabled = store.idle:map(function(stored)
    return idle.read(stored).enabled
end)

idle.schedule = computed({ store.idle, idle.active_profile }, function(stored, profile)
    return idle.plan(idle.read(stored), profile)
end)

--- Armed stage and elapsed time from the stamp `modules/global/idle.lua` writes. No stage is
--- `{ key = "", elapsed = 0 }`.
idle.arming = computed({ oblisk.system, idle.armed_at }, function(s, stamps)
    for key, at in pairs(stamps or {}) do
        return { key = key, elapsed = math.max(0, ((s and s.time) or 0) - at) }
    end
    return { key = "", elapsed = 0 }
end)

--- Seat idle duration in seconds, or `0` while awake.
idle.elapsed = computed({ oblisk.system, idle.since }, function(s, since)
    if since == 0 then
        return 0
    end
    return math.max(0, ((s and s.time) or 0) - since)
end)

return idle
