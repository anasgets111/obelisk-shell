-- Empty-list state matching `Components/PanelEmptyState.qml`. A dim `cell` left an empty panel with
-- only its header, which looked like a load failure rather than an intentional empty state.
-- The caller supplies `visible`, because it knows which list is empty;
-- `lib/util.lua`'s `shown_when`
-- already maps the capability payload to that boolean.
-- With `opts.icon`, use the mirror's large dim glyph over the message and
-- `Layout.minimumHeight: 120`, which reads as state rather than a gap. Without it, keep the single
-- line used by the launcher and power menu.
local theme = require("config.theme")
local cell = require("components.cell")
local glyph = require("components.glyph")

---@param message string|Bound
---@param visible boolean|Bound
---@param opts? { icon?: string|Bound }
return function(message, visible, opts)
    opts = opts or {}
    local lines = {}
    if opts.icon then
        lines[#lines + 1] = glyph(opts.icon, theme.TEXT_OFF, theme.icon.xl, { align = "Center" })
    end
    lines[#lines + 1] = cell(message, theme.TEXT_OFF, theme.font.sm, { align = "Center" })
    return column {
        width = "Fill",
        height = opts.icon and theme.panel_empty_height or theme.control.lg,
        align_v = "Center",
        spacing = theme.spacing.sm,
        visible = visible,
        children = lines,
    }
end
