local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local ui_state = require("lib.ui_state")
local sysinfo_module = require("modules.bar.indicators.system_info")
local panel_card = require("components.panel_card")
local panel_header = require("components.panel_header")
local section_header = require("components.section_header")

-- A real `xdg_toplevel`, opened by `power_menu.lua` and closable through its `panel_header`. § 6
-- gives a `window` no monitor, anchor, or size; inspect placement with `niri msg windows`.
--
-- This holds readouts without indicators. Bluetooth moved to `panels/bluetooth_panel.lua` when it
-- gained its own opener, matching Quickshell's control-behind-what-it-controls split.
return window {
    id = "settings",
    title = "Oblisk settings",
    app_id = "oblisk.settings",
    min_size = { width = 320, height = 240 },
    max_size = { width = 1280, height = 800 },
    visible = ui_state.settings_open,
    -- `panel_card` defaults to a 10px popup radius; this opaque toplevel has no edge to round
    -- against, so `radius = 0`.
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
