-- A module's visual grouping, the thing every bar calls a pill. Worth having as a component rather
-- than repeated inline: it is eleven modules, and the padding and radius being identical across
-- them is the entire visual effect.
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

return function(children, background)
    for _, child in ipairs(children) do
        if child.align_v == nil then
            child.align_v = "Center"
        end
    end
    return row {
        height = "Fill",
        align_v = "Center",
        spacing = 6,
        padding = { left = 10, right = 10 },
        background = background or theme.SURFACE,
        radius = 6,
        children = children,
    }
end
