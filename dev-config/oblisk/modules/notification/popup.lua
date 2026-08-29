local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")

-- A second surface, because one cannot catch a whole class of bug: the applied-scene log named
-- every surface by its kind (the literal string "panel") until a second one made that visible.
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
    child = column {
        spacing = 6,
        padding = { top = 10, right = 12, bottom = 10, left = 12 },
        background = "#181825ee",
        radius = 10,
        border_width = 1,
        border_color = theme.SURFACE,
        children = {
            cell(util.label(oblisk.notifications, function(n)
                local newest = (n.feed or {})[1]
                if not newest then
                    return "no notifications"
                end
                return util.truncate(newest.app_name or "?", 30)
            end), theme.DIM, 11),
            -- `summary` rather than `body`, and it is a limit rather than a shortcut. `body` is an
            -- array of spans (§ 2.7), not a string, so drawing it means drawing bold, italic and
            -- href runs separately, and nothing in this engine draws styled runs inside one `text`.
            -- Flattening the spans into one line here would throw away the structure the Supervisor
            -- went to the trouble of parsing and gain nothing, so this shows the summary until
            -- there is a way to draw the body properly.
            cell(util.label(oblisk.notifications, function(n)
                local newest = (n.feed or {})[1]
                if not newest then
                    return "nothing to show"
                end
                return util.truncate(newest.summary or "?", 34)
            end), theme.FG),
        },
    },
}
