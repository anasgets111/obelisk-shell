-- The idle clock. `IdleService.qml`'s three `IdleMonitor`s and its `setDisplaysPowered`, as one
-- threshold and one handler.
--
-- No surface, like `modules/global/power_events.lua`: this file is a registration and two
-- `on_change` handlers, and `shell.lua` requires it for the side effects. `lib/idle.lua` holds
-- everything it acts on, and holds it there so the bar can read the same facts without requiring
-- this file.
--
-- ## Why the clock and not three thresholds
--
-- `lib/idle.lua`'s header has the argument. Short version: nothing can unregister a threshold, so
-- timeouts a panel edits have to be numbers rather than registrations.
--
-- ## What is not guarded here, and why that is the point
--
-- There is no `armed` check, no "is an inhibitor held" test, and no re-read of the manual toggle
-- inside the handler. A logind idle inhibitor -- ours, `systemd-inhibit`'s, anyone's -- makes the
-- Supervisor hold every threshold event and hand back a `Resumed` for anything already idle
-- (ADR-0139), so `on_resume` below runs on the way in, `idle.since` goes to zero, and the handler
-- returns on its first line for as long as the hold lasts. The one thing the framework cannot know
-- is the master switch, which is the one thing checked.
--
-- ## Live-testing this
--
-- Set the thresholds you are testing to their shortest option and leave `suspend` off. A stage that
-- suspends the machine thirty seconds after you stop typing is a stage you cannot watch fire.
local idle = require("lib.idle")
local store = require("lib.store")

local function detached(cmd, args)
    process.run(cmd, args, function() end, function() end)
end

-- `CompositorService.setDisplaysPowered`, which for niri is these two actions. Paired with the
-- keyboard backlight, `KeyboardBacklightService.setBlanked`: a lit keyboard under a dark screen is
-- the tell that the blank did half its job. `backlight_pct` is `-1` on a machine with no backlight
-- device (§ 2.8), and setting it there is a write the capability drops, so there is nothing to
-- check first.
local function set_displays_powered(powered)
    if idle.blanked:get() == (not powered) then
        return
    end
    idle.blanked:set(not powered)
    detached("niri", { "msg", "action", powered and "power-on-monitors" or "power-off-monitors" })
    oblisk.keyboard:invoke("set_backlight", powered and 100 or 0)
end

local ACTIONS = {
    dpms = function()
        set_displays_powered(false)
    end,
    -- Guarded, unlike the other two: `lock:invoke("lock")` on an already-locked session is a
    -- second `ext_session_lock_v1` request, and the stage after this one can fire while the lock
    -- is up. The other two actions are their own guard -- a suspended machine is not idle.
    lock = function()
        local l = oblisk.lock:get()
        if l ~= nil and l.active then
            return
        end
        oblisk.lock:invoke("lock")
    end,
    suspend = function()
        detached("systemctl", { "suspend" })
    end,
}

-- The one registration, at the top level, where `register_threshold`'s contract wants it: a
-- registration made inside a callback that fires more than once registers more than once, and
-- there is no way to take one back.
oblisk.idle:register_threshold(idle.TICK, function()
    local s = oblisk.system:get()
    -- Back-dated by the threshold itself, so "idle 0:42" means forty-two seconds since the last
    -- keystroke rather than since the notification about it.
    idle.since:set(((s and s.time) or 0) - idle.TICK)
    idle.fired:set({})
end, function()
    -- Any input at all, whatever else is true. The mirror's own wake monitor is
    -- `respectInhibitors: false` for exactly this: a screen that stays dark because something took
    -- an inhibitor while it was off is a machine that looks broken.
    set_displays_powered(true)
    idle.since:set(0)
    idle.fired:set({})
end)

-- One tick, one pass over the stages. `oblisk.system` pushes once a second whatever else is
-- happening -- the bar clock and `power_menu.lua`'s countdown both ride it -- so this costs a
-- comparison per second while idle and an early return the rest of the time.
oblisk.system:on_change(function(s)
    local since = idle.since:get()
    if since == 0 then
        return
    end
    local settings = idle.read(store.idle:get())
    if not settings.enabled then
        return
    end
    local plan = idle.plan(settings, idle.profile_of(oblisk.power:get()))
    local elapsed = s.time - since
    local fired = idle.fired:get() or {}
    for _, entry in ipairs(plan.list) do
        if elapsed >= entry.at and not fired[entry.key] then
            local next_fired = {}
            for key, done in pairs(fired) do
                next_fired[key] = done
            end
            next_fired[entry.key] = true
            fired = next_fired
            idle.fired:set(fired)
            ACTIONS[entry.key]()
        end
    end
end)

-- The three things that can change the answer to "is something holding this awake" without anyone
-- clicking the bar button. The button's own edge goes through `idle.set_manual`, and a settings
-- write lands on `oblisk.storage` -- so turning "keep awake for media" off while a film is playing
-- drops the hold immediately rather than at the end of the film.
oblisk.privacy:on_change(idle.sync_inhibit)
oblisk.mpris:on_change(idle.sync_inhibit)
oblisk.storage:on_change(idle.sync_inhibit)
