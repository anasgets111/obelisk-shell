local theme = require("config.theme")
local util = require("lib.util")
local cell = require("components.cell")
local ui_state = require("lib.ui_state")
local panel_card = require("components.panel_card")
local panel_header = require("components.panel_header")
local section_header = require("components.section_header")

-- A real `xdg_toplevel`, opened by `power_menu.lua` and closable through its `panel_header`. A
-- `window` gets no monitor, anchor, or size; inspect placement with `niri msg windows`.
--
-- This holds readouts without indicators. Bluetooth moved to `panels/bluetooth_panel.lua` when it
-- gained its own opener, matching Quickshell's control-behind-what-it-controls split. The system
-- readout left the same way (ADR-0173): `SystemInfoWidget` belongs at the top of the notifications
-- panel, where the mirror instantiates it, and a second copy here was the same numbers twice.
--
-- What is left is thin on purpose. This file is the config's only `window {}`, so it is also the
-- only exercise of the toplevel -- surviving a compositor that gives it no monitor, anchor or
-- size. Deleting it for being thin would delete that.
return window {
    id = "settings",
    title = "Obelisk settings",
    app_id = "obelisk.settings",
    min_size = { width = 320, height = 240 },
    max_size = { width = 1280, height = 800 },
    visible = ui_state.settings_open,
    -- `panel_card` defaults to `theme.radius.md`; this opaque toplevel has no edge to round.
    -- Set `radius = 0`.
    child = panel_card({
        panel_header {
            title = "obelisk settings",
            on_close = function()
                ui_state.settings_open:set(false)
            end,
        },
        section_header("system"),
        cell(util.label(obelisk.system, function(s)
            return "up since " .. os.date("%H:%M:%S", s.time)
        end), theme.DIM, theme.font.xs),
        cell(util.label(obelisk.audio, function(a)
            return string.format("%d playback stream(s)", #(a.apps or {}))
        end), theme.DIM, theme.font.xs),
        cell(util.label(obelisk.screens, function(s)
            return string.format("%d output(s)", #s)
        end), theme.DIM, theme.font.xs),
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
