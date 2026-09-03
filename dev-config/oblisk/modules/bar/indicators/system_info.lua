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

-- `sysinfo`'s three pollers start dormant and stay there until a config names an interval
-- (ADR-0035), so without this line the two readouts below sit at their pre-first-sample `0%`
-- forever -- the capability was wired, started, and never asked for a number. Here rather than in
-- `shell.lua` because this is the only file that reads it: the module that wants the samples is the
-- one that says how often.
--
-- Two seconds for CPU and five for RAM, which is the mirror's cadence and about as slow as a
-- readout can tick before it reads as frozen. `temp_interval` is left at zero deliberately:
-- `temp_cores` and `temp_gpu` have no reader in this config, and a dormant poller costs nothing.
oblisk.sysinfo:invoke("configure", { cpu_interval = 2, ram_interval = 5 })

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
