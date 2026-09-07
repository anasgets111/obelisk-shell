-- Mirrors `UpdatePanel.qml`: pending updates, download size, and the install button in front of the
-- list. The bar only reports the count; installs happen here (ADR-0113 amendment).
--
-- This file owns wording, formatting, and thresholds; `oblisk.updates` stays unchanged. The
-- Supervisor publishes `install_exit_code` and pacman's output; here "failed retrieving file" maps
-- to "check your connection". Numbers are language-neutral; that sentence is not.
--
-- Not carried over: spinner (no per-frame property, ADR-0021) and copy-log button (no clipboard
-- primitive). Restart-safe check scheduling lives in `modules/bar/indicators/updates.lua` now that
-- config can act on success (ADR-0115).
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local panel_card = require("components.panel_card")
local panel_header = require("components.panel_header")
local panel_empty_state = require("components.panel_empty_state")
local icon_button = require("components.icon_button")
local action_button = require("components.action_button")
local meter = require("components.meter")

local KIND = "updates"
local PACKAGE_SCROLL = scroll("update_packages")
local LOG_SCROLL = scroll("update_log")

-- Close clears this and the next install sets it. "I have read the result" belongs to the panel,
-- not
-- pacman; QML keeps the same state in `dismissResult()`.
local dismissed = state("updates_result_dismissed", false)

-- Stamp the install-start click: `install_finished_at` is published, and the click is the only
-- known start. Config writes are allowed only in input callbacks (ADR-0044).
local started_at = state("updates_install_started_at", 0)

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

-- True while a finished install's result remains on screen. `install_finished_at` supplies the
-- completion fact; the capability deliberately has no "completed" state, which ends when read.
local result_showing = computed({ oblisk.updates, dismissed }, function(u, is_dismissed)
    return not is_dismissed and u ~= nil and not u.installing and u.install_finished_at ~= nil
end)

local function install_failed(u)
    return u ~= nil and ((u.install_exit_code ~= nil and u.install_exit_code ~= 0) or u.install_error ~= nil)
end

-- Replaces `_detectErrorMessage`: pacman's output supplies the reason, and this file turns it into
-- actionable wording. Falls back to the exit code, which is at least true.
local FAILURE_PHRASES = {
    { match = "failed retrieving",            say = "could not download; check the connection" },
    { match = "could not resolve host",       say = "could not download; check the connection" },
    { match = "connection refused",           say = "could not download; check the connection" },
    { match = "not enough free disk space",   say = "not enough disk space" },
    { match = "invalid or corrupted package", say = "a package failed its signature check" },
    { match = "signature from",               say = "a package failed its signature check" },
    { match = "conflicting files",            say = "files conflict with another package" },
    { match = "authentication",               say = "authentication failed" },
}

local function failure_reason(u)
    if u == nil then
        return "the install failed"
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
        return "the updater could not be started"
    end
    return string.format("pacman exited with %d", u.install_exit_code or -1)
end

local function status_line(u)
    if u == nil then
        return "waiting for the updater"
    end
    if u.installing then
        local package = u.install_current_package
        return (package ~= nil and package ~= "") and ("installing " .. package) or "starting the install"
    end
    if not dismissed:get() and u.install_finished_at ~= nil then
        return install_failed(u) and "update failed" or "update complete"
    end
    if u.checking then
        return "checking"
    end
    if u.check_error ~= nil then
        return "check failed"
    end
    if (u.count or 0) > 0 then
        return string.format("%d update%s available", u.count, u.count == 1 and "" or "s")
    end
    return "up to date"
end

local function detail_line(u)
    if u == nil then
        return ""
    end
    if u.installing then
        local total = u.install_total_steps or 0
        if total > 0 then
            return string.format("package %d of %d", u.install_current_step or 0, total)
        end
        return "pacman has not said how many yet"
    end
    if not dismissed:get() and u.install_finished_at ~= nil then
        if install_failed(u) then
            return failure_reason(u)
        end
        local seconds = u.install_finished_at - (started_at:get() or 0)
        if (started_at:get() or 0) > 0 and seconds >= 0 then
            return string.format("took %d min %d sec", math.floor(seconds / 60), seconds % 60)
        end
        return "finished"
    end
    -- A failed check keeps the last good list (§ 2.14), so say which list is shown.
    if u.check_error ~= nil then
        local failures = u.consecutive_check_failures or 0
        -- Five consecutive failures is the warning threshold.
        local repeated = failures >= 5 and string.format(" · %d in a row", failures) or ""
        return "showing the last result" .. repeated
    end
    if (u.count or 0) > 0 then
        return string.format("%s to download", human_bytes(download_total(u)))
    end
    return "nothing pending"
end

-- Include the date when the check is not today. "checked 07:08" in a shell running since Tuesday
-- falsely reads as this morning; the mirror prints the date unconditionally.
local function last_check_line(u)
    if u == nil or u.last_successful_check == nil then
        return "never checked"
    end
    local at = u.last_successful_check
    if os.date("%Y-%m-%d", at) == os.date("%Y-%m-%d") then
        return "checked " .. os.date("%H:%M", at)
    end
    return "checked " .. os.date("%b %d, %H:%M", at)
end

-- Sort by name. The capability uses `alpm`'s installed-database order, which has no useful reading
-- order.
local sorted_packages = oblisk.updates:map(function(u)
    local list = {}
    for _, package in ipairs(packages(u)) do
        list[#list + 1] = package
    end
    table.sort(list, function(left, right)
        return (left.name or "") < (right.name or "")
    end)
    return list
end)

local log_lines = oblisk.updates:map(function(u)
    return (u and u.install_log) or {}
end)

-- Mirror `logColor`: red failures are findable in two hundred lines of pacman output.
local function log_colour(line)
    local lowered = line:lower()
    if lowered:find("error", 1, true) or lowered:find("failed", 1, true) then
        return theme.RED
    end
    if lowered:find("warning", 1, true) then
        return theme.PEACH
    end
    if lowered:find("installing", 1, true) or lowered:find("upgrading", 1, true) then
        return theme.ACCENT
    end
    return theme.DIM
end

local packages_showing = computed({ oblisk.updates, result_showing }, function(u, showing)
    return not showing and u ~= nil and not u.installing and #packages(u) > 0
end)

local log_showing = computed({ oblisk.updates, result_showing }, function(u, showing)
    return u ~= nil and (u.installing or (showing and install_failed(u)))
end)

local body = {
    panel_header {
        title = "updates",
        icon = oblisk.updates:map(function(u)
            if u ~= nil and u.installing then
                return icons.updating
            end
            if u ~= nil and u.checking then
                return icons.checking
            end
            return ((u and u.count) or 0) > 0 and icons.updates or icons.up_to_date
        end),
        active = oblisk.updates:map(function(u)
            return ((u and u.count) or 0) > 0 or (u ~= nil and u.installing)
        end),
        subtitle = util.label(oblisk.updates, last_check_line),
        trailing = {
            -- Show only when relevant; a reboot badge after no install warns about nothing.
            cell("reboot pending", theme.PEACH, theme.font.xs, {
                align_v = "Center",
                visible = util.shown_when(oblisk.updates, function(u)
                    return u.reboot_required == true
                end),
            }),
            -- Hide refresh while checking/installing; `icon_button` has no disabled state, and a
            -- visible no-op control is worse than a hidden one.
            icon_button(icons.refresh, function()
                oblisk.updates:invoke("check")
            end, {
                size = theme.control.sm,
                icon_size = theme.icon.sm,
                slot = "updates-refresh",
                visible = oblisk.updates:map(function(u)
                    return u == nil or not (u.checking or u.installing)
                end),
            }),
        },
    },
    -- Status and detail.
    panel_card({
        cell(util.label(oblisk.updates, status_line), theme.FG, theme.font.md),
        cell(util.label(oblisk.updates, detail_line), theme.DIM, theme.font.xs),
        -- Determinate only while pacman counts packages. An unanimated indeterminate bar only
        -- repeats
        -- "wait"; wrap it because `meter` has no `visible` property.
        row {
            width = "Fill",
            visible = util.shown_when(oblisk.updates, function(u)
                return u.installing and (u.install_total_steps or 0) > 0
            end),
            children = {
                meter(oblisk.updates, function(u)
                    local total = u.install_total_steps or 0
                    if total <= 0 then
                        return 0
                    end
                    return 100 * (u.install_current_step or 0) / total
                end, theme.ACCENT, "Fill"),
            },
        },
    }, { background = theme.GLASS_CONTENT, width = "Fill", spacing = theme.spacing.xs }),
    -- List: name left, old/new versions in fixed columns, arrow between them. A heading row would
    -- duplicate the table's headings.
    --
    -- Fixed columns make versions readable down the table; the name takes the remainder and elides.
    --
    -- Own card, like every mirror section. A list on the panel's glass looked like package names
    -- floating over the window behind it.
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
                        -- Use `DIM`, not `TEXT_OFF`: the version is not a disabled control.
                        cell(package.old_version or "", theme.DIM, theme.font.xs, {
                            width = theme.update_version_width,
                            align = "End",
                            align_v = "Center",
                        }),
                        cell("→", theme.TEXT_OFF, theme.font.xs, { align_v = "Center" }),
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
    panel_empty_state("nothing to update", util.shown_when(oblisk.updates, function(u)
        return not u.installing and not u.checking and (u.count or 0) == 0 and u.install_finished_at == nil
    end), { icon = icons.up_to_date }),
    row {
        width = "Fill",
        spacing = theme.spacing.sm,
        children = {
            action_button(
                oblisk.updates:map(function(u)
                    return install_failed(u) and "retry" or "update"
                end),
                function()
                    local u = oblisk.updates:get()
                    if u == nil or (u.count or 0) == 0 or u.installing then
                        return
                    end
                    started_at:set(os.time())
                    dismissed:set(false)
                    oblisk.updates:invoke("install")
                end,
                "updates-install",
                {
                    -- The panel's purpose, and its only solid control.
                    tone = "solid",
                    width = "Fill",
                    visible = oblisk.updates:map(function(u)
                        return u ~= nil and not u.installing and (u.count or 0) > 0
                    end),
                }
            ),
            action_button("close", function()
                dismissed:set(true)
            end, "updates-dismiss", { tone = "quiet", width = "Fill", visible = result_showing }),
        },
    },
}

return { kind = KIND, body = body }
