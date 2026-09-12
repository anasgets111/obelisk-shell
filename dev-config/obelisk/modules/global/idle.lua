-- `IdleService.qml`'s three `IdleMonitor`s and `setDisplaysPowered`, as one threshold plus handler.
--
-- No surface, like `modules/global/power_events.lua`. `shell.lua` requires this for side effects;
-- `lib/idle.lua` holds shared facts so the bar need not require this file.
--
-- ## Why the clock
--
-- `lib/idle.lua` has the clock: thresholds cannot be unregistered, so editable timeouts must be
-- numbers rather than registrations.
--
-- ## What is not guarded here
--
-- No `armed` or inhibitor check, and no manual-toggle reread. Any logind inhibitor, including ours
-- or `systemd-inhibit`'s, makes the Supervisor hold threshold events and return `Resumed` for work
-- already idle (ADR-0139). `on_resume` zeros `idle.since`, so the handler returns while held; only
-- the master switch is checked here.
--
-- ## Live testing
--
-- Test with the shortest thresholds and `suspend` off. A stage suspending the machine thirty
-- seconds after typing stops cannot be watched firing.
local idle = require("lib.idle")
local store = require("lib.store")
local compositor = require("lib.compositor")

-- `CompositorService.setDisplaysPowered`, spelled per compositor in `lib.compositor`. Pair it with
-- `KeyboardBacklightService.setBlanked`: a lit keyboard under a dark screen means blanking stopped
-- halfway. `backlight_pct` is `-1` without a device (§ 2.8); setting it is a dropped write.
local function set_displays_powered(powered)
    if idle.blanked:get() == (not powered) then
        return
    end
    -- `idle.blanked` is the `dpms` stage's `done` predicate. Setting it on a no-op arms lock and
    -- then suspend over a lit screen, and the guard above refuses every retry.
    if not compositor.detach(powered and "displays_on" or "displays_off") then
        return
    end
    idle.blanked:set(not powered)
    obelisk.keyboard:invoke("set_backlight", powered and 100 or 0)
end

local ACTIONS = {
    dpms = function()
        set_displays_powered(false)
    end,
    -- No "already locked?" guard: `idle.armed` cannot return this stage while locked, its `done`
    -- predicate.
    lock = function()
        obelisk.lock:invoke("lock")
    end,
    suspend = function()
        process.detach("systemctl", { "suspend" })
    end,
}

-- Register once at top level. A registration inside a repeated callback duplicates it permanently.
obelisk.idle:register_threshold(idle.TICK, function()
    local s = obelisk.system:get()
    -- Back-date by the threshold so "idle 0:42" means since the last keystroke, not the push.
    idle.since:set(((s and s.monotonic) or 0) - idle.TICK)
end, function()
    -- Any input wakes it. The mirror uses `respectInhibitors: false`: an inhibitor taken while dark
    -- must not leave the screen dark.
    set_displays_powered(true)
    idle.since:set(0)
    idle.armed_at:set({})
    idle.fired_at:set({})
end)

-- One pass per `obelisk.system` tick, once a second; the bar clock and `power_menu.lua` countdown
-- already use it. Idle work is one comparison, with an early return otherwise.
--
-- This is `IdleService.qml`'s shape, not a scheduler: find the stage armed *now*, stamp it once,
-- and fire after its delay. Clear every other stamp, like `IdleMonitor { enabled: false }` tearing
-- down a timer.
--
-- After unlock, the lock stage arms again and its successor loses its stamp, so the screen does not
-- blank a minute after a lock the user already answered. No unlock watcher is needed; recompute it
-- each second.
obelisk.system:on_change(function(s)
    local since = idle.since:get()
    if since == 0 then
        return
    end
    local settings = idle.read(store.idle:get())
    if not settings.enabled then
        -- Clear on the way out: disabling automation mid-countdown must not leave a stamp that
        -- makes re-enabling resume from where it stopped rather than from now.
        if next(idle.armed_at:get() or {}) ~= nil then
            idle.armed_at:set({})
            idle.fired_at:set({})
        end
        return
    end
    local plan = idle.plan(settings, idle.profile_of(obelisk.power:get()))
    local armed = idle.armed(plan)

    -- Rebuild rather than mutate (`lib/ui_state.lua`): `set` compares table identity, and a fresh
    -- table cannot mutate a value under an unfinished resolve. At most one key survives.
    local stamps = idle.armed_at:get() or {}
    local next_stamps = {}
    if armed then
        -- Use the later of "when this armed" and "when the seat went idle"; active time must not
        -- count toward the stage.
        next_stamps[armed.key] = stamps[armed.key] or math.max(s.monotonic, since)
    end
    idle.armed_at:set(next_stamps)

    if armed and s.monotonic - next_stamps[armed.key] >= armed.delay then
        -- Fire once per arming. A stage with `done` is walked past next tick; a terminal stage
        -- stays armed after acting and would otherwise fire every second.
        local fired = idle.fired_at:get() or {}
        if fired[armed.key] ~= next_stamps[armed.key] then
            idle.fired_at:set({ [armed.key] = next_stamps[armed.key] })
            ACTIONS[armed.key]()
        end
    end
end)

-- Non-button changes to "is something holding this awake". The button uses `idle.set_manual`;
-- settings land on `obelisk.storage`, so disabling "keep awake for media" drops the hold during
-- playback.
obelisk.privacy:on_change(idle.sync_inhibit)
obelisk.mpris:on_change(idle.sync_inhibit)
obelisk.storage:on_change(idle.sync_inhibit)
