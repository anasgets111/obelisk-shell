-- The list half of § 2.7: the popup shows the newest few for as long as the Supervisor keeps them;
-- this shows the whole feed.
--
-- A feed needs scrolling; a fixed panel showed four and clipped the rest (ADR-0069).
--
-- Rows use the popup's `components/notification_card.lua`, so actions, replies, and expanded bodies
-- work here too. This file owns the header, DND toggle, and sectioned list.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local ui = require("lib.ui_state")
local cell = require("components.cell")
local section_header = require("components.section_header")
local panel_header = require("components.panel_header")
local panel_empty_state = require("components.panel_empty_state")
local icon_button = require("components.icon_button")
local notification_card = require("components.notification_card")
local info_badge = require("components.info_badge")
local identity = require("lib.identity")
local system_info = require("modules.bar.indicators.system_info")

local KIND = "notifications"
local SCROLL = scroll("notification_feed")

local function feed(n)
    return (n and n.feed) or {}
end

-- List non-transients (ADR-0100), grouped by application into "urgent" / "today" / "yesterday" /
-- "earlier". `oblisk.applications` supplies desktop-file names/icons (ADR-0101); `oblisk.system`
-- moves "today" at midnight.
local sections = computed({ oblisk.notifications, oblisk.applications, oblisk.system }, function(n, applications, s)
    local groups = util.group_notifications(feed(n), applications, { skip_transient = true })
    return util.notification_sections(groups, (s and s.time) or 0)
end)

local function kept(n)
    local count = 0
    for _, notification in ipairs(feed(n)) do
        if not notification.transient then
            count = count + 1
        end
    end
    return count
end

-- `historySummary`: count, application count, and DND state. A bare "5" did not say five what.
local function summary(n)
    local count, apps, seen = 0, 0, {}
    for _, notification in ipairs(feed(n)) do
        if not notification.transient then
            count = count + 1
            local app = notification.app_name or ""
            if not seen[app] then
                seen[app] = true
                apps = apps + 1
            end
        end
    end
    local dnd = n and n.dnd
    if count == 0 then
        return dnd and "silenced · history empty" or "history empty"
    end
    local parts = { string.format("%d in history", count) }
    if apps > 1 then
        parts[#parts + 1] = string.format("%d apps", apps)
    end
    if dnd then
        parts[#parts + 1] = "silenced"
    end
    return table.concat(parts, " · ")
end

-- `criticalCount`, the number behind the header's urgent badge. Transients are excluded for the
-- same reason as `kept`: they never reach this list, so counting them would badge rows that are
-- not here.
local function critical_count(n)
    local count = 0
    for _, notification in ipairs(feed(n)) do
        if not notification.transient and notification.urgency == "critical" then
            count = count + 1
        end
    end
    return count
end

-- "1st", "2nd", "3rd", "4th"; the teens are the exception, all "th".
local function ordinal(day)
    local tens = day % 100
    if tens >= 11 and tens <= 13 then
        return "th"
    end
    local ones = day % 10
    return ones == 1 and "st" or ones == 2 and "nd" or ones == 3 and "rd" or "th"
end

-- "Tuesday 08th of September 2026 03:07 PM". The bar's clock is abbreviated to fit a pill; this has
-- a panel's width, so it spells the day and month out and there is no second place to look.
local function long_date(seconds)
    local day = tonumber(os.date("%d", seconds)) or 0
    return string.format("%s%s of %s", os.date("%A %d", seconds), ordinal(day),
        os.date("%B %Y %I:%M %p", seconds))
end

local body = {
    -- Not the mirror's. `NotificationHistoryPanel.qml` opens straight into the weather, having no
    -- greeting anywhere; this panel is wide enough to be read as a sidebar, and a sidebar that
    -- never says whose session it is or what day it is was the gap.
    column {
        width = "Fill",
        children = {
            cell(identity.full_name:map(function(name)
                return { { text = name, bold = true } }
            end), theme.FG, theme.font.lg, { width = "Fill" }),
            cell(util.label(oblisk.system, function(s)
                return long_date(s.time)
            end), theme.DIM, theme.font.xs, { width = "Fill" }),
        },
    },
    -- `NotificationHistoryPanel.qml` opens with the weather, then the system readout, and only then
    -- the notifications masthead: the panel is the shell's status sheet, and the feed is its
    -- longest section rather than its subject. The weather half has no § 2.x capability behind it
    -- and is absent; the system half is `SystemInfoWidget`.
    system_info("notifications"),
    -- Shared masthead shape: bell, DND-dimmed when silenced, summary, and two trailing controls.
    panel_header {
        title = "notifications",
        icon = oblisk.notifications:map(function(n)
            return (n and n.dnd) and icons.bell_off or icons.bell
        end),
        active = oblisk.notifications:map(function(n)
            return not (n and n.dnd)
        end),
        subtitle = util.label(oblisk.notifications, summary),
        trailing = {
            -- `InfoBadge`, ahead of the two controls and shown only while something is critical.
            -- Critical notifications bypass DND and never expire, so the count is what the panel
            -- most needs to say before its list is read.
            info_badge(oblisk.notifications:map(function(n)
                return string.format("%d urgent", critical_count(n))
            end), theme.RED, {
                visible = util.shown_when(oblisk.notifications, function(n)
                    return critical_count(n) > 0
                end),
            }),
            -- DND is the mirror's third bell state and this control is lit while on. The Supervisor
            -- gates sound (ADR-0033), and the popup reads the same flag, standing down except for
            -- critical notifications.
            icon_button(icons.bell_off, function()
                local n = oblisk.notifications:get()
                oblisk.notifications:invoke("set_dnd", not (n and n.dnd))
            end, {
                size = theme.control.sm,
                icon_size = theme.icon.sm,
                background = oblisk.notifications:map(function(n)
                    return (n and n.dnd) and theme.ACCENT_MEDIUM or theme.GLASS_CONTROL
                end),
                slot = "notification-dnd",
            }),
            -- One `dismiss` per entry; § 3.2 has no `dismiss_all`. No copy is needed: the feed
            -- cannot
            -- push until this callback returns, unlike `network_panel.lua`.
            icon_button(icons.clear_all, function()
                for _, notification in ipairs(feed(oblisk.notifications:get())) do
                    oblisk.notifications:invoke("dismiss", notification.id)
                end
            end, { size = theme.control.sm, icon_size = theme.icon.sm }),
        },
    },
    column {
        width = "Fill",
        -- Same hold as the popup (ADR-0094): expiry must not reorder the list under a pointer. The
        -- history and popup regions are separate because their surfaces never overlap.
        hover = hover("notification_history_region"),
        on_hover = function(hovered)
            oblisk.notifications:invoke("hold_expiry", hovered and 300 or 0)
        end,
        children = {
            -- Card height up to the screen cap, then a scrolling viewport (ADR-0110), matching
            -- `maxAvailableHeight`.
            list {
                width = "Fill",
                max_height = theme.notification_list_height,
                scroll = SCROLL,
                spacing = theme.spacing.sm,
                source = sections,
                itemfn = function(item)
                    if item.kind == "header" then
                        return section_header(item.label)
                    end
                    -- Lighter ground inside the glass panel; the popup's heavier ground would read
                    -- as a second sheet. History also shows the time.
                    return notification_card(item, ui, { background = theme.GLASS_CONTENT, show_time = true })
                end,
                key = function(item)
                    return item.key
                end,
            },
        },
    },
    panel_empty_state(
        "no notifications",
        util.shown_when(oblisk.notifications, function(n)
            return kept(n) == 0
        end),
        {
            icon = oblisk.notifications:map(function(n)
                return (n and n.dnd) and icons.bell_off or icons.bell
            end),
            -- An empty feed under DND means something different from an empty feed without it, and
            -- the struck-through bell alone does not say which; the mirror spells it out here.
            subtext = oblisk.notifications:map(function(n)
                return (n and n.dnd) and "do not disturb is on" or "you're all caught up"
            end),
        }
    ),
}

return { kind = KIND, body = body }
