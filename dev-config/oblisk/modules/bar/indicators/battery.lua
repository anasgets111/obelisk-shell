-- Mirrors BatteryIndicator.qml: a pill whose ground fills left to right with the charge, a glyph
-- that steps through five levels, and the percentage. The one module on this bar that is a pill
-- rather than a circle, because it is the one carrying a number.
--
-- The fill is a `rect` sized as a percentage string inside a stacking parent, which is
-- `components/meter.lua`'s trick spent on a whole control instead of a 6px bar: a `rect` has no main
-- axis, so its children stack at its origin and `Fill` means its whole box. That puts the fill under
-- the text with no z-order property and no overlay node.
--
-- The pill carries `clip = "Rounded"`, so the fill is a plain square-cornered rect and the pill's
-- own arc cuts it. It used to carry the pill's radius instead and draw as a lozenge inside the left
-- end at low charge, because the engine only clipped to rectangles.
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
    -- Only a battery that is actually running down gets a warning colour. Red at 14% while the
    -- charger is in says the wrong thing, and it is the reference service's own rule:
    -- `isLowAndNotCharging` gates its threshold on `isOnBattery` for exactly this.
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
    -- Taking current gets the bolt. Sitting on mains at a charge limit, or full, gets the plug:
    -- the cable is in and the level is not moving, which is a different thing to show and used to
    -- be indistinguishable from running on battery.
    if b.state == "Charging" then
        return icons.battery_pending
    end
    if b.state == "PendingCharge" or b.state == "FullyCharged" then
        return icons.battery_ac
    end
    -- Five buckets over 0..100, which is `icons[min(floor(fraction * 5), 4)]` with Lua's 1-based
    -- indexing folded in: 100% lands in bucket 5 rather than falling off the end.
    local bucket = math.floor((b.percent or 0) / 20) + 1
    return icons.battery_levels[math.max(1, math.min(5, bucket))]
end

-- One colour for the readout, because the fill is now translucent and never gets light enough to
-- need black text over it. This was two, switched at the 60% mark, and the switch was the whole
-- reason the fill had to be opaque: a solid #a6e3a1 bar at 89% charge was the brightest object on
-- the bar and it existed to make a contrast threshold meaningful. Tint the ground instead and the
-- threshold, and the second colour, both go away.
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
