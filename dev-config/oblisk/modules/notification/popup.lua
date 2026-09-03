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
local HOVER = hover("notification_stack_region")

-- The cards this surface should be showing: the newest few groups of everything live that has not
-- already had its turn as a popup, and nothing at all while any bar panel is up.
--
-- One signal for two properties. `visible` used to ask "is the feed non-empty" and the list asked
-- something else, which is how the surface came to map itself around an empty column; asking once
-- and reading the answer twice makes the two agree by construction.
local visible_groups = computed(
    { oblisk.notifications, ui.popup_seen, ui.panel_open, oblisk.lock, oblisk.applications },
    function(n, seen, panel_open, lock, applications)
        -- Any panel, not only the history: the mirror's `PanelHost` suspends popups on
        -- `onOverlayOpen` and pumps them again on close, so a card never sits beside an open
        -- panel (both anchor top-right, and the two surfaces would overlap). Standing down is not
        -- the same as being retired: only the history marks the feed seen (`ui.popup_seen`, on
        -- its open and its close), so what was up when the network panel opened comes back when
        -- it closes -- unless its countdown ran out meanwhile, since the Supervisor keeps counting
        -- (ADR-0100), in which case it is in the history and nowhere else, as the mirror's
        -- expire-transients step leaves it.
        --
        -- Nothing while the session is locked either (the mirror's `_popupsBlocked`): a popup
        -- over the lock screen is a message readable without the password. Not marked seen, so
        -- what arrived while locked pops up on unlock -- except what expired meanwhile, since the
        -- Supervisor's countdowns keep running and a five-second notification is `expired` long
        -- before the unlock. Which is the right split: a critical alert waits, a chat ping does not.
        if panel_open or (lock and lock.active) then
            return {}
        end
        -- Three ways a notification has had its turn: its own timeout ran out (`expired`, set by
        -- the Supervisor, ADR-0100); the history was opened while it was up (`ui.popup_seen`, this
        -- config's own note, ADR-0098); or do-not-disturb is on and it is not critical, which is
        -- the one urgency the mirror lets through DND. Each keeps it out of the stack; none takes it
        -- out of the history.
        local dnd = n and n.dnd
        local unseen = {}
        for _, notification in ipairs((n and n.feed) or {}) do
            local quiet = dnd and notification.urgency ~= "critical"
            if not notification.expired and not quiet and not (seen or {})[util.notification_key(notification)] then
                unseen[#unseen + 1] = notification
            end
        end
        local all = util.group_notifications(unseen, applications)
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
    -- The keyboard, on demand, while the pointer is on the stack or a reply is half-typed
    -- (ADR-0109). Bound rather than constant for the reason `modules/shell/panel_host.lua` states
    -- at length: niri gives an `on_demand` or `exclusive` layer surface focus the moment it *maps*,
    -- and this surface maps every time a notification arrives. A constant here would take the
    -- keyboard away from whatever you were typing in, every time anything notified you.
    --
    -- `OnDemand`, not `Exclusive` (ADR-0108). Exclusive is the lock screen's word: the keyboard
    -- stays here whatever is clicked, and a surface this small has nowhere for a click-outside to
    -- land. On demand, niri gives this surface the keyboard on a *click* while the mode is already
    -- on demand -- not on the flip to it, measured -- which is why the hover is in the binding:
    -- the pointer arrives before the click into the reply field, so the field's own click is the
    -- one that brings the keyboard, and typing starts at once. The pending draft keeps the ask
    -- alive after the pointer leaves, for a click-to-focus compositor that would otherwise drop the
    -- keyboard mid-sentence; under focus-follows-mouse the keyboard has left with the pointer
    -- anyway and comes back with it, and the field keeps its text through both.
    keyboard_interactivity = computed({ HOVER, ui.reply_pending }, function(hovered, pending)
        return (hovered or pending) and "OnDemand" or "None"
    end),
    child = column {
        width = "Fill",
        height = "Fill",
        -- The pointer resting on a card stops every countdown, and leaving releases it (ADR-0094,
        -- ADR-0095). On a card, not anywhere in this box: the input region is built from what is
        -- drawn (ADR-0109), so the empty surface below the cards sends no pointer events at all. One region for the whole stack rather than one per card, and that
        -- is not just economy: sibling cards are written in tree order within a single pass, so a
        -- pointer moving from the second card to the first would fire the first's enter before the
        -- second's leave, and the leave would release the hold the enter had just placed.
        hover = HOVER,
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
