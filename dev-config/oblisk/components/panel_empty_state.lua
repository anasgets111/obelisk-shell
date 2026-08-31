-- What a panel shows when the thing it lists is empty, which is `Components/PanelEmptyState.qml`'s
-- whole job. Worth a component rather than a dim `cell` inline because the alternative is what this
-- config did until now: an empty panel drew its header and then nothing, which reads as a panel
-- that failed to load rather than one with nothing to say.
--
-- Takes the `visible` signal rather than computing it, because only the caller knows which list is
-- empty and `lib/util.lua`'s `shown_when` already turns a capability payload into that boolean.
local theme = require("config.theme")
local cell = require("components.cell")

return function(message, visible)
    return row {
        width = "Fill",
        height = theme.control.lg,
        align_h = "Center",
        align_v = "Center",
        visible = visible,
        children = { cell(message, theme.TEXT_OFF, theme.font.sm, { align = "Center" }) },
    }
end
