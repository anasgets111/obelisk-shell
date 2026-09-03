-- Mirrors NotificationPopup.qml plus NotificationCard.qml, which are one surface here because a
-- popup that shows one notification does not need a host and a card as separate files.
--
-- The card is bounded and the text elides. It used to be a 34-character budget, a character
-- budget guessing at a pixel width, which cut "Xylophone" and "iiiiiiiii" at the same place and to
-- twice different widths. The card has a real width, so the engine cuts each string to the box
-- (`components/cell.lua`).
--
-- Still one notification at a time, the newest. The feed carries more (§ 2.7) and a stack of cards
-- wants either one surface per card or a scrolling column in one surface; the second is now
-- possible and the first is not, and neither is worth it until there is a history panel to open.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local icon_button = require("components.icon_button")

local function has_notification(n)
    return #(n.feed or {}) > 0
end

local function newest(n)
    return (n.feed or {})[1]
end

local function field(read)
    return util.label(oblisk.notifications, function(n)
        local top = newest(n)
        return top and read(top) or ""
    end)
end

local function dismiss_newest()
    local n = oblisk.notifications:get()
    local top = n and newest(n)
    if top and top.id then
        oblisk.notifications:invoke("dismiss", top.id)
    end
end

return panel {
    id = "notification_area",
    layer = "Overlay",
    anchor = { top = true, right = true },
    margin = { top = theme.bar_height + theme.spacing.md, right = theme.spacing.md },
    width = theme.notification_width,
    height = theme.notification_height,
    visible = util.shown_when(oblisk.notifications, has_notification),
    child = button {
        width = "Fill",
        height = "Fill",
        on_click = function(_, mouse_button)
            if mouse_button ~= "left" then
                return
            end
            dismiss_newest()
        end,
        children = {
            column {
                width = "Fill",
                height = "Fill",
                spacing = theme.spacing.xs,
                padding = {
                    top = theme.spacing.sm,
                    right = theme.spacing.md,
                    bottom = theme.spacing.sm,
                    left = theme.spacing.md,
                },
                background = theme.GLASS,
                radius = theme.radius.md,
                border_width = theme.border_width,
                border_color = theme.BORDER,
                children = {
                    -- App name on the left, close on the right, the header spread `Fill` gives for
                    -- one property. The close button is what makes the whole card not have to be
                    -- the dismiss target; it still is, because a notification you can dismiss by
                    -- hitting anywhere on it is the better default and this only adds an aim point.
                    row {
                        width = "Fill",
                        align_v = "Center",
                        spacing = theme.spacing.sm,
                        children = {
                            cell(field(function(top)
                                return top.app_name or "?"
                            end), theme.DIM, theme.font.xs, { width = "Fill" }),
                            icon_button(icons.close, dismiss_newest, {
                                size = theme.control.xs,
                                icon_size = theme.icon.xs,
                                background = theme.BORDER_SUBTLE,
                            }),
                        },
                    },
                    cell(field(function(top)
                        return top.summary or "?"
                    end), theme.FG, theme.font.md, { width = "Fill" }),
                    -- Two lines, then an ellipsis over whatever is left (ADR-0089). A freedesktop
                    -- body runs to 512 bytes and senders use them, so one elided line was showing
                    -- the opening clause of a sentence and dropping the rest without saying so.
                    cell(field(function(top)
                        return util.notification_body(top.body)
                    end), theme.TEXT_OFF, theme.font.sm, { width = "Fill", wrap = "Word", max_lines = 2 }),
                },
            },
        },
    },
}
