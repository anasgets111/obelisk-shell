-- What a panel shows when the thing it lists is empty, which is `Components/PanelEmptyState.qml`'s
-- whole job. Worth a component rather than a dim `cell` inline because the alternative is what this
-- config did until now: an empty panel drew its header and then nothing, which reads as a panel
-- that failed to load rather than one with nothing to say.
--
-- Takes the `visible` signal rather than computing it, because only the caller knows which list is
-- empty and `lib/util.lua`'s `shown_when` already turns a capability payload into that boolean.
--
-- With `opts.icon` it is the mirror's shape: a large dim glyph over the line, at a height that
-- reads as a state (`Layout.minimumHeight: 120`) rather than as a gap where rows should be. Without
-- one it stays the single line the launcher and the power menu want.
local theme = require("config.theme")
local cell = require("components.cell")

---@param message string|Bound
---@param visible boolean|Bound
---@param opts? { icon?: string|Bound }
return function(message, visible, opts)
    opts = opts or {}
    local lines = {}
    if opts.icon then
        lines[#lines + 1] = cell(opts.icon, theme.TEXT_OFF, theme.icon.xl, { align = "Center" })
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
