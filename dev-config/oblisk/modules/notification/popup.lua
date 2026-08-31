-- Mirrors Notification/NotificationPopup.qml.
--
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")

-- A second surface, because one cannot catch a whole class of bug: the applied-scene log named
-- every surface by its kind (the literal string "panel") until a second one made that visible.
--
-- Shown only while something is actually in the feed. Without the `visible` binding below this is a
-- card reading "no notifications" parked in the corner of the screen forever, which is not a
-- notification popup, it is a widget about the absence of notifications.
--
-- Nothing here runs a timer, and it does not need one: § 2.7's `feed` is the *active* set, and the
-- Supervisor already resolves `expire_timeout` and drops an entry when it lapses (docs/adr/0033,
-- where a critical notification never expires). So the card appears when one arrives and goes when
-- the daemon says it is over, and the config's whole part in that is reading whether the list is
-- empty.
--
-- ponytail: one card, showing `feed[1]`, where the reference shell stacks a card per notification.
-- A `list` over the feed is the right shape and lays out vertically already (§ 5.2 item 7), but this
-- surface has a literal height -- layer-shell only sizes an axis whose two edges are both anchored,
-- and this anchors one corner -- so a stack means binding `height` to the feed's length and taking
-- a compositor reconfigure on every arrival. Worth doing when a second notification actually needs
-- to be readable at the same time as the first; until then this drops the older one on screen
-- rather than in the Supervisor, which still has all twenty.
local function has_notification(n)
    return #(n.feed or {}) > 0
end

local function newest(n)
    return (n.feed or {})[1]
end

return panel {
    id = "notification_area",
    layer = "Overlay",
    anchor = { top = true, right = true },
    -- Anchored to one corner, so it reserves nothing and floats over whatever is behind it.
    margin = { top = 44, right = 12 },
    -- Both axes explicit, and they have to be: layer-shell only lets the compositor pick an
    -- axis whose two edges are both anchored, and this anchors one corner.
    width = 380,
    height = 96,
    visible = util.shown_when(oblisk.notifications, has_notification),
    -- Click to dismiss, which is § 3.2's `notifications:dismiss(id)` and the one interaction a
    -- notification popup has to have. The id is read inside the handler rather than captured when
    -- the node was built: the feed moves under this card, and a captured id would dismiss whichever
    -- notification happened to be newest at evaluation time.
    child = button {
        width = "Fill",
        height = "Fill",
        on_click = function(_, mouse_button)
            if mouse_button ~= "left" then
                return
            end
            local n = oblisk.notifications:get()
            local top = n and newest(n)
            if top and top.id then
                oblisk.notifications:invoke("dismiss", top.id)
            end
        end,
        children = {
            column {
                width = "Fill",
                height = "Fill",
                spacing = 6,
                padding = { top = 10, right = 12, bottom = 10, left = 12 },
                background = "#181825ee",
                radius = 10,
                border_width = 1,
                border_color = theme.SURFACE,
                children = {
                    cell(util.label(oblisk.notifications, function(n)
                        local top = newest(n)
                        -- Never seen while the surface is up, and still total: a `:map` runs during
                        -- resolution whether or not the node it feeds is visible, so this has to
                        -- answer on an empty feed rather than index into nothing.
                        return top and util.truncate(top.app_name or "?", 30) or ""
                    end), theme.DIM, 11),
                    -- `summary` rather than `body`, and it is a limit rather than a shortcut. `body`
                    -- is an array of spans (§ 2.7), not a string, so drawing it means drawing bold,
                    -- italic and href runs separately, and nothing in this engine draws styled runs
                    -- inside one `text`. Flattening the spans into one line here would throw away
                    -- the structure the Supervisor went to the trouble of parsing and gain nothing,
                    -- so this shows the summary until there is a way to draw the body properly.
                    cell(util.label(oblisk.notifications, function(n)
                        local top = newest(n)
                        return top and util.truncate(top.summary or "?", 34) or ""
                    end), theme.FG),
                },
            },
        },
    },
}
