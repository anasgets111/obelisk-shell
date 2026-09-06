-- A module's visual grouping, shared by eleven bar modules. Identical padding and radius are the
-- visual effect.
-- Glass, not solid: `surface2` at 0.42 alpha with a near-white 0.18 hairline, matching
-- `Components/IconButton.qml` and the mirror's pills. Opaque `SURFACE` made a filled rectangle on
-- the strip instead of a control floating over wallpaper.
-- `item_radius` is half `item_height`, giving semicircular ends that match adjacent icon buttons
-- without duplicating their number.
-- Fill missing child `align_v` because it defaults to `"Start"` and the pill is taller. Otherwise a
-- child sits at the top while neighbours centre; the volume pill's bare `icon` was a few pixels
-- above its readout while its `button` and `meter` were correct. The row's own alignment only
-- places
-- the pill in the bar, not its children. Explicit `"Start"` or `"Stretch"` still wins.
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
