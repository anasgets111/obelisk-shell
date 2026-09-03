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

-- The cards this surface should be showing: the newest few groups of everything live that has not
-- already had its turn as a popup, and nothing at all while the history panel is up.
--
-- One signal for two properties. `visible` used to ask "is the feed non-empty" and the list asked
-- something else, which is how the surface came to map itself around an empty column; asking once
-- and reading the answer twice makes the two agree by construction.
local visible_groups = computed(
    { oblisk.notifications, ui.popup_seen, ui.panel_showing("notifications") },
    function(n, seen, in_history)
        -- The panel draws the same cards from the same feed, and both anchor top-right. Standing
        -- down is not the same as being retired, though: `ui.popup_seen` is what stops these
        -- coming back when the panel closes.
        if in_history then
            return {}
        end
        -- Two ways a notification has had its turn: its own timeout ran out (`expired`, set by
        -- the Supervisor, ADR-0100), or the history was opened while it was up (`ui.popup_seen`,
        -- this config's own note, ADR-0098). Either keeps it out of the stack; neither takes it
        -- out of the history.
        local unseen = {}
        for _, notification in ipairs((n and n.feed) or {}) do
            if not notification.expired and not (seen or {})[util.notification_key(notification)] then
                unseen[#unseen + 1] = notification
            end
        end
        local all = util.group_notifications(unseen)
        local shown = {}
        for index = 1, math.min(#all, MAX_CARDS) do
            shown[index] = all[index]
        end
        return shown
    end
)

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
    visible = visible_groups:map(function(shown)
        return #shown > 0
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
                source = visible_groups,
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
