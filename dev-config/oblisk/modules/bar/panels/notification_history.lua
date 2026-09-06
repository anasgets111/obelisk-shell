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
local section_header = require("components.section_header")
local panel_header = require("components.panel_header")
local panel_empty_state = require("components.panel_empty_state")
local icon_button = require("components.icon_button")
local notification_card = require("components.notification_card")

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

local body = {
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
    panel_empty_state("nothing waiting", util.shown_when(oblisk.notifications, function(n)
        return kept(n) == 0
    end)),
}

return { kind = KIND, body = body }
