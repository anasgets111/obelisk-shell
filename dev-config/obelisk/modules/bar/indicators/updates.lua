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
-- decision visible. The notification's action button is the mirror's one exception, and this one's.
local theme = require("config.theme")
local icons = require("config.icons")
local cell = require("components.cell")
local icon_button = require("components.icon_button")
local tooltip = require("components.tooltip")
local util = require("lib.util")
local ui_state = require("lib.ui_state")
local update_panel = require("modules.bar.panels.update_panel")
local store = require("lib.store")

local SLOT = "updates"

-- The panel owns "I have read the result"; `state(...)` is name-keyed, so this is that same signal.
-- Without it the badge would stay red until the next install rather than until the user closes the
-- result, which is where `ArchChecker.qml` clears it (`dismissResult()`).
local dismissed = state("updates_result_dismissed", false)

-- Two ids, as the mirror uses: the actionable offer owns 8001 and every plain toast 8002, so a
-- completion or a failed-check warning neither replaces an offer to install nor is replaced by one.
--
-- Only the actionable one is held: `--wait` keeps that process open until the toast is answered and
-- prints the chosen action key on stdout, so an unanswered one is killed before the next. A plain
-- toast exits immediately and has nothing to hold.
local live_toast = nil
local function toast(urgency, title, body, action)
    local args = { "-u", urgency, "-a", "System Updates", "-i", "system-software-update",
        "--replace-id", action and "8001" or "8002" }
    if action then
        args[#args + 1] = "--wait"
        args[#args + 1] = "-A"
        args[#args + 1] = "run-updates=" .. action
    end
    args[#args + 1] = title
    args[#args + 1] = body
    if action and live_toast then
        live_toast:kill()
    end
    local handle
    handle = process.run("notify-send", args, function(line)
        if line:find("run-updates", 1, true) then
            update_panel.install()
        end
    end, function()
        -- The one just killed exits after its replacement is live; only clear yourself.
        if live_toast == handle then
            live_toast = nil
        end
    end)
    -- Only the actionable one is tracked; letting a plain toast take the slot would orphan an
    -- unanswered offer and leave `live_toast` pointing at a process that has already exited.
    if action then
        live_toast = handle
    end
end

-- Updates stay dormant until configured (ADR-0034); otherwise `state_of` remains `idle` and
-- `visible` hides the indicator. Configure here, not `shell.lua`, because this module needs the
-- answer. The cadence is `update_panel.CHECK_INTERVAL`, which also sets what counts as stale there.
--
-- Seed on `obelisk.storage`'s first push, which carries the file `lib/store.lua` declared
-- (ADR-0115, ADR-0136). Persisted `checked_at` and its package list let a restart within the hour
-- skip the check and still show an answer; `previous == nil` seeds once per process.
obelisk.storage:on_change(function(_, previous)
    if previous == nil then
        obelisk.updates:invoke("configure", {
            interval = update_panel.CHECK_INTERVAL,
            checked_at = store.updates_checked_at:get(),
            packages = store.updates_packages:get(),
        })
    end
end)

-- On a completed check, remember its time and packages, and announce what is new, as
-- `UpdateService.qml` does. The install's own outcome is reported by `panels/update_panel.lua`,
-- which is the only file that knows when the developer tooling behind it has finished too.
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
    -- Every fifth consecutive failure, matching the mirror's threshold: the count on the bar is no
    -- longer the system's answer, and only the panel says so.
    local failures = u.consecutive_check_failures or 0
    if failures > 0 and failures % 5 == 0 and previous ~= nil and (previous.consecutive_check_failures or 0) ~= failures then
        toast("critical", "Update check failed", u.check_error or "")
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
    toast("normal", "Updates Available", body, "Run updates")
end)

-- The mirror's order: installing, then a failed run, then a failed check, then a running check,
-- then a count. `ArchChecker.qml` reads `isError` off the install state machine, so a failed
-- install owns the glyph until the result is read.
local function state_of(u, is_dismissed)
    if u == nil then
        return "idle"
    end
    if u.installing then
        return "installing"
    end
    if not is_dismissed and update_panel.install_failed(u) then
        return "install_failed"
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

local status = computed({ obelisk.updates, dismissed }, state_of)

local indicator = icon_button(status:map(function(s)
    if s == "installing" then
        return icons.updating
    elseif s == "error" or s == "install_failed" then
        return icons.update_err
    elseif s == "checking" then
        return icons.checking
    elseif s == "pending" then
        return icons.updates
    end
    return icons.up_to_date
end), nil, {
    -- Right-click always opens the panel, as the mirror does: at idle it is otherwise unreachable,
    -- and with it the reboot badge, the last check time, and the empty state.
    on_button = function(rect, mouse_button)
        if mouse_button ~= "left" and mouse_button ~= "right" then
            return
        end
        -- Read at click time: this handler is registered once, while `status` changes.
        if mouse_button == "right" or state_of(obelisk.updates:get(), dismissed:get()) ~= "idle" then
            ui_state.toggle_panel(update_panel.kind, rect)
            return
        end
        -- The mirror's idle click. The Supervisor refuses `check` while one is running, so no guard
        -- is needed.
        obelisk.updates:invoke("check")
    end,
    slot = SLOT,
    -- Accent while this indicator's panel is open.
    selected = ui_state.panel_showing(update_panel.kind),
    -- Hide when the Supervisor has no supported package manager; `package_manager` stays nil, and
    -- an indicator that can only report its own failure is worse than none.
    visible = obelisk.updates:map(function(u)
        return u ~= nil and u.package_manager ~= nil
    end),
    foreground = status:map(function(s)
        if s == "error" or s == "install_failed" then
            return theme.RED
        end
        -- Dim while idle, accent while pending, matching the mirror's "wants attention" split.
        return s == "idle" and theme.DIM or theme.ACCENT
    end),
})

-- The glyph and its colour leave five states sharing two grounds; `tooltipText` names which.
local update_tooltip = tooltip({
    id = "updates_tooltip",
    slot = SLOT,
    children = {
        cell(computed({ obelisk.updates, dismissed }, function(u, is_dismissed)
            if u == nil then
                return "--"
            end
            local current = state_of(u, is_dismissed)
            if current == "installing" then
                local package = u.install_current_package
                return (package ~= nil and package ~= "") and ("installing " .. package) or "installing"
            end
            if current == "install_failed" then
                return "update failed, click for details"
            end
            if current == "error" then
                return "check failed, click for details"
            end
            if current == "checking" then
                return "checking for updates"
            end
            if current == "pending" then
                return u.count == 1 and "one package can be upgraded"
                    or string.format("%d packages can be upgraded", u.count)
            end
            return "up to date, right-click for the updater"
        end), theme.FG, theme.font.sm),
    },
})

return { indicator = indicator, tooltip = update_tooltip }
