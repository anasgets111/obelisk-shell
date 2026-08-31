-- Volume and brightness OSD: a corner overlay that flashes the level a click just changed and
-- hides itself after, the shape VolumeOSD.qml/BrightnessOSD.qml both are, minus their animation.
--
-- Armed by a click, not by watching `oblisk.audio`/`oblisk.brightness` push: `lib/ui_state.lua`'s
-- own comment on `arm_osd` has the reason (no signal-change event a config can observe, no
-- `on_hover`, no `on_scroll`). `modules/bar/indicators/volume.lua` and `.../brightness.lua`'s own
-- `on_click` handlers call it; this file only reads the two `state` signals that call leaves
-- behind.
--
-- One `panel`, two stacked rows switched by `visible`, rather than two panels: § 6.1 gives every
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
    spacing = 10,
    padding = { left = 14, right = 14 },
    visible = ui_state.osd_kind:map(function(kind)
        return kind == "volume"
    end),
    children = {
        icon { name = oblisk.audio:map(util.volume_icon_name), size = 20 },
        meter(oblisk.audio, function(a)
            return a.muted and 0 or a.volume * 100
        end, theme.MAUVE, 120),
        text {
            content = util.label(oblisk.audio, function(a)
                return a.muted and "muted" or string.format("%d%%", math.floor(a.volume * 100 + 0.5))
            end),
            foreground = theme.FG,
            font_size = 13,
        },
    },
}

local brightness_row = row {
    width = "Fill",
    height = "Fill",
    align_v = "Center",
    spacing = 10,
    padding = { left = 14, right = 14 },
    visible = ui_state.osd_kind:map(function(kind)
        return kind == "brightness"
    end),
    children = {
        text { content = "sun", foreground = theme.FG, font_size = 13 },
        meter(oblisk.brightness, function(b)
            return b.percent
        end, theme.YELLOW, 120),
        text {
            content = util.label(oblisk.brightness, function(b)
                return string.format("%d%%", b.percent)
            end),
            foreground = theme.FG,
            font_size = 13,
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
    margin = { bottom = 56 },
    width = 260,
    height = 44,
    visible = ui_state.osd_visible,
    child = column {
        width = "Fill",
        height = "Fill",
        background = "#181825ee",
        radius = 10,
        border_width = 1,
        border_color = theme.SURFACE,
        children = { volume_row, brightness_row },
    },
}
