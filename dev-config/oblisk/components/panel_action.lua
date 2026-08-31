-- One clickable line in a panel body: an icon, a label, and a left-click. The three panels under
-- `modules/bar/panels/` were writing the same `button`-wrapping-a-`row` block, which is
-- `components/pill.lua`'s reason for being all over again.
--
-- Left button only. A panel row that fired on right-click too would put `power_menu.lua`'s lock
-- one stray click away, which is the case docs/adr/0050's second amendment widened `on_click` to
-- let a config refuse.
local theme = require("config.theme")
local cell = require("components.cell")

return function(icon_name, label, on_activate, color)
    return button {
        height = 24,
        align_v = "Center",
        on_click = function(_, mouse_button)
            if mouse_button ~= "left" then
                return
            end
            on_activate()
        end,
        children = {
            row {
                spacing = 8,
                align_v = "Center",
                children = {
                    icon { name = icon_name, size = 14 },
                    cell(label, color or theme.FG, 12),
                },
            },
        },
    }
end
