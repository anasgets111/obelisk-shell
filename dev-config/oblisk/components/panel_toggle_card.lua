-- A label next to a `components/toggle.lua`, the settings-row shape `modules/bar/panels/settings.lua`
-- wants for `bluetooth.enabled` and every boolean capability after it. Content-sized rather than
-- `width = "Fill"` with the toggle pinned to the far edge, for the same reason
-- `components/panel_header.lua` gives up on that: this `row`'s main axis has no space-between.
local theme = require("config.theme")
local cell = require("components.cell")
local toggle = require("components.toggle")

return function(label, signal, read, on_change)
    return row {
        height = 24,
        align_v = "Center",
        spacing = 10,
        children = { cell(label, theme.FG), toggle(signal, read, on_change) },
    }
end
