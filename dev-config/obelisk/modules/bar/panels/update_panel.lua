-- Mirrors `UpdatePanel.qml`: pending updates, download size, and the install button in front of the
-- list. The bar only reports the count; installs happen here (ADR-0113 amendment).
--
-- This file owns wording, formatting, and thresholds; `obelisk.updates` stays unchanged. The
-- Supervisor publishes `install_exit_code` and pacman's output; "failed retrieving file" becomes
-- "could not download; check the connection". Numbers are language-neutral; that sentence is not.
--
-- Not carried over: copy-log button (no clipboard primitive).
-- `modules/bar/indicators/updates.lua` schedules the checks (ADR-0115) on the cadence
-- declared below and installs from the notification action through `install` here.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local panel_card = require("components.panel_card")
local panel_header = require("components.panel_header")
local panel_empty_state = require("components.panel_empty_state")
local spinner = require("components.spinner")
local icon_button = require("components.icon_button")
local action_button = require("components.action_button")
local panel_action_icon = require("components.panel_action_icon")
local panel_row = require("components.panel_row")
local section_header = require("components.section_header")
local toggle = require("components.toggle")
local meter = require("components.meter")
local store = require("lib.store")
local dev_tools = require("config.dev_tools")

local KIND = "updates"
-- Check hourly: a badge is read on that scale, and each check is a real `-Sy` against a mirror. The
-- indicator schedules on it and `last_check_line` calls twice this stale.
local CHECK_INTERVAL = 3600
local PACKAGE_SCROLL = scroll("update_packages")
local LOG_SCROLL = scroll("update_log")

-- Close clears this and the next install sets it. "I have read the result" belongs to the panel,
-- not pacman; QML keeps the same state in `dismissResult()`.
local dismissed = state("updates_result_dismissed", false)

-- Stamp the install-start click: `install_finished_at` is published, and the click is the only
-- known start. Config writes are allowed only in input callbacks (ADR-0044).
local started_at = state("updates_install_started_at", 0)

-- A failed run shows its log unasked; a successful one hides it behind a button, as the mirror's
-- `showCompletedLog` does.
local log_open = state("updates_log_open", false)

-- The tick list replaces the body, as every other view here does.
local settings_open = state("updates_settings_open", false)

-- Which `requires` binaries are on `PATH`. Lua cannot stat `PATH`, so this is probed once per
-- process from the first capability push below; `state` is name-keyed, so an in-place reload keeps
-- the answer rather than blanking the list until the probe re-answers.
local tools_present = state("updates_tools_present", {})

-- The dev chain's only state: which `config/dev_tools.lua` entry is running, and its output.
-- `install_log` stays the capability's; the two are concatenated for display.
local dev_running = state("updates_dev_tool", "")
local dev_log = state("updates_dev_log", {})

local function packages(u)
    return (u and u.packages) or {}
end

-- KiB, MiB, GiB use 1024, matching pacman's package sizes.
local function human_bytes(bytes)
    local size = bytes or 0
    if size < 1024 then
        return string.format("%d B", size)
    end
    for _, unit in ipairs({ "KiB", "MiB", "GiB" }) do
        size = size / 1024
        if size < 1024 or unit == "GiB" then
            return string.format("%.1f %s", size, unit)
        end
    end
    return string.format("%d B", bytes or 0)
end

local function download_total(u)
    local total = 0
    for _, package in ipairs(packages(u)) do
        total = total + (package.download_size or 0)
    end
    return total
end

-- A run that ended. `install_finished_at` marks one the manager answered; a spawn failure publishes
-- only `install_error` and never stamps it (`controller.rs` returns early), and reading just the
-- stamp left that run invisible and reported as a success.
local function install_ended(u)
    return u ~= nil and (u.install_finished_at ~= nil or u.install_error ~= nil)
end

-- True while a finished install's result remains on screen. The capability deliberately has no
-- "completed" state, which would end when read.
local result_showing = computed({ obelisk.updates, dismissed }, function(u, is_dismissed)
    return not is_dismissed and u ~= nil and not u.installing and install_ended(u)
end)

-- A manager killed by a signal publishes no exit code, so an absent one on a run the manager
-- answered is a failure, not a success.
local function install_failed(u)
    return install_ended(u) and (u.install_error ~= nil or u.install_exit_code ~= 0)
end

-- Absent means on, so a tool added to `config/dev_tools.lua` runs without a `state.json` edit.
local function tool_enabled(name)
    return (store.updates_dev_tools:get() or {})[name] ~= false
end

-- Present as well as ticked: a run of nothing but `[SKIP]` lines is not worth a button.
local function any_tool_runnable()
    local present = tools_present:get() or {}
    for _, tool in ipairs(dev_tools) do
        if tool_enabled(tool.name) and present[tool.requires] then
            return true
        end
    end
    return false
end

-- Id 8002, the plain one; `indicators/updates.lua` keeps 8001 for the offer it must replace.
local function toast(urgency, title, body)
    process.detach("notify-send", {
        "-u", urgency, "-a", "System Updates", "-i", "system-software-update", "--replace-id", "8002", title, body,
    })
end

-- Copies to push: mutating the held table leaves the signal's value identical and the scene clean.
-- ponytail: unbounded, unlike the Supervisor's 200-line tail. A dev run prints hundreds of lines,
-- not thousands; cap it here if one ever does.
local function append_dev_log(line)
    local lines = { table.unpack(dev_log:get() or {}) }
    lines[#lines + 1] = line
    dev_log:set(lines)
end

-- Stops at the first non-zero exit.
local function run_commands(commands, index, done)
    local command = commands[index]
    if command == nil then
        return done(true)
    end
    process.run(command[1], { table.unpack(command, 2) }, append_dev_log, function(code)
        if code ~= 0 then
            return done(false)
        end
        run_commands(commands, index + 1, done)
    end)
end

-- Moved off `indicators/updates.lua`'s install edge: only this file knows when the last tool exited.
local function report_run(u, failures)
    if install_failed(u) then
        return toast("critical", "Update failed", "The updates panel has pacman's output")
    end
    if #failures > 0 then
        return toast("critical", "Update finished with failures", table.concat(failures, ", "))
    end
    local count = (u and u.install_total_steps) or 0
    toast("normal", "Update complete", count > 0
        and string.format("%d package%s updated", count, count == 1 and "" or "s")
        or "Developer tooling updated")
end

-- Walks `config/dev_tools.lua`, carrying the failures so far. `command -v` takes the name as `$1`
-- rather than interpolated. `[SKIP]`, `▶` and `[ OK ]` are the markers `log_colour` already tints:
-- the `update` script this replaces printed the same three.
local function run_tools(index, failures)
    local tool = dev_tools[index]
    if tool == nil then
        dev_running:set("")
        return report_run(obelisk.updates:get(), failures)
    end
    if not tool_enabled(tool.name) then
        return run_tools(index + 1, failures)
    end
    process.run("sh", { "-c", 'command -v "$1" >/dev/null', "sh", tool.requires }, function() end, function(code)
        if code ~= 0 then
            append_dev_log(string.format("[SKIP] %s (%s not found)", tool.name, tool.requires))
            return run_tools(index + 1, failures)
        end
        dev_running:set(tool.name)
        append_dev_log("▶ " .. tool.name)
        run_commands(tool.run, 1, function(ok)
            append_dev_log((ok and "[ OK ] " or "[FAIL] ") .. tool.name)
            if not ok then
                failures[#failures + 1] = tool.name
            end
            run_tools(index + 1, failures)
        end)
    end)
end

local function start_dev_tools()
    dev_log:set({})
    run_tools(1, {})
end

-- Exported: the notification's action button in `indicators/updates.lua` is this same click.
--
-- A retry runs with no pending count: a run that failed partway can leave the count at zero with
-- the system still half-upgraded, and refusing there left the failure card holding a dead button.
local function install()
    local u = obelisk.updates:get()
    if u == nil or u.installing or dev_running:get() ~= "" then
        return
    end
    local packages_pending = (u.count or 0) > 0 or install_failed(u)
    if not packages_pending and not any_tool_runnable() then
        return
    end
    started_at:set(os.time())
    dismissed:set(false)
    log_open:set(false)
    settings_open:set(false)
    if packages_pending then
        return obelisk.updates:invoke("install")
    end
    start_dev_tools()
end

-- Replaces `_detectErrorMessage`: pacman's output supplies the reason, and this file turns it into
-- actionable wording. Falls back to the exit code, which is at least true.
local FAILURE_PHRASES = {
    { match = "failed retrieving",            say = "Could not download; check the connection" },
    { match = "could not resolve host",       say = "Could not download; check the connection" },
    { match = "connection refused",           say = "Could not download; check the connection" },
    { match = "not enough free disk space",   say = "Not enough disk space" },
    { match = "invalid or corrupted package", say = "A package failed its signature check" },
    { match = "signature from",               say = "A package failed its signature check" },
    { match = "conflicting files",            say = "Files conflict with another package" },
    { match = "authentication",               say = "Authentication failed" },
}

local function failure_reason(u)
    if u == nil then
        return "The install failed"
    end
    for _, line in ipairs(u.install_log or {}) do
        local lowered = line:lower()
        for _, phrase in ipairs(FAILURE_PHRASES) do
            if lowered:find(phrase.match, 1, true) then
                return phrase.say
            end
        end
    end
    if u.install_error ~= nil then
        return "The updater could not be started"
    end
    if u.install_exit_code == nil then
        return "pacman was killed before it finished"
    end
    return string.format("pacman exited with %d", u.install_exit_code)
end

-- ponytail: `install_log` is the last 200 lines, so a run longer than that undercounts. The
-- Supervisor would have to keep the counter for an exact one.
local function warning_count(u)
    local count = 0
    for _, line in ipairs(u.install_log or {}) do
        if line:lower():find("warning", 1, true) then
            count = count + 1
        end
    end
    return count
end

-- `is_dismissed` is a parameter, not a `dismissed:get()`: a signal read inside a map over
-- `obelisk.updates` alone never re-runs on close, and the card kept reading "update complete" after
-- the buttons under it had gone.
local function status_line(u, is_dismissed, tool)
    if u == nil then
        return "Waiting for the updater"
    end
    if tool ~= "" then
        return "Updating " .. tool
    end
    if u.installing then
        local package = u.install_current_package
        return (package ~= nil and package ~= "") and ("Installing " .. package) or "Starting the install"
    end
    if not is_dismissed and install_ended(u) then
        return install_failed(u) and "Update failed" or "Update complete"
    end
    if u.checking then
        return "Checking"
    end
    if u.check_error ~= nil then
        return "Check failed"
    end
    if (u.count or 0) > 0 then
        return string.format("%d update%s available", u.count, u.count == 1 and "" or "s")
    end
    return "Up to date"
end

local function detail_line(u, is_dismissed, tool)
    if u == nil then
        return ""
    end
    if tool ~= "" then
        return "Developer tooling"
    end
    if u.installing then
        local total = u.install_total_steps or 0
        if total > 0 then
            return string.format("Package %d of %d", u.install_current_step or 0, total)
        end
        -- No step line yet means pacman is downloading, and it prints nothing per package without a
        -- tty. `alpm` already sized the transaction, so say what is being fetched rather than that
        -- we were not told.
        return string.format("Downloading %d package%s · %s", u.count, u.count == 1 and "" or "s",
            human_bytes(download_total(u)))
    end
    if not is_dismissed and install_ended(u) then
        if install_failed(u) then
            return failure_reason(u)
        end
        local warnings = warning_count(u)
        local noted = warnings > 0 and string.format(" · %d warning%s", warnings, warnings == 1 and "" or "s") or ""
        local seconds = u.install_finished_at - (started_at:get() or 0)
        if (started_at:get() or 0) > 0 and seconds >= 0 then
            return string.format("Took %d min %d sec%s", math.floor(seconds / 60), seconds % 60, noted)
        end
        return "Finished" .. noted
    end
    -- A failed check keeps the last good list, so say which list is shown.
    if u.check_error ~= nil then
        local failures = u.consecutive_check_failures or 0
        -- Five consecutive failures is the warning threshold.
        local repeated = failures >= 5 and string.format(" · %d in a row", failures) or ""
        return "Showing the last result" .. repeated
    end
    if (u.count or 0) > 0 then
        return string.format("%s to download", human_bytes(download_total(u)))
    end
    return "Nothing pending"
end

-- Include the date when the check is not today. "checked 07:08" in a shell running since Tuesday
-- falsely reads as this morning; the mirror prints the date unconditionally.
--
-- Past two intervals, say so: a suspended laptop otherwise shows an old count with nothing marking
-- it old. The mirror's `isStale` without its error half, which `detail_line` already covers.
local function last_check_line(u, now)
    if u == nil or u.last_successful_check == nil then
        return "Never checked"
    end
    local at = u.last_successful_check
    local when = os.date("%Y-%m-%d", at) == os.date("%Y-%m-%d") and os.date("%H:%M", at)
        or os.date("%b %d, %H:%M", at)
    return "Checked " .. when .. (now - at > CHECK_INTERVAL * 2 and " · stale" or "")
end

-- Sort by name; `alpm`'s installed-database order has no useful reading order.
local sorted_packages = obelisk.updates:map(function(u)
    local list = {}
    for _, package in ipairs(packages(u)) do
        list[#list + 1] = package
    end
    table.sort(list, function(left, right)
        return (left.name or "") < (right.name or "")
    end)
    return list
end)

-- Two owners, one view: the capability clears `install_log` per install, the chain appends after.
local log_lines = computed({ obelisk.updates, dev_log }, function(u, lines)
    local combined = { table.unpack((u and u.install_log) or {}) }
    for _, line in ipairs(lines or {}) do
        combined[#combined + 1] = line
    end
    return combined
end)

-- Mirror `logColor`: red failures are findable in two hundred lines of pacman output.
local function log_colour(line)
    local lowered = line:lower()
    if lowered:find("[fail]", 1, true) or lowered:find("error", 1, true) or lowered:find("failed", 1, true) then
        return theme.RED
    end
    if lowered:find("warning", 1, true) or lowered:find("[skip]", 1, true) then
        return theme.PEACH
    end
    if lowered:find("downloading", 1, true) or lowered:find("retrieving", 1, true) or lowered:find("installing", 1, true) or lowered:find("upgrading", 1, true) or lowered:find("%(%s*%d+/%d+%)") then
        return theme.ACCENT
    end
    if lowered:find("[ ok ]", 1, true) or lowered:find("complete", 1, true) or lowered:find("up to date", 1, true) then
        return theme.GREEN
    end
    if line:sub(1, 1) == "▶" or line:sub(1, 2) == "::" or line:sub(1, 3) == "==>" then
        return theme.FG
    end
    return theme.DIM
end

-- The tick list takes the whole body, so every other view yields to it.
local function unless_settings(showing)
    return computed({ showing, settings_open }, function(visible, settings)
        return visible and not settings
    end)
end

local packages_showing = unless_settings(computed({ obelisk.updates, result_showing }, function(u, showing)
    return not showing and u ~= nil and not u.installing and #packages(u) > 0
end))

local log_showing = unless_settings(computed({ obelisk.updates, result_showing, log_open, dev_running },
    function(u, showing, open, tool)
        return u ~= nil and (u.installing or tool ~= "" or (showing and (install_failed(u) or open)))
    end))

-- `result_showing`, not `install_ended`: the latter stays true for the rest of the session once one
-- install finishes, and the empty state never came back after it.
local empty_showing = unless_settings(computed({ obelisk.updates, result_showing }, function(u, showing)
    return u ~= nil and not showing and not u.installing and not u.checking and (u.count or 0) == 0
end))

-- The mirror's "Checking…".
local checking_showing = unless_settings(computed({ obelisk.updates, result_showing }, function(u, showing)
    return u ~= nil and not showing and not u.installing and u.checking == true
end))

-- Follow the newest line, as the mirror's `followOutput` does. Every push reveals, not only the
-- lengthening ones: the log is a 200-line tail, so past that the content changes while the length
-- does not.
--
-- Not gated on `installing`: the exit push carries the drained stderr, which is where a failure says
-- why.
--
-- ponytail: unlike the mirror, scrolling back during an install does not stop the follow, so a user
-- reading an earlier line is dragged to the end by the next one. The mirror pauses on
-- `onMovementStarted`; the offset alone cannot stand in for that, because `reveal` lands in a later
-- layout pass than the read (`lua-meta/signals.lua`), so a recorded offset always trails the real
-- one by a reveal and a scroll back above it is indistinguishable from sitting at the end. Wants an
-- engine-side "the wheel moved this viewport" signal.
obelisk.updates:on_change(function(u, previous)
    if previous == nil then
        -- One shell for every tool, not one per tool: this runs on each process start.
        local names = {}
        for _, tool in ipairs(dev_tools) do
            names[#names + 1] = tool.requires
        end
        local found = {}
        process.run("sh", { "-c", 'for n; do command -v "$n" >/dev/null && echo "$n"; done', "sh", table.unpack(names) },
            function(line)
                found[line] = true
            end, function()
                tools_present:set(found)
            end)
    end
    local lines = u ~= nil and #(u.install_log or {}) or 0
    if lines > 0 then
        LOG_SCROLL:reveal(lines)
    end
    -- Keyed on the stamp moving, like `indicators/updates.lua`: pushes coalesce, and a spawn
    -- failure raises and clears `installing` too fast for an edge watcher to see the rise.
    if previous == nil or u == nil or u.installing or not install_ended(u) then
        return
    end
    if u.install_finished_at == previous.install_finished_at and u.install_error == previous.install_error then
        return
    end
    -- Whatever just installed is still in `packages`, and nothing else clears it until the hourly
    -- tick. A half-finished run leaves a stale list too, so re-check on failure as well
    -- (`_finishUpdate`'s unconditional `doPoll`).
    obelisk.updates:invoke("check")
    if install_failed(u) then
        -- A half-upgraded system is the wrong place to rebuild a toolchain against.
        return report_run(u, {})
    end
    start_dev_tools()
end)

-- One row per `config/dev_tools.lua` entry; the subtitle is the binary it needs, so a row ticked on
-- a machine without it reads as the `[SKIP]` it will produce.
local tool_rows = {}
for _, tool in ipairs(dev_tools) do
    tool_rows[#tool_rows + 1] = panel_row {
        title = tool.name,
        subtitle = tool.requires,
        -- Nothing to decide about a tool this machine cannot run.
        visible = tools_present:map(function(present)
            return (present or {})[tool.requires] == true
        end),
        trailing = toggle(store.updates_dev_tools, function(ticked)
            return (ticked or {})[tool.name] ~= false
        end, function(on)
            local ticked = {}
            for key, value in pairs(store.updates_dev_tools:get() or {}) do
                ticked[key] = value
            end
            ticked[tool.name] = on
            store:set("updates_dev_tools", ticked)
        end),
    }
end

-- The mirror's "Working…": installing before pacman has counted the packages.
local working = util.shown_when(obelisk.updates, function(u)
    return u.installing and (u.install_total_steps or 0) == 0
end)

local body = {
    panel_header {
        title = "Updates",
        icon = obelisk.updates:map(function(u)
            if u ~= nil and u.installing then
                return icons.updating
            end
            if u ~= nil and u.checking then
                return icons.checking
            end
            return ((u and u.count) or 0) > 0 and icons.updates or icons.up_to_date
        end),
        active = obelisk.updates:map(function(u)
            return ((u and u.count) or 0) > 0 or (u ~= nil and u.installing)
        end),
        subtitle = computed({ obelisk.updates, obelisk.system }, function(u, clock)
            return last_check_line(u, (clock and clock.time) or os.time())
        end),
        trailing = {
            -- Show only when relevant; a reboot badge after no install warns about nothing.
            cell("Reboot pending", theme.PEACH, theme.font.xs, {
                align_v = "Center",
                visible = util.shown_when(obelisk.updates, function(u)
                    return u.reboot_required == true
                end),
            }),
            panel_action_icon(icons.settings, function()
                settings_open:set(not settings_open:get())
            end, { slot = "updates-settings" }),
            -- Hide refresh while checking/installing; `icon_button` has no disabled state, and a
            -- visible no-op control is worse than a hidden one.
            icon_button(icons.refresh, function()
                obelisk.updates:invoke("check")
            end, {
                size = theme.control.sm,
                icon_size = theme.icon.sm,
                slot = "updates-refresh",
                visible = obelisk.updates:map(function(u)
                    return u == nil or not (u.checking or u.installing)
                end),
            }),
        },
    },
    panel_card({
        cell(computed({ obelisk.updates, dismissed, dev_running }, status_line), theme.FG, theme.font.md),
        cell(computed({ obelisk.updates, dismissed, dev_running }, detail_line), theme.DIM, theme.font.xs),
        -- Determinate only while pacman counts packages. An unanimated indeterminate bar only
        -- repeats "wait"; wrap it because `meter` has no `visible` property.
        row {
            width = "Fill",
            visible = util.shown_when(obelisk.updates, function(u)
                return u.installing and (u.install_total_steps or 0) > 0
            end),
            children = {
                meter(obelisk.updates, function(u)
                    local total = u.install_total_steps or 0
                    if total <= 0 then
                        return 0
                    end
                    return 100 * (u.install_current_step or 0) / total
                end, theme.ACCENT, "Fill"),
            },
        },
        row {
            spacing = theme.spacing.sm,
            align_v = "Center",
            visible = working,
            children = { spinner(working, theme.control.sm), cell("Working…", theme.DIM, theme.font.xs) },
        },
    }, { background = theme.GLASS_CONTENT, width = "Fill", spacing = theme.spacing.xs }),
    -- List: name left, old/new versions in fixed columns, arrow between them. A heading row would
    -- duplicate the table's headings.
    --
    -- Fixed columns keep versions readable down the table; the name takes the remainder and elides.
    --
    -- Own card, like every mirror section. A list on the panel's glass made package names float
    -- over the window behind it.
    panel_card({
        list {
            width = "Fill",
            max_height = theme.update_list_height,
            scroll = PACKAGE_SCROLL,
            spacing = theme.spacing.xs,
            source = sorted_packages,
            itemfn = function(package)
                return row {
                    width = "Fill",
                    height = theme.control.sm,
                    align_v = "Center",
                    spacing = theme.spacing.sm,
                    children = {
                        cell(package.name or "?", theme.FG, theme.font.sm, { width = "Fill", align_v = "Center" }),
                        -- Old version ends at the arrow and new starts there, regardless of length.
                        cell(package.old_version or "", theme.DIM, theme.font.xs, {
                            width = theme.update_version_width,
                            align = "End",
                            align_v = "Center",
                        }),
                        cell("→", theme.DIM, theme.font.xs, { align_v = "Center" }),
                        cell(package.new_version or "", theme.ACCENT, theme.font.xs, {
                            width = theme.update_version_width,
                            align_v = "Center",
                        }),
                    },
                }
            end,
            key = function(package)
                return package.name or "?"
            end,
        },
    }, { background = theme.GLASS_CONTENT, width = "Fill", visible = packages_showing }),
    -- Pacman's output explains failures. Keep it in a card; two hundred lines over live wallpaper
    -- are unreadable without that boundary.
    panel_card({
        list {
            width = "Fill",
            max_height = theme.update_log_height,
            scroll = LOG_SCROLL,
            source = log_lines,
            itemfn = function(line)
                return cell(line, log_colour(line), theme.font.xs, { width = "Fill", wrap = "Word", max_lines = 3 })
            end,
        },
    }, { background = theme.GLASS_CONTENT, width = "Fill", visible = log_showing }),
    panel_empty_state("Nothing to update", empty_showing, { icon = icons.up_to_date }),
    panel_empty_state("Checking…", checking_showing, { icon = spinner(checking_showing, theme.control.sm) }),
    panel_card({ section_header("run with package updates"), column {
        width = "Fill",
        children = tool_rows,
    } }, { background = theme.GLASS_CONTENT, width = "Fill", visible = settings_open }),
    row {
        width = "Fill",
        spacing = theme.spacing.sm,
        children = {
            action_button(
                computed({ obelisk.updates, result_showing }, function(u, showing)
                    return (showing and install_failed(u)) and "Retry" or "Update"
                end),
                install,
                "updates-install",
                {
                    tone = "solid",
                    width = "Fill",
                    visible = computed({ obelisk.updates, result_showing, dev_running }, function(u, showing, tool)
                        if u == nil or u.installing or tool ~= "" then
                            return false
                        end
                        return (u.count or 0) > 0 or (showing and install_failed(u)) or any_tool_runnable()
                    end),
                }
            ),
            action_button("View log", function()
                log_open:set(true)
            end, "updates-log", {
                tone = "quiet",
                width = "Fill",
                visible = computed({ result_showing, obelisk.updates, log_open }, function(showing, u, open)
                    return showing and not open and not install_failed(u)
                end),
            }),
            action_button("Close", function()
                dismissed:set(true)
                log_open:set(false)
            end, "updates-dismiss", { tone = "quiet", width = "Fill", visible = result_showing }),
        },
    },
}

return {
    kind = KIND,
    body = body,
    install = install,
    install_failed = install_failed,
    install_ended = install_ended,
    CHECK_INTERVAL = CHECK_INTERVAL,
}
