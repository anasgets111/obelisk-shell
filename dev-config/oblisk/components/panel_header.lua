-- A panel's title and its own close affordance, so a window does not depend on the button that
-- opened it to close it again. `modules/bar/panels/settings.lua` had no way to close itself before
-- this; the only path back was re-clicking the bar button that opened it.
--
-- Title and close button sit as a content-sized block, not spread across the card's full width,
-- and that is now a leftover rather than a constraint. `width = "Fill"` on a row's main axis used
-- to resolve against the parent's whole width regardless of siblings, so a `Fill` title took the
-- card and pushed the close button out of it. That is fixed in `scene.rs`: a `Fill` child is sized
-- from what its siblings leave. Giving the title `width = "Fill"` pins the button to the far edge
-- in one property. Not done here, because it changes what is on screen and wants looking at.
local theme = require("config.theme")

return function(title, on_close)
    return row {
        spacing = 8,
        align_v = "Center",
        children = {
            text { content = title, foreground = theme.FG, font_size = 16 },
            button {
                width = 18,
                height = 18,
                align_v = "Center",
                on_click = function(_, mouse_button)
                    if mouse_button == "left" then
                        on_close()
                    end
                end,
                children = { icon { name = "window-close", size = 12 } },
            },
        },
    }
end
