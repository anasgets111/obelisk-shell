-- The notification feed as a list, which is the half of § 2.7 nothing was reading. The popup in
-- `modules/notification/popup.lua` shows the newest one for as long as the Supervisor keeps it in
-- the feed (ADR-0033); this is where the rest of them are.
--
-- Needed a scrolling container to exist at all: a feed is however many notifications have arrived,
-- so a fixed panel could show the first four and clip the rest with no way to reach them
-- (ADR-0069).
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local section_header = require("components.section_header")
local panel_row = require("components.panel_row")
local panel_empty_state = require("components.panel_empty_state")
local icon_button = require("components.icon_button")

local KIND = "notifications"
local SCROLL = scroll("notification_feed")

local function feed(n)
    return (n and n.feed) or {}
end

local body = {
    row {
        width = "Fill",
        align_v = "Center",
        spacing = theme.spacing.sm,
        children = {
            section_header("notifications"),
            cell(util.label(oblisk.notifications, function(n)
                return string.format("%d", #feed(n))
            end), theme.TEXT_OFF, theme.font.xs, { width = "Fill", align = "End" }),
            -- One `dismiss` per entry, because § 3.2 has no `dismiss_all`. Iterating a copy is
            -- not needed here the way it is in `network_panel.lua`: nothing pushes a new feed
            -- until this returns, so the list being walked cannot change underneath it.
            icon_button(icons.clear_all, function()
                for _, notification in ipairs(feed(oblisk.notifications:get())) do
                    oblisk.notifications:invoke("dismiss", notification.id)
                end
            end, { size = theme.control.xs, icon_size = theme.icon.xs }),
        },
    },
    list {
        width = "Fill",
        height = "Fill",
        scroll = SCROLL,
        spacing = theme.spacing.xs,
        source = oblisk.notifications:map(feed),
        itemfn = function(notification)
            return panel_row {
                slot = "notification-" .. tostring(notification.id),
                -- `icon_path` is a cached asset path or empty (§ 2.7), and `icon { name = ... }`
                -- takes either a theme name or an absolute path (ADR-0054 decision 2), so the
                -- one property covers both without this having to tell them apart.
                -- `art`, not `icon`: this is the sending application's own artwork, which nobody
                -- here chose and nothing should recolour (`components/panel_row.lua` has the split).
                -- `icon_path` is a cached asset path or empty (§ 2.7), and `icon { name = ... }`
                -- takes either a theme name or an absolute path (ADR-0054 decision 2), so the
                -- one property covers both without this having to tell them apart.
                art = (notification.icon_path ~= nil and notification.icon_path ~= "") and notification.icon_path
                    or "dialog-information",
                title = notification.summary or "?",
                subtitle = notification.body or notification.app_name or "",
                -- Clicking dismisses, which is what the popup's whole surface already does.
                -- Activating a notification's default action is not offered because the protocol
                -- half is not built: § 2.7 carries no `actions` array and § 3.2 has only
                -- `dismiss(id)`.
                on_activate = function()
                    oblisk.notifications:invoke("dismiss", notification.id)
                end,
            }
        end,
        key = function(notification)
            return tostring(notification.id)
        end,
    },
    panel_empty_state("nothing waiting", util.shown_when(oblisk.notifications, function(n)
        return #feed(n) == 0
    end)),
}

return { kind = KIND, body = body }
