-- Small dim section label. `modules/bar/panels/settings.lua` is easier to scan as "system" and
-- "bluetooth" than as one column, and other panels use the same split.
-- `PanelSectionHeader.qml` sets the word itself: uppercased, bold, `opacityMuted`, indented by
-- `spacingSm`. Its `spacingXs` is headroom, not a gap underneath -- the mirror sizes an `Item` to
-- `label.implicitHeight + spacingXs` and anchors the label to its *bottom*, so the air is on top
-- and the label sits tight against the list it introduces.
local theme = require("config.theme")

return function(content)
    return text {
        content = { { text = content:upper(), bold = true } },
        foreground = theme.DIM,
        font_size = theme.font.xs,
        opacity = theme.opacity.muted,
        padding = { top = theme.spacing.xs, left = theme.spacing.sm },
    }
end
