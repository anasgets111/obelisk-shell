-- Mirrors ArchChecker.qml: one glyph whose shape says what the updater is doing and whose ground
-- says whether it wants attention.
--
-- Five states, tested in the mirror's own order, because they overlap: an error that happened
-- during a check still has a stale count sitting behind it, and a check running after one still
-- reports the error rather than hiding it behind a spinner. First match wins and the order is what
-- makes that right.
--
-- "checking" is the state this could not draw until ADR-0134: `UpdatesState` always carried the
-- in-flight flag, and the panel header has been reading it all along -- this file was the one place
-- still treating a check in flight and a check never run as the same thing.
--
-- The mirror spins the glyph while installing. Nothing here animates, so the state is carried by
-- the glyph's colour alone; a rotation would need a per-frame property and there is no timer under
-- ADR-0021's 5ms cap that would drive one.
--
-- Present whenever this machine has a package manager at all, which is `ArchChecker.qml`'s own gate
-- one layer down: `LeftSide.qml` wraps it in a `Loader` whose `active` is `UpdateService.ready`,
-- and that is `MainService.isArchBased && command -v checkupdates`. `oblisk.updates` answers the
-- same question in `package_manager`, and answers it at startup rather than after the first check.
--
-- This was absent whenever it had nothing to say, and the argument for that was sound as far as it
-- went: a permanent circle whose one meaning is "no action available" is a control that never does
-- anything, and the idle click was a no-op that proved it. The mirror has no such circle either --
-- its idle click re-checks. So does this one now, which is what earns the pixels back. A bar whose
-- update indicator vanishes when you are up to date is a bar with no way to ask.
--
-- The click never installs. It briefly invoked `install` directly, and the day the module was first
-- switched on, one click launched a real `pkexec pacman -Syu` -- it got no further than "Error
-- creating textual authentication agent", so nothing was upgraded, but nothing about that was by
-- design either. `ArchChecker.qml` never installs from the bar for the same reason: installing is a
-- decision made in front of the package list, which is what the panel is.
local theme = require("config.theme")
local icons = require("config.icons")
local icon_button = require("components.icon_button")
local ui_state = require("lib.ui_state")
local update_panel = require("modules.bar.panels.update_panel")
local store = require("lib.store")

-- Nothing checks for updates until a config names an interval (ADR-0034), so without this line the
-- capability starts, stays dormant, and the indicator below is invisible forever -- `state_of` reads
-- `idle` off a state nothing ever wrote, and `visible` hides `idle`. Here rather than in `shell.lua`
-- for `system_info.lua`'s reason: the module that wants the answer says how often.
--
-- An hour, which is the cadence a pending-updates badge is read at, not the cadence Arch moves at. A
-- check is a real `-Sy` against a mirror; anything much shorter is bandwidth spent on a number that
-- changes a few times a day. A config reload inside the hour does not re-run it (ADR-0113 amendment).
--
-- Sent on `oblisk.storage`'s first push rather than at load, because that push is what carries the
-- file `lib/store.lua` declared (ADR-0115, ADR-0136): the time of the last check that succeeded is
-- remembered there, below, so a shell restart inside the hour does not re-run the check either.
-- `previous == nil` is that first push and nothing else; a reload re-registers this handler, but
-- the payload it would compare against is already there, so the seed is sent exactly once per
-- process.
local UPDATE_INTERVAL = 3600
oblisk.storage:on_change(function(_, previous)
    if previous == nil then
        local checked_at = store.updates_checked_at:get()
        oblisk.updates:invoke("configure", { interval = UPDATE_INTERVAL, checked_at = checked_at })
    end
end)

-- The two things the reference `UpdateService.qml` does when a check comes back, neither of which
-- had a place to run before `on_change`: remember when, and say what is new.
--
-- "New" is against the names already announced, kept in the store as the mirror keeps
-- `notifiedPackagesKey`, so a restart does not re-announce the same twelve packages, and a package
-- that got upgraded elsewhere falls out of the key when the next check no longer lists it.
oblisk.updates:on_change(function(u, previous)
    -- Against the file, not the previous push: the first push after a restart carries the time this
    -- file seeded, and writing it back would touch the store on every start for nothing.
    if u.last_successful_check and u.last_successful_check ~= store.updates_checked_at:get() then
        store:set("updates_checked_at", u.last_successful_check)
    end
    if u.checking or previous == nil or previous.checking ~= true then
        -- Only the push that ends a check, which is the one whose list is fresh.
        return
    end
    local announced = store.updates_notified:get() or ""
    local names = {}
    for _, package in ipairs(u.packages) do
        names[#names + 1] = package.name
    end
    table.sort(names)
    local key = table.concat(names, "\n")
    if key == announced then
        return
    end
    store:set("updates_notified", key)
    local fresh = 0
    for _, name in ipairs(names) do
        if not announced:find(name, 1, true) then
            fresh = fresh + 1
        end
    end
    if fresh == 0 then
        return
    end
    local body = fresh == 1 and string.format("One new package can be upgraded (%d)", u.count)
        or string.format("%d new packages can be upgraded (%d)", fresh, u.count)
    process.run("notify-send", {
        "-u", "normal", "-a", "System Updates", "-i", "system-software-update", "Updates Available", body,
    }, function() end, function() end)
end)

local function state_of(u)
    if u == nil then
        return "idle"
    end
    if u.installing then
        return "installing"
    end
    if u.check_error and u.check_error ~= "" then
        return "error"
    end
    if u.checking then
        return "checking"
    end
    if (u.count or 0) > 0 then
        return "pending"
    end
    return "idle"
end

local status = oblisk.updates:map(state_of)

return icon_button(status:map(function(s)
    if s == "installing" then
        return icons.updating
    elseif s == "error" then
        return icons.update_err
    elseif s == "checking" then
        return icons.checking
    elseif s == "pending" then
        return icons.updates
    end
    return icons.up_to_date
end), function(rect)
    -- Read at click time rather than off a captured value: `status` is a signal, and a handler
    -- registered once has to ask what the state is now, not what it was when the config loaded.
    if state_of(oblisk.updates:get()) == "idle" then
        -- The mirror's idle click, and the reason this circle is allowed to exist while there is
        -- nothing pending. A `check` while one is already running is refused by the Supervisor, so
        -- the double-click case needs no guard here.
        oblisk.updates:invoke("check")
        return
    end
    ui_state.toggle_panel(update_panel.kind, rect)
end, {
    slot = "updates",
    -- The accent ring every other indicator wears while its own panel is the one on screen.
    selected = ui_state.panel_showing(update_panel.kind),
    -- Nothing to check with means nothing to show: on a machine whose package manager this
    -- Supervisor does not speak, `package_manager` is nil and stays nil, and an indicator that can
    -- only ever report its own failure is worse than no indicator.
    visible = oblisk.updates:map(function(u)
        return u ~= nil and u.package_manager ~= nil
    end),
    foreground = status:map(function(s)
        if s == "error" then
            return theme.RED
        end
        -- Dim while there is nothing waiting, accent once there is: the same "wants attention"
        -- split the ground carries in the mirror, applied to the glyph because this bar tints the
        -- glyph and leaves the circle alone.
        return s == "idle" and theme.DIM or theme.ACCENT
    end),
})
