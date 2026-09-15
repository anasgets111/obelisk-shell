-- Mirrors BatteryIndicator.qml: a left-to-right fill, five-level glyph, and percentage. The only
-- pill on this bar carries a number.
--
-- The percentage-sized `rect` applies `components/meter.lua`'s stacking trick to the whole control.
-- It is not a 6px bar. A `rect` has no main axis, so children stack at its origin.
--
-- `clip = "Rounded"` on the pill cuts the square-cornered fill to its arc. Putting the radius on
-- the fill drew a lozenge inside the pill's left end at low charge because the engine clipped only
-- to rectangles.
local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local tooltip = require("components.tooltip")

local SLOT = "battery"

-- Only the draining check changes colour. The glyph shows cable state; a second signal made a
-- plugged-in battery at 90% use the same green.
local function battery_color(b)
    if b == nil then
        return theme.DIM
    end
    -- Warn only while draining. Red at 14% on the charger is wrong; the reference service's
    -- `isLowAndNotCharging` also gates its threshold on `isOnBattery`.
    if util.battery_at_most(b, util.battery_thresholds.critical) then
        return theme.RED
    end
    if util.battery_at_most(b, util.battery_thresholds.low) then
        return theme.PEACH
    end
    -- `activeColor`, not green. `BatteryIndicator.qml` maps `critical : warning :
    -- Theme.activeColor`.
    -- Green would add a fourth state absent from the mirror.
    return theme.ACCENT
end

-- Two copies of the readout, one per ground: the fill's box clips its copy, so the contrast colour
-- flips at the fill edge, mid-glyph, instead of the mirror's single colour switching at 60%.
local ON_PILL = theme.text_contrast(theme.GLASS_CONTROL)
local ON_FILL = obelisk.battery:map(function(b)
    return theme.text_contrast(battery_color(b))
end)

-- `onIsPluggedInChanged: if (isPluggedIn) plugFlash.restart()`. `pulse` marks a change and
-- `computed`
-- keeps only the rising edge, so unplugging does not flash. The two-cycle flash lasts four
-- `animation_fast_ms` in all.
local plugged = obelisk.battery:map(function(b)
    return b ~= nil and b.present and not util.battery_is_draining(b.state)
end)
local plug_flash = computed({ pulse(plugged, theme.animation_fast_ms * 4), plugged }, function(fired, on)
    return fired and on
end)

-- Use `cell`, not `glyph`: both lines are `OText`/`Theme.fontFamily`; `components/glyph.lua` would
-- force `iconFontFamily` and mismatch the circles beside it. Both `OText`s are `bold: true`, which
-- keeps dark ink readable over the opaque accent fill, as in the mirror.
local GLYPH = obelisk.battery:map(function(b)
    return { { text = util.battery_glyph(b), bold = true } }
end)
local PERCENT = util.label(obelisk.battery, function(b)
    return string.format("%d%%", b.percent)
end):map(function(shown)
    return { { text = shown, bold = true } }
end)

-- `width` is the pill's, so the copy inside the fill lines up with the one under it.
local function readout(color, width)
    return row {
        width = width,
        height = "Fill",
        align_h = "Center",
        align_v = "Center",
        spacing = theme.spacing.xs,
        children = {
            cell(GLYPH, color, theme.icon.md, { align_v = "Center" }),
            cell(PERCENT, color, theme.font.sm, { align_v = "Center" }),
        },
    }
end

local fill = rect {
    width = obelisk.battery:map(function(b)
        if b == nil then
            return "0%"
        end
        return string.format("%d%%", math.floor(math.max(0, math.min(100, b.percent or 0)) + 0.5))
    end),
    height = "Fill",
    background = obelisk.battery:map(battery_color),
    -- `BatteryIndicator.qml`: the level slides and threshold colour fades (ADR-0145), while the
    -- fill blinks twice when the cable goes in. The entry's presence runs the sequence (ADR-0152),
    -- so the table is bound rather than using a `running` flag. `PropertyAction` has no duration
    -- and `PauseAnimation` sits between equal values.
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
    children = { readout(ON_FILL, theme.battery_pill_width) },
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
    -- A desktop's UPower `DisplayDevice` answers `present = false`, which drew a dim AC glyph and
    -- the word "ac": a percentage pill reporting that there is no percentage. Hidden nodes take no
    -- width and no spacing gap, so the zone closes up, and `shown_when` keeps it down until the
    -- first snapshot rather than flashing an empty pill.
    visible = util.shown_when(obelisk.battery, function(b)
        return b.present
    end),
    -- Pill copy first: the fill paints over it, and shows it again while the plug flash fades.
    children = { readout(ON_PILL, "Fill"), fill },
}

local battery_tooltip = tooltip({
    id = "battery_tooltip",
    slot = SLOT,
    children = {
        cell(util.label(obelisk.battery, function(b)
            return string.format("%d%% %s%s", b.percent, util.battery_phrase(b.state), util.battery_eta(b))
        end), theme.FG, theme.font.sm),
        cell(util.label(obelisk.power, function(p)
            local parts = {}
            if p.on_battery ~= nil then
                parts[#parts + 1] = p.on_battery and "On battery" or "On AC"
            end
            if p.energy_rate ~= nil then
                parts[#parts + 1] = string.format("%.1f W", p.energy_rate)
            end
            if p.active_profile ~= nil then
                parts[#parts + 1] = p.active_profile
            end
            return #parts > 0 and table.concat(parts, ", ") or "No power detail"
        end), theme.DIM, theme.font.xs),
    },
})

return { indicator = battery_module, tooltip = battery_tooltip }
