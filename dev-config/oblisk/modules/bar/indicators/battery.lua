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
    -- `activeColor`, not green. `BatteryIndicator.qml` reads
    -- `critical : warning : Theme.activeColor`, so a healthy battery is the same accent every other
    -- "this is fine and on" thing on the bar wears; green is a fourth state the mirror does not
    -- have. Side by side with the QML bar the green pill was the loudest difference between them.
    return theme.ACCENT
end

-- `textColor: Theme.textContrast(percentage > 0.6 ? batteryColor : bgColor)`. The readout sits over
-- two grounds -- the fill on its left, the pill on its right -- and 60% is where the text's centre
-- crosses from one to the other, so that is which ground it contrasts against.
--
-- This had been one colour against a fill tinted to 38%, on the argument that a solid `#a6e3a1` at
-- 89% was the bar's brightest object. That was true of green. The mirror's fill is opaque and
-- accent, which is the same weight as every other lit control here, so the tint and the single
-- colour both go with it.
local READOUT = oblisk.battery:map(function(b)
    local over_fill = b ~= nil and b.present and (b.percent or 0) > 60
    return theme.text_contrast(over_fill and battery_color(b) or theme.GLASS_CONTROL)
end)

-- `onIsPluggedInChanged: if (isPluggedIn) plugFlash.restart()`. `pulse` says a change just
-- happened; the `computed` beside it keeps only the rising edge, so unplugging does not flash.
-- The window has to outlast what it gates: the flash is two cycles of hold-dark, jump-lit,
-- hold-lit, so four `animation_fast_ms` in all.
local plugged = oblisk.battery:map(function(b)
    return b ~= nil and b.present and not util.battery_is_draining(b.state)
end)
local plug_flash = computed({ pulse(plugged, theme.animation_fast_ms * 4), plugged }, function(fired, on)
    return fired and on
end)

local fill = rect {
    width = oblisk.battery:map(function(b)
        if b == nil or not b.present then
            return "0%"
        end
        return string.format("%d%%", math.floor(math.max(0, math.min(100, b.percent or 0)) + 0.5))
    end),
    height = "Fill",
    background = oblisk.battery:map(battery_color),
    -- `BatteryIndicator.qml`: the level slides and the threshold colour fades (ADR-0145), and the
    -- fill blinks twice when the cable goes in. The entry's presence is what runs the sequence
    -- (ADR-0152), so the whole table is bound rather than a `running` flag inside it. `PropertyAction`
    -- is a segment of no duration and `PauseAnimation` a segment between two equal values.
    animate = plug_flash:map(function(flashing)
        local eases = {
            width = theme.animation_ms,
            background = { duration = theme.animation_ms, easing = "OutCubic" },
        }
        if flashing then
            eases.opacity = {
                duration = theme.animation_fast_ms,
                loops = 2,
                keyframes = { 0, 0, { value = 1, duration = 0 }, 1 },
            }
        end
        return eases
    end),
}

local readout = row {
    width = "Fill",
    height = "Fill",
    align_h = "Center",
    align_v = "Center",
    spacing = theme.spacing.xs,
    children = {
        -- `cell`, not `glyph`: both lines here are `OText`, which is `Theme.fontFamily`, and the
        -- pill is a bar control rather than a panel row. `components/glyph.lua` would force
        -- `iconFontFamily` and put this one glyph in a different face from the circles beside it.
        -- Both `OText`s are `bold: true`. On an accent fill at full opacity the weight is what
        -- keeps the dark ink readable, which is the same reason the mirror sets it.
        cell(oblisk.battery:map(function(b)
            return { { text = util.battery_glyph(b), bold = true } }
        end), READOUT, theme.icon.md, { align_v = "Center" }),
        cell(util.label(oblisk.battery, function(b)
            if not b.present then
                return "ac"
            end
            return string.format("%d%%", b.percent)
        end):map(function(shown)
            return { { text = shown, bold = true } }
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
