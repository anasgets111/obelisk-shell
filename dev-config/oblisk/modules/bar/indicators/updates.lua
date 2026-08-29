-- Mirrors ArchChecker.qml.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local pill = require("components.pill")

return pill({ cell(util.label(oblisk.updates, function(u)
    if u.check_error and u.check_error ~= "" then
        return "updates?"
    end
    if u.installing then
        return string.format("installing %d/%d", u.install_current_step or 0, u.install_total_steps or 0)
    end
    if (u.count or 0) == 0 then
        return "up to date"
    end
    return string.format("%d updates", u.count)
end), theme.YELLOW) })
