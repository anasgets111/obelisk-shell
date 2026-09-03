-- Mirrors NotificationPopup.qml: a stack of the newest cards in one corner, not one surface each.
--
-- One surface holding a scrolling column, which is the shape ADR-0069 made possible and the one
-- this file said it was waiting for. A surface per card is the alternative and is not available:
-- a `panel` is declared, not spawned, so N of them means N declarations and a fixed ceiling anyway.
--
-- The card itself is `components/notification_card.lua`, shared with the history panel. What is
-- here is the surface: where it sits, how tall it is allowed to get, when it takes the keyboard,
-- and the expiry hold.
local theme = require("config.theme")
local util = require("lib.util")
local ui = require("lib.ui_state")
local notification_card = require("components.notification_card")

-- How many cards the stack shows at once, matching `maxVisibleNotifications`. The feed carries
-- twenty (§ 2.7) and a column of twenty cards is taller than the screen; the rest are one click
-- away in the history panel, which is the half of § 2.7 this popup was never meant to be.
local MAX_CARDS = 4

local SCROLL = scroll("notification_stack")

local function groups(n)
    local all = util.group_notifications(n and n.feed)
    local shown = {}
    for index = 1, math.min(#all, MAX_CARDS) do
        shown[index] = all[index]
    end
    return shown
end

return panel {
    id = "notification_area",
    layer = "Overlay",
    anchor = { top = true, right = true },
    margin = { top = theme.bar_height + theme.spacing.md, right = theme.spacing.md },
    width = theme.notification_width,
    -- Tall enough for the whole stack and no taller: the column sizes to its cards and the surface
    -- has to be able to hold them. A fixed `notification_height` was right when this showed one
    -- notification and is a clipping box now that it shows four that each grow when expanded.
    height = theme.notification_stack_height,
    -- Up when there is something to show and the history panel is not already showing it. Both
    -- surfaces anchor top-right, so without the second half the popup sits on top of the panel --
    -- the same notification drawn twice, one card overlapping the other, and the copy underneath
    -- is the one you opened the panel to read.
    visible = computed({ oblisk.notifications, ui.panel_showing("notifications") }, function(n, in_history)
        return not in_history and #((n and n.feed) or {}) > 0
    end),
    -- The keyboard, and only while a reply field is actually open. Bound rather than constant for
    -- the reason `modules/shell/panel_host.lua` states at length: niri gives an `on_demand` or
    -- `exclusive` layer surface focus the moment it *maps*, and this surface maps every time a
    -- notification arrives. A constant here would take the keyboard away from whatever you were
    -- typing in, every time anything notified you.
    --
    -- Which is also why the reply field is opened by a button rather than by clicking the field
    -- itself: the click that focuses a `textfield` deliberately fires no `on_click` (ADR-0092
    -- decision 7), so there is no way for the field's own press to be what raises this. The Reply
    -- button is the ask, and this follows it.
    keyboard_interactivity = ui.reply_id:map(function(id)
        return id ~= 0 and "Exclusive" or "None"
    end),
    child = column {
        width = "Fill",
        height = "Fill",
        -- The pointer resting anywhere on the stack stops every countdown, and leaving releases it
        -- (ADR-0094, ADR-0095). One region for the whole stack rather than one per card, and that
        -- is not just economy: sibling cards are written in tree order within a single pass, so a
        -- pointer moving from the second card to the first would fire the first's enter before the
        -- second's leave, and the leave would release the hold the enter had just placed.
        hover = hover("notification_stack_region"),
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
                    return notification_card(group, ui)
                end,
                key = function(group)
                    return group.key
                end,
            },
        },
    },
}
