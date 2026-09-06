-- Mirrors BatteryIndicator.qml: a pill filled left to right, a five-level glyph, and a percentage.
-- It is the only pill on this bar because it carries a number.
--
-- The fill is a percentage-sized `rect` in a stacking parent, using `components/meter.lua`'s trick
-- spent on a whole control instead of a 6px bar. A `rect` has no main axis, so children stack at
-- its origin and `Fill` puts
-- the fill under the text without z-order or an overlay node.
--
-- `clip = "Rounded"` on the pill cuts the square-cornered fill to its arc. Putting the radius on
-- the fill drew a lozenge inside the pill's left end at low charge because the engine clipped only
-- to rectangles.
local theme = require("config.theme")
local icons = require("config.icons")
local util = require("lib.util")
local cell = require("components.cell")
local tooltip = require("components.tooltip")

local SLOT = "battery"

-- No per-state branch beyond the draining check. The glyph below already says what the cable is
-- doing, and a second signal for it here just meant that plugging in and standing at 90% painted
-- the same green.
local function battery_color(b)
    if b == nil or not b.present then
        return theme.DIM
    end
    -- Warn only while running down. Red at 14% on the charger is wrong; the reference service's
    -- `isLowAndNotCharging` gates its threshold on `isOnBattery` too.
    if util.battery_at_most(b, util.battery_thresholds.critical) then
        return theme.RED
    end
    if util.battery_at_most(b, util.battery_thresholds.low) then
        return theme.PEACH
    end
    return theme.GREEN
end

local function battery_glyph(b)
    if b == nil or not b.present then
        return icons.battery_ac
    end
-- `Charging` gets the bolt. Mains at a charge limit and full get the plug: the cable is in and the
-- level is not moving, a state once indistinguishable from running on battery.
    if b.state == "Charging" then
        return icons.battery_pending
    end
    if b.state == "PendingCharge" or b.state == "FullyCharged" then
        return icons.battery_ac
    end
    -- Five buckets over 0..100. Lua's 1-based indexing makes 100% bucket 5, not an out-of-range 6.
    local bucket = math.floor((b.percent or 0) / 20) + 1
    return icons.battery_levels[math.max(1, math.min(5, bucket))]
end

-- One readout colour works with the translucent fill. The old two-colour switch at 60% existed for
-- an opaque fill: solid `#a6e3a1` at 89% was the bar's brightest object. Tinting the ground removes
-- that contrast threshold and the second colour.
local READOUT = theme.text_contrast(theme.GLASS_CONTROL)

local fill = rect {
    width = oblisk.battery:map(function(b)
        if b == nil or not b.present then
            return "0%"
        end
        return string.format("%d%%", math.floor(math.max(0, math.min(100, b.percent or 0)) + 0.5))
    end),
    height = "Fill",
    background = oblisk.battery:map(function(b)
        return theme.with_opacity(battery_color(b), 0.38)
    end),
}

local readout = row {
    width = "Fill",
    height = "Fill",
    align_h = "Center",
    align_v = "Center",
    spacing = theme.spacing.xs,
    children = {
        cell(oblisk.battery:map(battery_glyph), READOUT, theme.icon.md, { align_v = "Center" }),
        cell(util.label(oblisk.battery, function(b)
            if not b.present then
                return "ac"
            end
            return string.format("%d%%", b.percent)
        end), READOUT, theme.font.sm, { align_v = "Center" }),
    },
}

local battery_module = rect {
    width = theme.battery_pill_width,
    height = theme.item_height,
    align_v = "Center",
    radius = theme.item_radius,
    clip = "Rounded",
    background = theme.GLASS_CONTROL,
    border_width = theme.border_width,
    border_color = theme.GLASS_BORDER,
    hover = hover(SLOT),
    children = { fill, readout },
}

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
            return string.format("%d%% %s%s", b.percent, util.battery_phrase(b.state), util.battery_eta(b))
        end), theme.FG, theme.font.sm),
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
        end), theme.DIM, theme.font.xs),
    },
})

return { indicator = battery_module, tooltip = battery_tooltip }
