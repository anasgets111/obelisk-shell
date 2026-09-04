-- Volume, brightness and battery OSD: a corner overlay that flashes what just changed and hides
-- itself after, the shape VolumeOSD.qml/BrightnessOSD.qml and OSDService.qml's battery events are,
-- minus their animation.
--
-- Armed by `lib/ui_state.lua`'s `arm_osd`, never by this file watching a capability: the volume
-- and brightness rows are armed by the bar click that changed the level, the battery row by
-- `modules/global/power_events.lua`'s `on_change` handlers (ADR-0115). This file only reads the
-- `state` signals that call leaves behind.
--
-- One `panel`, three stacked rows switched by `visible`, rather than three panels: § 6.1 gives every
-- surface its own compositor identity, and a volume change while the brightness OSD is still fading
-- out (if this engine ever grows a fade) would otherwise be two overlapping corner surfaces
-- fighting over the same screen position instead of one replacing the other.
local theme = require("config.theme")
local util = require("lib.util")
local ui_state = require("lib.ui_state")
local meter = require("components.meter")

local volume_row = row {
    width = "Fill",
    height = "Fill",
    align_v = "Center",
    spacing = theme.spacing.md,
    padding = { left = theme.spacing.lg, right = theme.spacing.lg },
    visible = ui_state.osd_kind:map(function(kind)
        return kind == "volume"
    end),
    children = {
        icon { name = oblisk.audio:map(util.volume_icon_name), size = theme.icon.lg },
        meter(oblisk.audio, function(a)
            return a.muted and 0 or a.volume * 100
        end, theme.MAUVE, "Fill"),
        text {
            content = util.label(oblisk.audio, function(a)
                return a.muted and "muted" or string.format("%d%%", math.floor(a.volume * 100 + 0.5))
            end),
            foreground = theme.FG,
            font_size = theme.font.sm,
            width = theme.s(44, 34),
            text_align = "End",
        },
    },
}

local brightness_row = row {
    width = "Fill",
    height = "Fill",
    align_v = "Center",
    spacing = theme.spacing.md,
    padding = { left = theme.spacing.lg, right = theme.spacing.lg },
    visible = ui_state.osd_kind:map(function(kind)
        return kind == "brightness"
    end),
    children = {
        icon { name = "display-brightness", size = theme.icon.md },
        meter(oblisk.brightness, function(b)
            return b.percent
        end, theme.YELLOW, "Fill"),
        text {
            content = util.label(oblisk.brightness, function(b)
                return string.format("%d%%", b.percent)
            end),
            foreground = theme.FG,
            font_size = theme.font.sm,
            width = theme.s(44, 34),
            text_align = "End",
        },
    },
}

-- No meter: a charger event is a fact, not a level. The glyph and the line both come from
-- `osd_message`, so this row knows nothing about batteries.
local battery_row = row {
    width = "Fill",
    height = "Fill",
    align_v = "Center",
    spacing = theme.spacing.md,
    padding = { left = theme.spacing.lg, right = theme.spacing.lg },
    visible = ui_state.osd_kind:map(function(kind)
        return kind == "battery"
    end),
    children = {
        text {
            content = ui_state.osd_message:map(function(m)
                return m.glyph
            end),
            foreground = theme.FG,
            font_size = theme.icon.lg,
        },
        text {
            content = ui_state.osd_message:map(function(m)
                return m.text
            end),
            foreground = theme.FG,
            font_size = theme.font.sm,
        },
    },
}

return panel {
    id = "osd",
    layer = "Overlay",
    -- No `left`/`right`: § 6.1's anchor booleans pass straight through to `zwlr_layer_surface_v1`
    -- (`renderer/src/wayland/layer.rs`'s `anchor_for` is a bare bitflag map, nothing more), and the
    -- protocol centers an axis with neither of its edges anchored. Explicit `width`/`height` are
    -- required because `bottom` alone doesn't anchor both edges of either axis.
    anchor = { bottom = true },
    margin = { bottom = theme.s(56, 40) },
    width = theme.osd_width,
    height = theme.osd_height,
    visible = ui_state.osd_visible,
    child = column {
        width = "Fill",
        height = "Fill",
        background = theme.GLASS,
        radius = theme.radius.md,
        border_width = theme.border_width,
        border_color = theme.BORDER,
        children = { volume_row, brightness_row, battery_row },
    },
}
