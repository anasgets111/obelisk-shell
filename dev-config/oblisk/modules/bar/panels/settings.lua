local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local ui_state = require("lib.ui_state")
local sysinfo_module = require("modules.bar.indicators.system_info")
local panel_card = require("components.panel_card")
local panel_header = require("components.panel_header")
local section_header = require("components.section_header")

-- A real `xdg_toplevel`, opened from `power_menu.lua`'s settings row and, since `panel_header`
-- joined this file, closable from inside itself too. The compositor places and sizes this, not the
-- config: § 6.2 gives a `window` no `monitor`, no `anchor` and no size, so `niri msg windows` is
-- where you check that the title and app_id arrived.
--
-- What is left here is the readouts with no indicator of their own. The bluetooth section moved to
-- `panels/bluetooth_panel.lua` when that panel gained an indicator to open it, which is the split
-- Quickshell draws too: a control belongs behind the thing it controls.
return window {
    id = "settings",
    title = "Oblisk settings",
    app_id = "oblisk.settings",
    min_size = { width = 320, height = 240 },
    max_size = { width = 1280, height = 800 },
    visible = ui_state.settings_open,
    -- `panel_card`'s own defaults are the popup shape (10px radius): this is an opaque toplevel
    -- with no edge to round against, so `radius = 0` overrides it.
    child = panel_card({
        panel_header {
            title = "oblisk settings",
            on_close = function()
                ui_state.settings_open:set(false)
            end,
        },
        section_header("system"),
        cell(util.label(oblisk.system, function(s)
            return "up since " .. os.date("%H:%M:%S", s.time)
        end), theme.DIM, theme.font.xs),
        cell(util.label(oblisk.audio, function(a)
            return string.format("%d playback stream(s)", util.count(a.apps))
        end), theme.DIM, theme.font.xs),
        cell(util.label(oblisk.screens, function(s)
            return string.format("%d output(s)", #s)
        end), theme.DIM, theme.font.xs),
        sysinfo_module,
    }, {
        width = "Fill",
        height = "Fill",
        padding = {
            top = theme.spacing.lg,
            right = theme.spacing.lg,
            bottom = theme.spacing.lg,
            left = theme.spacing.lg,
        },
        spacing = theme.spacing.sm,
        radius = 0,
    }),
}
