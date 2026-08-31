-- A label next to a `components/toggle.lua`, the settings-row shape `modules/bar/panels/settings.lua`
-- wants for `bluetooth.enabled` and every boolean capability after it. Content-sized rather than
-- `width = "Fill"` with the toggle pinned to the far edge, which was a `row` bug this engine no
-- longer has -- see `components/panel_header.lua` for the same note and the same one-property fix.
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
