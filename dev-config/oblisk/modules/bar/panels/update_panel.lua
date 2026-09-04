-- Mirrors `Modules/Bar/Panels/UpdatePanel.qml`: what is pending, how big it is, and the one button
-- that installs it -- in front of the list, which is the whole reason the panel exists. The bar
-- badge says how many; nothing installs from the bar (ADR-0113 amendment).
--
-- Everything here is wording, formatting and thresholds, and every fact under it comes from
-- `oblisk.updates` unchanged. That split is deliberate and is the answer to "what belongs in the
-- framework": the Supervisor publishes `install_exit_code` and the tail of pacman's own output, and
-- this file is where a non-zero code and a line saying "failed retrieving file" become "check your
-- connection". A number is the same in every language; that sentence is not.
--
-- Two things the mirror has that are not reachable yet and are not faked here: a spinner (nothing
-- animates without a per-frame property, ADR-0021) and a copy-the-log button (no clipboard
-- primitive). The check that resumes its interval across a restart lives in
-- `modules/bar/indicators/updates.lua` now that a config can act when a check succeeds (ADR-0115).
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

-- Cleared by the Close button and set again by the next install, which is why it lives here rather
-- than in the capability: "I have read the result" is a fact about the person, not about pacman.
-- `UpdateService.qml` keeps the same thing in `dismissResult()` because QML has no seam between the
-- service and the panel; we do, so this is the side of it that belongs to the panel.
local dismissed = state("updates_result_dismissed", false)

-- Stamped on the click that starts an install, which is the only way to know how long one took:
-- `install_finished_at` is published, and the start is whoever asked for it. An input callback is
-- also the only place a config may write anything at all (ADR-0044), so the click is not a
-- convenient moment, it is the moment.
local started_at = state("updates_install_started_at", 0)

local function packages(u)
    return (u and u.packages) or {}
end

-- KiB, MiB, GiB against 1024, matching what pacman prints beside a package so the two agree.
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

-- Whether an install has finished and its result is still on screen. Everything the completed and
-- failed states draw hangs off this, and `install_finished_at` is what makes it answerable: the
-- capability has no "completed" of its own, deliberately, because completion stops being
-- interesting the moment somebody has looked at it.
local result_showing = computed({ oblisk.updates, dismissed }, function(u, is_dismissed)
    return not is_dismissed and u ~= nil and not u.installing and u.install_finished_at ~= nil
end)

local function install_failed(u)
    return u ~= nil and ((u.install_exit_code ~= nil and u.install_exit_code ~= 0) or u.install_error ~= nil)
end

-- `_detectErrorMessage`'s job, and the reason `install_error` stopped being a sentence written in
-- Rust: pacman says why in its own output, and turning that into something a person can act on is
-- wording. Falls back to the exit code, which is not helpful but is at least true.
local FAILURE_PHRASES = {
    { match = "failed retrieving", say = "could not download; check the connection" },
    { match = "could not resolve host", say = "could not download; check the connection" },
    { match = "connection refused", say = "could not download; check the connection" },
    { match = "not enough free disk space", say = "not enough disk space" },
    { match = "invalid or corrupted package", say = "a package failed its signature check" },
    { match = "signature from", say = "a package failed its signature check" },
    { match = "conflicting files", say = "files conflict with another package" },
    { match = "authentication", say = "authentication failed" },
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
    -- A failed check keeps the last good list on screen (§ 2.14), so the line says which it is.
    if u.check_error ~= nil then
        local failures = u.consecutive_check_failures or 0
        -- Five in a row is somebody's threshold, and this file is where somebody gets to have one.
        local repeated = failures >= 5 and string.format(" · %d in a row", failures) or ""
        return "showing the last result" .. repeated
    end
    if (u.count or 0) > 0 then
        return string.format("%s to download", human_bytes(download_total(u)))
    end
    return "nothing pending"
end

-- The day as well, once the check is not today's. "checked 07:08" under a shell that has been up
-- since Tuesday reads as this morning, and a check going stale is the one thing this line exists to
-- show. The mirror prints the date unconditionally; here it earns its room only when it says
-- something the time alone does not.
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

-- Sorted by name, which is the order a list of package names is read in. The capability hands them
-- over in `alpm`'s own order, which is the installed database's, and that is no order at all to a
-- reader.
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

-- The mirror's `logColor`, which is the one place a log line's own words are worth reading for
-- colour: a failure in red is findable in two hundred lines of pacman output, and a scroll is not.
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
            -- Only while there is something to say: a reboot badge on a shell that has installed
            -- nothing this session is a warning about nothing.
            cell("reboot pending", theme.PEACH, theme.font.xs, {
                align_v = "Center",
                visible = util.shown_when(oblisk.updates, function(u)
                    return u.reboot_required == true
                end),
            }),
            -- The mirror's header refresh. Hidden rather than greyed while a check or an install is
            -- running: there is no disabled state on `icon_button`, and a control that is there but
            -- does nothing is worse than one that is not there.
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
    -- The status card: what is happening, in one line, with the detail under it.
    panel_card({
        cell(util.label(oblisk.updates, status_line), theme.FG, theme.font.md),
        cell(util.label(oblisk.updates, detail_line), theme.DIM, theme.font.xs),
        -- A determinate bar while pacman is counting packages, and nothing while it is not: an
        -- indeterminate bar with no animation behind it is a rectangle that means "wait", which the
        -- line above already says in words. Wrapped, because `meter` takes no `visible` of its own
        -- and widening a shared component for one caller is the wrong direction.
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
    -- The list: name on the left, the version move in two fixed columns, which is the mirror's three
    -- columns minus its own headings. A heading row over three words is a table of contents for a
    -- table of contents, and the arrow between the two versions says which way they read.
    --
    -- Fixed columns rather than content-sized cells, and that is the whole difference between a
    -- table and five separate two-word sentences: ragged versions have to be read row by row, a
    -- column is read down. The name takes what is left and elides, since a name is what you scan
    -- for and a version is what you check.
    --
    -- In a card of its own, like every section the mirror draws. Not decoration: this panel's ground
    -- is glass, so a list laid straight onto it is a column of package names floating over whichever
    -- window happens to be behind the shell, which is what it looked like.
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
                        -- The old version ends at the arrow and the new one starts from it, so the
                        -- pair sits nose to nose whatever length either happens to be. `DIM` rather
                        -- than the `TEXT_OFF` this had: that is the wash a disabled control wears,
                        -- and a version you are being asked to compare against is not disabled.
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
    -- Pacman's own words, which is where a failure explains itself past the one line above it. In a
    -- card for the reason the list is, and the reason is louder here: two hundred lines of output
    -- over a live wallpaper is unreadable however good the colours are.
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
                    -- The one thing this panel is open for, so the one solid control in the config.
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
