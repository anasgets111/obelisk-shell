-- The feed's list half: the popup shows the newest few; this shows the whole feed.
--
-- A feed needs scrolling; a fixed panel showed four and clipped the rest (ADR-0069).
--
-- Rows use `components/notification_card.lua`, so actions, replies, and expanded bodies work here.
-- This file owns the header, DND toggle, and sectioned list.
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
local weather_widget = require("modules.bar.indicators.weather")

local KIND = "notifications"
local SCROLL = scroll("notification_feed")

local function feed(n)
    return (n and n.feed) or {}
end

-- List non-transients (ADR-0100), grouped by application into "urgent" / "today" / "yesterday" /
-- "earlier". `obelisk.applications` supplies desktop-file names/icons (ADR-0101); `obelisk.system`
-- moves "today" at midnight.
local sections = computed({ obelisk.notifications, obelisk.applications, obelisk.system }, function(n, applications, s)
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

-- `criticalCount` drives the urgent badge; exclude transients like `kept` because they never reach
-- this list.
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

-- The panel has room to spell out the day and month; the bar's clock is abbreviated to fit a pill.
local function long_date(seconds)
    local day = tonumber(os.date("%d", seconds)) or 0
    return string.format("%s%s of %s", os.date("%A %d", seconds), ordinal(day),
        os.date("%B %Y %I:%M %p", seconds))
end

local body = {
    -- `NotificationHistoryPanel.qml` opens into weather without a greeting; this sidebar identifies
    -- the session and date before the feed.
    column {
        width = "Fill",
        children = {
            cell(identity.full_name:map(function(name)
                return { { text = name, bold = true } }
            end), theme.FG, theme.font.lg, { width = "Fill" }),
            cell(util.label(obelisk.system, function(s)
                return long_date(s.time)
            end), theme.DIM, theme.font.xs, { width = "Fill" }),
        },
    },
    -- `NotificationHistoryPanel.qml` orders weather, `SystemInfoWidget`, then the notifications
    -- masthead.
    weather_widget("notifications"),
    system_info("notifications"),
    panel_header {
        title = "notifications",
        icon = obelisk.notifications:map(function(n)
            return (n and n.dnd) and icons.bell_off or icons.bell
        end),
        active = obelisk.notifications:map(function(n)
            return not (n and n.dnd)
        end),
        subtitle = util.label(obelisk.notifications, summary),
        trailing = {
            -- `InfoBadge` shows the urgent count before the two controls. Critical notifications
            -- bypass DND and never expire.
            info_badge(obelisk.notifications:map(function(n)
                return string.format("%d urgent", critical_count(n))
            end), theme.RED, {
                visible = util.shown_when(obelisk.notifications, function(n)
                    return critical_count(n) > 0
                end),
            }),
            -- DND is the mirror's third bell state and this control is lit while on. The Supervisor
            -- gates sound (ADR-0033), and the popup reads the same flag, standing down except for
            -- critical notifications.
            icon_button(icons.bell_off, function()
                local n = obelisk.notifications:get()
                obelisk.notifications:invoke("set_dnd", not (n and n.dnd))
            end, {
                size = theme.control.sm,
                icon_size = theme.icon.sm,
                background = obelisk.notifications:map(function(n)
                    return (n and n.dnd) and theme.ACCENT_MEDIUM or theme.GLASS_CONTROL
                end),
                slot = "notification-dnd",
            }),
            -- One `dismiss` per entry; there is no `dismiss_all`. The feed cannot push until this
            -- callback returns, unlike `network_panel.lua`.
            icon_button(icons.clear_all, function()
                for _, notification in ipairs(feed(obelisk.notifications:get())) do
                    obelisk.notifications:invoke("dismiss", notification.id)
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
            obelisk.notifications:invoke("hold_expiry", hovered and 300 or 0)
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
                    -- `groupScope: "history"`: lighter ground inside the glass panel, because the
                    -- popup's heavier ground would read as a second sheet; a timestamp, because
                    -- this list is about when; and no flight in from the right, because there is
                    -- no screen edge here to fly in from.
                    return notification_card(item, ui, { scope = "history" })
                end,
                key = function(item)
                    return item.key
                end,
            },
        },
    },
    panel_empty_state(
        "no notifications",
        util.shown_when(obelisk.notifications, function(n)
            return kept(n) == 0
        end),
        {
            icon = obelisk.notifications:map(function(n)
                return (n and n.dnd) and icons.bell_off or icons.bell
            end),
            -- An empty feed under DND means something different from an empty feed without it, and
            -- the struck-through bell alone does not say which; the mirror spells it out here.
            subtext = obelisk.notifications:map(function(n)
                return (n and n.dnd) and "do not disturb is on" or "you're all caught up"
            end),
        }
    ),
}

return { kind = KIND, body = body }
