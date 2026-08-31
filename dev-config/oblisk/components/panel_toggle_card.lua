-- A label next to a `components/toggle.lua`, the settings-row shape `modules/bar/panels/settings.lua`
-- wants for `bluetooth.enabled` and every boolean capability after it.
--
-- Spread rather than content-sized: the label takes `width = "Fill"` and the switch sits against the
-- far edge, which is what `Components/PanelToggleCard.qml` does and what a settings list has to do
-- to read as a column of switches rather than a ragged row of pairs. That needed the `Fill` fix in
-- `scene.rs`; `components/panel_header.lua` carries the same note.
local theme = require("config.theme")
local cell = require("components.cell")
local toggle = require("components.toggle")

return function(label, signal, read, on_change)
    return row {
        width = "Fill",
        height = theme.control.sm,
        align_v = "Center",
        spacing = theme.spacing.md,
        children = { cell(label, theme.FG, nil, { width = "Fill" }), toggle(signal, read, on_change) },
    }
end
