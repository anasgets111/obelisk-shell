-- Mirrors SystemInfoWidget.qml's collapsed form: a glyph and number per readout, not "cpu 12% ram
-- 34%".
--
-- Not on the bar. `modules/bar/panels/settings.lua` shows it; two percentages need more room once
-- the rest of the bar is circles.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local pill = require("components.pill")

-- `sysinfo`'s three pollers start dormant until configured (ADR-0035); without this, the readouts
-- stay at pre-first-sample `0%`. Configure here, not in `shell.lua`, because this is the only
-- module reading them.
--
-- CPU every 2s and RAM every 5s, about as slow as a readout can tick before it reads as frozen,
-- matching the mirror's cadence. Leave `temp_interval` at zero:
-- nothing reads `temp_cores` or `temp_gpu`, and a dormant poller costs nothing.
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
