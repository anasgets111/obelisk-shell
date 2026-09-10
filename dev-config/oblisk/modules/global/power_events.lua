-- Battery side effects, not drawing: charger OSD, `BatteryService.qml`'s two `notify-send`s, and
-- `PowerManagementService.qml`'s automatic suspend and brightness step. No surface; `shell.lua`
-- requires two `on_change` handlers for their side effects (ADR-0115).
--
-- Each handler acts on a crossing, like the mirror's `onIsLowAndNotChargingChanged` signals. The
-- first push (`previous == nil`) is not an edge; the mirror uses `initialized` for the same reason.
local icons = require("config.icons")
local util = require("lib.util")
local osd = require("modules.osd.service")

local thresholds = util.battery_thresholds

-- `notify-send` uses the mirror's `_sendNotification` arguments through our daemon, so it becomes a
-- normal card. No 15-second dedupe; an edge fires once.
local function notify(summary, body, critical)
    local urgency = critical and "critical" or "normal"
    process.run("notify-send", { "-a", "Battery", "-u", urgency, "-t", "5000", "-e", summary, body }, function() end,
        function() end)
end

-- Mains state is `oblisk.power` (UPower manager `OnBattery`), not the battery capability.
oblisk.power:on_change(function(p, previous)
    if previous == nil or p.on_battery == nil or p.on_battery == previous.on_battery then
        return
    end
    local b = oblisk.battery:get()
    if not (b and b.present) then
        return
    end
    -- `onIsACPoweredChanged`: plug when connected, bolt-through-battery when disconnected.
    osd.show("battery", {
        glyph = p.on_battery and icons.battery_levels[2] or icons.battery_ac,
        text = p.on_battery and "charger disconnected" or "charger connected",
    })
    -- `adjustBrightness`: the mirror's two levels, not dimming. Keyboard backlight is not mirrored;
    -- there is no capability for it.
    oblisk.brightness:invoke("set", p.on_battery and 10 or 100)
end)

oblisk.battery:on_change(function(b, previous)
    if previous == nil or not b.present then
        return
    end
    -- OSDService.qml's two charge events. Both imply mains, so no `isACPowered` guard.
    -- Only a crossing out of `Charging` is the limit. `PendingCharge` also arrives from
    -- `Discharging` for a few seconds at every plug-in, while the asus driver still reads
    -- `Not charging`, and announcing that said "charge limit reached" at 37% against a limit of 70.
    -- Charging up to a real limit passes through `Charging`, so no true announcement is lost.
    if b.state == "PendingCharge" and previous.state == "Charging" then
        osd.show("battery", { glyph = icons.battery_ac, text = "charge limit reached" })
    elseif previous.state == "Charging" and b.state ~= "Charging" and (b.state == "FullyCharged" or b.percent >= 100) then
        osd.show("battery", { glyph = icons.battery_ac, text = "fully charged" })
    end
    -- Three draining thresholds, each on a downward crossing. Plugging in and unplugging at 15%
    -- crosses `low` again and reports it again, matching the mirror.
    if util.battery_at_most(b, thresholds.low) and not util.battery_at_most(previous, thresholds.low) then
        notify("Low Battery", "Plug in soon!", false)
    end
    if util.battery_at_most(b, thresholds.critical) and not util.battery_at_most(previous, thresholds.critical) then
        notify("Critical Battery", string.format("Automatic suspend at %d%%!", thresholds.suspend), true)
    end
    if util.battery_at_most(b, thresholds.suspend) and not util.battery_at_most(previous, thresholds.suspend) then
        process.run("systemctl", { "suspend" }, function() end, function() end)
    end
end)
