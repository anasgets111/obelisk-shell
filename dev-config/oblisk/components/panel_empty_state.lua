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
---@param opts? { icon?: string|Bound, subtext?: string|Bound }
return function(message, visible, opts)
    opts = opts or {}
    local lines = {}
    if opts.icon then
        lines[#lines + 1] = glyph(opts.icon, theme.DIM, theme.icon.xl, { align = "Center" })
    end
    lines[#lines + 1] = cell(message, theme.DIM, theme.font.sm, { align = "Center" })
    if opts.subtext then
        -- The mirror's third line: `textInactiveColor` at `opacityMuted`, wrapped and centred. It
        -- carries the reason rather than repeating the message, so an empty list can say whether
        -- it is empty because nothing arrived or because something is suppressing it. Written as a
        -- colour at that alpha rather than a node `opacity`, which `cell` does not take.
        local subtext = opts.subtext
        ---@cast subtext -nil
        ---@type boolean|Signal
        local shown = true
        if type(subtext) == "userdata" then
            ---@cast subtext Signal
            shown = subtext:map(function(value)
                return value ~= nil and value ~= ""
            end)
        end
        lines[#lines + 1] = cell(subtext, theme.with_opacity(theme.DIM, theme.opacity.muted), theme.font.sm, {
            align = "Center",
            width = "Fill",
            wrap = "Word",
            visible = shown,
        })
    end
    return column {
        width = "Fill",
        height = opts.icon and theme.panel_empty_height or theme.control.lg,
        align_v = "Center",
        spacing = theme.spacing.sm,
        visible = visible,
        children = lines,
    }
end
