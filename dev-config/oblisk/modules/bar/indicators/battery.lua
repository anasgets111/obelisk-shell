-- Mirrors BatteryIndicator.qml.
--
-- Battery, real as of docs/adr/0053. Colour carries the state, which is what a bar is for, and it
-- is a signal rather than a constant because a property resolves from a signal like any other
-- (ADR-0044). Charging is green whatever the level, because a charging battery at 8% is not the
-- emergency an 8% discharging one is.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local pill = require("components.pill")
local meter = require("components.meter")

local function battery_color(b)
    if b == nil or not b.present then
        return theme.DIM
    end
    if b.charging then
        return theme.GREEN
    end
    if b.percent < 15 then
        return theme.RED
    end
    if b.percent < 30 then
        return theme.PEACH
    end
    return theme.GREEN
end

local battery_module = pill({
    cell(util.label(oblisk.battery, function(b)
        if not b.present then
            return "ac"
        end
        return string.format("%d%%%s", b.percent, b.charging and " +" or "")
    end), oblisk.battery:map(battery_color)),
    meter(oblisk.battery, function(b)
        return b.present and b.percent or 0
    end, oblisk.battery:map(battery_color), 32),
    -- `power` sits beside the battery it describes rather than in a pill of its own: § 2.13's two
    -- UPower fields read the same hardware § 2.2 reports the charge of, and the right zone has no
    -- room for a twelfth pill.
    --
    -- Each of § 2.13's four fields can be absent on its own, so each is read behind its own check
    -- rather than through one `nil` guard. This machine has no power-profiles-daemon, so
    -- `active_profile` is `nil` forever and this draws the rate and the source alone. That is the
    -- absence being reported correctly, not the module failing, and it is the difference between
    -- `nil` and a fabricated `"balanced"` that made every field optional.
    cell(util.label(oblisk.power, function(p)
        local parts = {}
        if p.on_battery ~= nil then
            parts[#parts + 1] = p.on_battery and "bat" or "ac"
        end
        if p.energy_rate ~= nil then
            parts[#parts + 1] = string.format("%.1fW", p.energy_rate)
        end
        if p.active_profile ~= nil then
            parts[#parts + 1] = p.active_profile
        end
        return #parts > 0 and table.concat(parts, " ") or "--"
    end), theme.DIM, 11),
})

return battery_module
