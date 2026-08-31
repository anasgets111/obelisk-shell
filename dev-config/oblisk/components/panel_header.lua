-- A panel's title and its own close affordance, so a window does not depend on the button that
-- opened it to close it again. `modules/bar/panels/settings.lua` had no way to close itself before
-- this; the only path back was re-clicking the bar button that opened it.
--
-- Title and close button sit as a content-sized block, not spread across the card's full width.
-- `row`'s main axis has no space-between: `position_children`'s `row` arm (`scene.rs`) resolves
-- the whole block's position from the row's own `align_h`, never from a per-child "take the
-- remainder". `modules/bar/center_side.lua` hits the identical gap and works around it with three
-- fixed-width zones instead of one `Fill` title, which only works because a bar's total width is
-- known up front; a card's is not, so that workaround does not carry over here. Pinning the close
-- button to the far edge needs a `row` feature this engine does not have yet.
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
