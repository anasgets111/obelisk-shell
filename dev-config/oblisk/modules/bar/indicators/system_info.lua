-- Mirrors SystemInfoWidget.qml's collapsed form: a glyph per readout with its number beside it,
-- rather than one run of "cpu 12% ram 34%".
--
-- Not on the bar. `modules/bar/panels/settings.lua` is what shows it, and the mirror's own widget
-- is behind an expander for the same reason: two percentages take more room than a bar has once
-- everything else on it is a circle.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local pill = require("components.pill")

local function readout(glyph, read)
    return row {
        align_v = "Center",
        spacing = theme.spacing.xs,
        children = {
            cell(glyph, theme.DIM, theme.icon.sm, { align_v = "Center" }),
            cell(util.label(oblisk.sysinfo, read), theme.FG, theme.font.xs, { align_v = "Center" }),
        },
    }
end

return pill({
    readout(icons.cpu, function(s)
        return string.format("%d%%", s.cpu_percent or 0)
    end),
    readout(icons.ram, function(s)
        return string.format("%d%%", s.ram_percent or 0)
    end),
})
