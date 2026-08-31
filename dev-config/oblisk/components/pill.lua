-- A module's visual grouping, the thing every bar calls a pill. Worth having as a component rather
-- than repeated inline: it is eleven modules, and the padding and radius being identical across
-- them is the entire visual effect.
--
-- Glass, not solid. The ground is surface2 at 0.42 alpha with a hairline of near-white at 0.18
-- around it, which is what `Components/IconButton.qml` and the mirror's own pills paint. An opaque
-- ground here is the difference between a control that floats over the wallpaper and a filled
-- rectangle sitting on a strip, and it was opaque `SURFACE` until now.
--
-- `item_radius` is half `item_height`, so a pill's ends are semicircles and it agrees with the
-- circular icon buttons beside it without either naming the other's number.
--
-- Cross-axis alignment is filled in here because `align_v` defaults to `"Start"` and a pill is
-- taller than everything in it. A child that forgets the property sits on the top edge while its
-- neighbours that remembered sit centred, which is a per-child bug that reads as an engine one: the
-- volume pill drew its `icon` a few pixels above the readout for exactly this reason, since the
-- `button` and the `meter` beside it both set the property and the bare `icon` did not. Setting the
-- row's own `align_v` does not help, because that places the pill inside the bar rather than placing
-- the pill's children inside the pill. A child wanting `"Start"` or `"Stretch"` still says so and
-- keeps it.
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
