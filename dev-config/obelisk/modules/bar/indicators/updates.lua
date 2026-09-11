-- Mirrors `ArchChecker.qml`: the glyph says what the updater does; the ground says whether it wants
-- attention.
--
-- Five overlapping states use the mirror's order: a check error beats a stale count and a later
-- spinner. First match wins.
--
-- ADR-0134 adds the "checking" state. `UpdatesState` always carried the in-flight flag; only this
-- indicator treated it as never checked.
--
-- The mirror spins while installing. Here colour carries the state; rotation needs a per-frame
-- property, and ADR-0021's 5ms timer cap provides no timer to drive it.
--
-- Visible when a package manager exists. `LeftSide.qml` wraps it in a `Loader` whose `active` is
-- `UpdateService.ready`, matching `ArchChecker.qml`'s `MainService.isArchBased && command -v
-- checkupdates` gate. `obelisk.updates.package_manager` answers it before the first check.
--
-- Keep it visible when up to date so its idle click can re-check, as the mirror does.
--
-- The click never installs. A direct `install` test launched real `pkexec pacman -Syu` and stopped
-- at "Error creating textual authentication agent"; nothing was upgraded, but that was not by
-- design. `ArchChecker.qml` installs only from the panel, where the package list makes that
-- decision visible.
local theme = require("config.theme")
local icons = require("config.icons")
local icon_button = require("components.icon_button")
local ui_state = require("lib.ui_state")
local update_panel = require("modules.bar.panels.update_panel")
local store = require("lib.store")

-- Updates stay dormant until configured (ADR-0034); otherwise `state_of` remains `idle` and
-- `visible` hides the indicator. Configure here, not `shell.lua`, because this module needs the
-- answer.
--
-- Check hourly, the cadence a pending-updates badge is read rather than the cadence Arch changes.
-- Shorter intervals spend bandwidth on a number that changes a few times a day. Each check runs a
-- real `-Sy` against a mirror; reloads within the hour do not rerun it (ADR-0113 amendment).
--
-- Seed on `obelisk.storage`'s first push, which carries the file `lib/store.lua` declared
-- (ADR-0115, ADR-0136). Persisted `checked_at` and its package list let a restart within the hour
-- skip the check and still show an answer; `previous == nil` seeds once per process.
local UPDATE_INTERVAL = 3600
obelisk.storage:on_change(function(_, previous)
    if previous == nil then
        obelisk.updates:invoke("configure", {
            interval = UPDATE_INTERVAL,
            checked_at = store.updates_checked_at:get(),
            packages = store.updates_packages:get(),
        })
    end
end)

-- On a completed check, remember its time and packages, and announce what is new, as
-- `UpdateService.qml` does.
--
-- "New" compares package names with the stored announced key, like `notifiedPackagesKey`: restarts
-- do not repeat the same twelve packages, and upgraded packages drop out on the next check.
obelisk.updates:on_change(function(u, previous)
    -- Compare with the store, not the previous push: the first post-restart push carries seeded
    -- time.
    if u.last_successful_check and u.last_successful_check ~= store.updates_checked_at:get() then
        store:set("updates_checked_at", u.last_successful_check)
        store:set("updates_packages", u.packages)
    end
    if u.checking or previous == nil or previous.checking ~= true then
        -- Only the push that ends a check has a fresh list.
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
        -- Anchored on newlines: a bare `find` matches inside a neighbour, so a new `python`
        -- counted as already announced whenever `python-pip` was in the stored list.
        if not ("\n" .. announced .. "\n"):find("\n" .. name .. "\n", 1, true) then
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

local status = obelisk.updates:map(state_of)

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
    -- Read at click time: this handler is registered once, while `status` changes.
    if state_of(obelisk.updates:get()) == "idle" then
        -- The mirror's idle click. The Supervisor refuses `check` while one is running, so no guard
        -- is needed.
        obelisk.updates:invoke("check")
        return
    end
    ui_state.toggle_panel(update_panel.kind, rect)
end, {
    slot = "updates",
    -- Accent while this indicator's panel is open.
    selected = ui_state.panel_showing(update_panel.kind),
    -- Hide when the Supervisor has no supported package manager; `package_manager` stays nil, and
    -- an indicator that can only report its own failure is worse than none.
    visible = obelisk.updates:map(function(u)
        return u ~= nil and u.package_manager ~= nil
    end),
    foreground = status:map(function(s)
        if s == "error" then
            return theme.RED
        end
        -- Dim while idle, accent while pending, matching the mirror's "wants attention" split.
        return s == "idle" and theme.DIM or theme.ACCENT
    end),
})
