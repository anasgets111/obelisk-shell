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
    -- No "already locked?" guard, because `idle.armed` cannot hand this stage back while the lock
    -- is up: that is the stage's own `done` predicate, and one answer to a question beats two.
    lock = function()
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
end, function()
    -- Any input at all, whatever else is true. The mirror's own wake monitor is
    -- `respectInhibitors: false` for exactly this: a screen that stays dark because something took
    -- an inhibitor while it was off is a machine that looks broken.
    set_displays_powered(true)
    idle.since:set(0)
    idle.armed_at:set({})
end)

-- One tick, one pass. `oblisk.system` pushes once a second whatever else is happening -- the bar
-- clock and `power_menu.lua`'s countdown both ride it -- so this costs a comparison per second
-- while idle and an early return the rest of the time.
--
-- The shape is `IdleService.qml`'s, not a scheduler: work out which stage is armed *now*, stamp it
-- the first time it arms, and fire it its own delay after that stamp. Every stage that is not the
-- armed one has its stamp cleared, which is `IdleMonitor { enabled: false }` tearing a timer down.
--
-- That clearing is the whole reason the file is written this way. Unlocking makes the lock stage
-- undone, so it becomes the armed stage again and the stage behind it loses its stamp: the screen
-- stops being due to blank a minute after a lock the user has already answered. Nothing here
-- watches for an unlock; it falls out of asking the question every second instead of latching it.
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
    local armed = idle.armed(plan)

    -- Rebuilt rather than mutated, `lib/ui_state.lua`'s rule: `set` compares a table by identity,
    -- so a fresh one is both what makes the write land and what keeps the value under an unfinished
    -- resolve from being mutated. One key at most survives, so this stays a two-entry table.
    local stamps = idle.armed_at:get() or {}
    local next_stamps = {}
    if armed then
        -- The stamp is the later of "when this armed" and "when the seat went idle": a stage armed
        -- while the user was active must not count the time they were using the machine.
        next_stamps[armed.key] = stamps[armed.key] or math.max(s.time, since)
    end
    idle.armed_at:set(next_stamps)

    if armed and s.time - next_stamps[armed.key] >= armed.delay then
        ACTIONS[armed.key]()
    end
end)

-- The three things that can change the answer to "is something holding this awake" without anyone
-- clicking the bar button. The button's own edge goes through `idle.set_manual`, and a settings
-- write lands on `oblisk.storage` -- so turning "keep awake for media" off while a film is playing
-- drops the hold immediately rather than at the end of the film.
oblisk.privacy:on_change(idle.sync_inhibit)
oblisk.mpris:on_change(idle.sync_inhibit)
oblisk.storage:on_change(idle.sync_inhibit)
