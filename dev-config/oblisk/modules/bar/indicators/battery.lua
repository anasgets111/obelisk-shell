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
local tooltip = require("components.tooltip")

-- One name for the hover slot, read by the pill that triggers it and the tooltip that shows for it.
-- The name is the identity (docs/adr/0062 decision 2), the way `state(name, initial)` already works.
local SLOT = "battery"

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
})

-- The pill declares the hover slot its tooltip reads (docs/adr/0062). On the `row` `pill` returns
-- rather than on anything inside it, because a hover is true for a node and every ancestor of it:
-- naming the outermost box means the whole pill is one hover region, including the gaps between
-- its children.
battery_module.hover = hover(SLOT)

-- § 2.13's UPower fields, which used to be a fourth cell inside the pill and made it the widest
-- module on the bar. A tooltip is where Quickshell's BatteryIndicator puts them too, and it is the
-- better place for the same reason: this is detail you go looking for, not a readout you watch.
--
-- Each of § 2.13's four fields can be absent on its own, so each is read behind its own check
-- rather than through one `nil` guard. This machine has no power-profiles-daemon, so
-- `active_profile` is `nil` forever and this draws the rate and the source alone. That is the
-- absence being reported correctly, not the module failing, and it is the difference between `nil`
-- and a fabricated `"balanced"` that made every field optional.
local battery_tooltip = tooltip({
    id = "battery_tooltip",
    slot = SLOT,
    width = 180,
    height = 64,
    children = {
        cell(util.label(oblisk.battery, function(b)
            if not b.present then
                return "no battery"
            end
            return string.format("%d%% %s", b.percent, b.charging and "charging" or "discharging")
        end), theme.FG, 12),
        cell(util.label(oblisk.power, function(p)
            local parts = {}
            if p.on_battery ~= nil then
                parts[#parts + 1] = p.on_battery and "on battery" or "on ac"
            end
            if p.energy_rate ~= nil then
                parts[#parts + 1] = string.format("%.1f W", p.energy_rate)
            end
            if p.active_profile ~= nil then
                parts[#parts + 1] = p.active_profile
            end
            return #parts > 0 and table.concat(parts, ", ") or "no power detail"
        end), theme.DIM, 11),
    },
})

return { indicator = battery_module, tooltip = battery_tooltip }
