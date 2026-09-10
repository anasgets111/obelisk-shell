-- Shared by eleven bar modules: identical padding and radius make one visual group.
-- Glass, not solid: `surface2` at 0.42 alpha with a near-white 0.18 hairline, matching
-- `Components/IconButton.qml` and the mirror's pills; opaque `SURFACE` made a filled strip, not a
-- control over wallpaper.
-- `item_radius` is half `item_height`, giving semicircular ends that match adjacent icon buttons.
-- Fill missing child `align_v`: it defaults to `"Start"`, leaving a taller pill's child at the top.
-- The volume pill's bare `icon` sat a few pixels above its readout while its `button` and `meter`
-- were correct. The row's alignment places the pill in the bar, not its children. Explicit
-- `"Start"` or `"Stretch"` still wins.
local theme = require("config.theme")

return function(children, background, opts)
    opts = opts or {}
    for _, child in ipairs(children) do
        if child.align_v == nil then
            child.align_v = "Center"
        end
    end
    return row {
        height = opts.height or theme.item_height,
        align_v = "Center",
        spacing = theme.spacing.xs,
        padding = { left = theme.spacing.sm, right = theme.spacing.sm },
        background = background or theme.GLASS_CONTROL,
        radius = opts.radius or theme.item_radius,
        border_width = opts.border == false and nil or theme.border_width,
        border_color = opts.border == false and nil or theme.GLASS_BORDER,
        children = children,
    }
end
