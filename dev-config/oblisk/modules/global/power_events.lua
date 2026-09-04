-- What the reference shell does *about* the battery, as opposed to drawing it: the OSD lines
-- OSDService.qml raises on a charger event, BatteryService.qml's two `notify-send`s, and
-- PowerManagementService.qml's automatic suspend and brightness step. No surface here; this file is
-- four `on_change` handlers (ADR-0115) and `shell.lua` requires it for its side effects.
--
-- Every handler compares the pushed payload against the one it replaced and acts on the crossing,
-- which is what the mirror's `onIsLowAndNotChargingChanged`-style signals are: an edge, fired once.
-- `previous == nil` is the first push after start and is never an edge -- the mirror's OSD holds
-- an `initialized` flag for the same reason.
local icons = require("config.icons")
local util = require("lib.util")
local ui_state = require("lib.ui_state")

local thresholds = util.battery_thresholds

-- `notify-send` with the mirror's own arguments (`_sendNotification`), through our own daemon, so
-- it lands as a card like any other. No 15-second dedupe: an edge fires once by construction.
local function notify(summary, body, critical)
    local urgency = critical and "critical" or "normal"
    process.run("notify-send", { "-a", "Battery", "-u", urgency, "-t", "5000", "-e", summary, body }, function() end, function() end)
end

-- The mains question lives on `oblisk.power` (UPower's manager `OnBattery`), not on the battery.
oblisk.power:on_change(function(p, previous)
    if previous == nil or p.on_battery == nil or p.on_battery == previous.on_battery then
        return
    end
    local b = oblisk.battery:get()
    if not (b and b.present) then
        return
    end
    -- `onIsACPoweredChanged`: the plug for connected, the bolt-through-battery for disconnected.
    ui_state.arm_osd("battery", {
        glyph = p.on_battery and icons.battery_levels[2] or icons.battery_ac,
        text = p.on_battery and "charger disconnected" or "charger connected",
    })
    -- `adjustBrightness`: the mirror's own two levels, not a dimming scheme. Keyboard backlight is
    -- not mirrored; there is no capability for it.
    oblisk.brightness:invoke("set", p.on_battery and 10 or 100)
end)

oblisk.battery:on_change(function(b, previous)
    if previous == nil or not b.present then
        return
    end
    -- OSDService.qml's two charge events. Both states imply mains, so no `isACPowered` guard.
    if b.state == "PendingCharge" and previous.state ~= "PendingCharge" then
        ui_state.arm_osd("battery", { glyph = icons.battery_ac, text = "charge limit reached" })
    elseif previous.state == "Charging" and b.state ~= "Charging" and (b.state == "FullyCharged" or b.percent >= 100) then
        ui_state.arm_osd("battery", { glyph = icons.battery_ac, text = "fully charged" })
    end
    -- The three draining thresholds, each on its own downward crossing. Plugging in and unplugging
    -- at 15% crosses `low` again, and says so again, which is what the mirror does too.
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
