-- The notification feed as a list, which is the half of § 2.7 the popup is not. The popup shows the
-- newest few for as long as the Supervisor keeps them; this is where all of them are.
--
-- Needed a scrolling container to exist at all: a feed is however many notifications have arrived,
-- so a fixed panel could show the first four and clip the rest with no way to reach them
-- (ADR-0069).
--
-- The rows used to be `panel_row`s -- an icon, a title, a subtitle -- and are now the same
-- `components/notification_card.lua` the popup draws, so an action button, a reply and an expanded
-- body work here too. What is left in this file is the header and the list.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local ui = require("lib.ui_state")
local cell = require("components.cell")
local section_header = require("components.section_header")
local panel_empty_state = require("components.panel_empty_state")
local icon_button = require("components.icon_button")
local notification_card = require("components.notification_card")

local KIND = "notifications"
local SCROLL = scroll("notification_feed")

local function feed(n)
    return (n and n.feed) or {}
end

local function groups(n)
    return util.group_notifications(feed(n))
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
    column {
        width = "Fill",
        height = "Fill",
        -- Same hold as the popup's stack and for the same reason (ADR-0094): a notification that
        -- expired while you were reading the history of it would be the one place a list can
        -- rearrange itself under a pointer with no input at all. A separate region from the
        -- popup's, and the two never overlap -- they are different surfaces, so a pointer leaves
        -- one before it enters the other.
        hover = hover("notification_history_region"),
        on_hover = function(hovered)
            oblisk.notifications:invoke("hold_expiry", hovered and 300 or 0)
        end,
        children = {
            list {
                width = "Fill",
                height = "Fill",
                scroll = SCROLL,
                spacing = theme.spacing.sm,
                source = oblisk.notifications:map(groups),
                itemfn = function(group)
                    -- The lighter ground: this card sits inside a panel that is already glass, and
                    -- the popup's heavier one over a wallpaper would read as a second sheet here.
                    return notification_card(group, ui, { background = theme.GLASS_CONTENT })
                end,
                key = function(group)
                    return group.key
                end,
            },
        },
    },
    panel_empty_state("nothing waiting", util.shown_when(oblisk.notifications, function(n)
        return #feed(n) == 0
    end)),
}

return { kind = KIND, body = body }
