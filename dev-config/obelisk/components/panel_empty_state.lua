-- Empty-list state matching `Components/PanelEmptyState.qml`. A dim `cell` left an empty panel with
-- only its header, which looked like a load failure rather than an intentional empty state.
--
-- The caller supplies `visible`; `lib/util.lua`'s `shown_when` maps the capability payload to it.
--
-- With `opts.icon`, use the mirror's large dim glyph over the message and
-- `Layout.minimumHeight: 120`, which reads as state rather than a gap. Without it, use the single
-- line from the launcher and power menu.
local theme = require("config.theme")
local cell = require("components.cell")
local glyph = require("components.glyph")
local util = require("lib.util")

---@param message string|Bound
---@param visible boolean|Bound
---@param opts? { icon?: string|Bound|table, subtext?: string|Bound }
return function(message, visible, opts)
    opts = opts or {}
    local lines = {}
    local mark = opts.icon
    if type(mark) == "table" then
        lines[#lines + 1] = mark
    elseif mark then
        lines[#lines + 1] = glyph(mark, theme.DIM, theme.icon.xl, { align = "Center" })
    end
    lines[#lines + 1] = cell(message, theme.DIM, theme.font.sm, { align = "Center" })
    if opts.subtext then
        -- The mirror's third line uses `textInactiveColor` at `opacityMuted`, wrapped and centred.
        -- It explains whether nothing arrived or something is suppressing the list, rather than
        -- repeating the message. Use a colour at that alpha because `cell` takes no node `opacity`.
        local subtext = opts.subtext
        ---@cast subtext -nil
        ---@type boolean|Signal
        local shown = true
        if type(subtext) == "userdata" then
            ---@cast subtext Signal
            shown = util.shown_when(subtext, function(value)
                return value ~= ""
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
