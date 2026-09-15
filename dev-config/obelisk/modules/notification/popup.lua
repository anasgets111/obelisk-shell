-- Mirrors NotificationPopup.qml: newest cards stacked in one corner on one surface.
--
-- One scrolling surface, enabled by ADR-0069. A declared `panel` avoids one surface per card and
-- keeps a fixed ceiling.
--
-- `components/notification_card.lua` is shared with history. This file owns placement, height,
-- keyboard mode, and expiry hold.
local theme = require("config.theme")
local util = require("lib.util")
local ui = require("lib.ui_state")
local notification_card = require("components.notification_card")

-- Sounds as `NotificationService.qml` played them; the Supervisor is silent until a tier is set.
local SOUNDS = "/usr/share/sounds/freedesktop/stereo/"
obelisk.notifications:invoke("set_sound", "low", SOUNDS .. "message.oga")
obelisk.notifications:invoke("set_sound", "normal", SOUNDS .. "message.oga")
obelisk.notifications:invoke("set_sound", "critical", SOUNDS .. "bell.oga")

-- Visible stack count matches `maxVisibleNotifications`. The feed carries twenty, too many
-- for a screen; the rest belong one click away in history.
local MAX_CARDS = 4

local SCROLL = scroll("notification_stack")
local HOVER = hover("notification_stack_region")

-- Show the newest unseen cards, except while a bar panel is open.
--
-- One signal drives both properties. Sharing the answer prevents `visible` and the list predicate
-- from mapping different results.
local visible_groups = computed(
    { obelisk.notifications, ui.popup_seen, ui.panel_open, obelisk.lock, obelisk.applications },
    function(n, seen, panel_open, lock, applications)
        -- Any panel suspends popups, as `PanelHost` does on `onOverlayOpen`; both anchor top-right.
        -- Standing down is not retiring: only history marks entries seen (`ui.popup_seen`), so a
        -- card returns after panel close unless its Supervisor countdown expires (ADR-0100).
        --
        -- Nothing while locked (`_popupsBlocked`): a popup over the lock screen is readable without
        -- a password. Do not mark it seen, so it returns on unlock unless its Supervisor countdown
        -- expires; a five-second chat ping may expire while a critical alert waits.
        if panel_open or (lock and lock.active) then
            return {}
        end
        -- Expiry (`expired`, Supervisor, ADR-0100), history's seen set (`ui.popup_seen`, ADR-0098),
        -- and DND remove a card from the stack; expiry, unlike the others, leaves it in history.
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
            local group = all[index]
            -- `components/notification_card.lua` reads this rank to stagger entry. `all` is fresh
            -- on every pass, so this stamps a local table, not the capability's data.
            group.rank = index
            shown[index] = group
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
    -- No `height`: the surface is the stack. The old cap put one card in 560px of surface and left
    -- four expanded cards nowhere to grow; `max_height` now bounds the list instead.
    visible = visible_groups:map(function(shown)
        return #shown > 0
    end),
    -- On demand while the pointer is on the stack or a reply is pending (ADR-0109). Bind it because
    -- niri focuses an `on_demand`/`exclusive` layer surface on map; a constant would steal the
    -- keyboard on every notification.
    --
    -- `OnDemand`, not `Exclusive` (ADR-0108): a small surface has no outside click and must not
    -- keep the keyboard. Measured niri behavior focuses it on a *click* while already on demand,
    -- not on the mode flip, so hover enters the binding before a reply-field click. A pending draft
    -- keeps the request alive after the pointer leaves under click-to-focus; focus-follows-mouse
    -- returns the keyboard and preserves the field text.
    keyboard_interactivity = computed({ HOVER, ui.reply_pending }, function(hovered, pending)
        return (hovered or pending) and "OnDemand" or "None"
    end),
    child = column {
        width = "Fill",
        -- No `height` here or on the list: content fills the surface without a parent-child sizing
        -- loop.
        -- A pointer on any card stops countdowns; leaving releases the hold (ADR-0094, ADR-0095).
        -- The region follows drawn input (ADR-0109), so empty space below sends no events. One
        -- region avoids sibling enter/leave ordering that could release a newly acquired hold.
        hover = HOVER,
        on_hover = function(hovered)
            obelisk.notifications:invoke("hold_expiry", hovered and 300 or 0)
        end,
        children = {
            list {
                width = "Fill",
                -- The one bounded box in that chain: below the cap the stack is exactly its
                -- cards, at it the rest becomes the remainder `SCROLL` scrolls.
                max_height = theme.notification_stack_height,
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
