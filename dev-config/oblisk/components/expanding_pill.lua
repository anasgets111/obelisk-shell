-- `ExpandingPill.qml`: a row of circles showing one when collapsed and all of them under the
-- pointer, collapsing a moment after it leaves. Each cell tweens its width and opacity between
-- zero and a slot (ADR-0145, ADR-0146) and the row, content-sized, follows. The collapse delay is
-- `delay`, so a pointer that returns within `collapse_ms` cancels it; `hold_open` keeps the pill
-- open regardless, for a countdown in progress.
--
-- Deliberately not mirrored: when the collapsed slot changes, the mirror slides the strip so the
-- new circle arrives from the side. Here the old cell shrinks as the new one grows in place, which
-- reads as a hand-off rather than a scroll and needs no offset arithmetic.
--
-- The gap between circles is each cell's own right padding rather than the row's `spacing`: a
-- zero-width cell still earns `spacing`, and a collapsed strip would be one circle plus every gap.
-- The last cell's gap trails the pill by one `spacing.sm` while expanded, which the mirror's
-- `expandedWidth` does not have; nothing sits close enough to notice.
local theme = require("config.theme")
local util = require("lib.util")

local pill = {}

---@class ExpandingPillOpts
---@field slot string The hover slot the whole row declares.
---@field collapse_ms? integer How long after the pointer leaves the pill stays open. Default `theme.animation_ms`.
---@field hold_open? Signal<boolean> Keeps the pill open while true.

---@param opts ExpandingPillOpts
function pill.new(opts)
    local hovered = hover(opts.slot)
    local lingering = util.linger(hovered, opts.collapse_ms or theme.animation_ms)
    local expanded = opts.hold_open
            and computed({ lingering, opts.hold_open }, function(open, held)
                return open or held
            end)
        or lingering
    local self = { hovered = hovered, expanded = expanded }

    --- One cell: `circle` fills it, so a cell narrowing to zero narrows its circle with it, the
    --- way the mirror's delegate fills its cell. `shown` is whether this is the circle the
    --- collapsed pill keeps.
    ---@param circle table
    ---@param shown Signal<boolean>
    function self.cell(circle, shown)
        circle.width = "Fill"
        circle.height = "Fill"
        local width = computed({ expanded, shown }, function(open, kept)
            if open then
                return theme.item_width + theme.spacing.sm
            end
            return kept and theme.item_width or 0
        end)
        return row {
            width = width,
            height = theme.item_height,
            align_v = "Center",
            padding = expanded:map(function(open)
                return { right = open and theme.spacing.sm or 0 }
            end),
            opacity = computed({ expanded, shown }, function(open, kept)
                return (open or kept) and 1 or 0
            end),
            animate = { width = theme.animation_ms, padding = theme.animation_ms, opacity = theme.animation_ms },
            children = { circle },
        }
    end

    --- The pill: the hover region for every cell and the gaps between them.
    ---@param children table Cells, or a `list` of them.
    function self.row(children)
        return row {
            height = theme.item_height,
            align_v = "Center",
            hover = hovered,
            children = children,
        }
    end

    return self
end

return pill
